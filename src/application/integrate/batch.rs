//! Verify a selected prefix, then publish its proven trees using the
//! ordinary per-run approval, integration token, and squash path.

use super::*;
use crate::domain::landing_batch::{
    Prepared, Progress, Segment, Stage, Stages, Verification, Verified, verification_commands,
};

pub(super) enum Gate {
    Ordinary,
    Proof {
        branch: LandingBranch,
        tree: String,
        rejection: Option<Verdict>,
    },
    Stop(Verdict),
    Abort(anyhow::Error),
}

/// The selected followers hold ordinary run leases. Only the member
/// being checked or published has the queue's integrating status.
pub(crate) struct Batch {
    pub followers: Vec<TaskRun>,
    pub directory: PathBuf,
    pub turn: super::super::heavy_turn::Turn,
    pub lock: super::super::heavy_turn::Turn,
    pub over_handoff: bool,
}

pub(crate) fn lock(
    files: &dyn RunFiles,
    directory: &Path,
) -> Result<Option<super::super::heavy_turn::Turn>> {
    files.create_dir_all(directory)?;
    Ok(files.try_lock(&directory.join("turn"))?)
}

impl Integrator {
    pub(crate) fn land_batch<'ctx>(
        &self,
        ctx: &mut Integration<'ctx>,
        first: &IntegrationRequest,
        batch: Batch,
        remote: &'ctx dyn MainRemote,
    ) -> Result<IntegrationOutcome> {
        let Batch {
            followers,
            directory,
            turn: _turn,
            lock: _lock,
            over_handoff,
        } = batch;
        let mut members = vec![first.run.clone()];
        members.extend(followers);
        let mut progress = Progress {
            id: ctx.ids.uuid(),
            runs: members.iter().map(|run| run.id().clone()).collect(),
            stage: None,
            removed: Vec::new(),
            finished: false,
        };
        // The heavy turn is already held over the batch. In particular a
        // writer waiting for it must not stop one of our later stages.
        ctx.heavy_turn = None;
        let scratch = directory.join("verification");
        let mut landing = BatchLanding {
            integrator: self,
            ctx,
            first,
            remote,
            members,
            progress: &mut progress,
            scratch: &scratch,
            over_handoff,
            active: Some(first.clone()),
            first_outcome: None,
        };
        let result = landing.run();
        landing.close(result)
    }
}

struct BatchLanding<'a, 'ctx> {
    integrator: &'a Integrator,
    ctx: &'a mut Integration<'ctx>,
    first: &'a IntegrationRequest,
    remote: &'ctx dyn MainRemote,
    members: Vec<TaskRun>,
    progress: &'a mut Progress,
    scratch: &'a Path,
    over_handoff: bool,
    active: Option<IntegrationRequest>,
    first_outcome: Option<IntegrationOutcome>,
}

impl BatchLanding<'_, '_> {
    fn record(&self) -> Result<()> {
        self.ctx.queue.record_runtime_event(
            self.first.run.id(),
            EventKind::LandingBatchProgress,
            serde_json::to_value(&self.progress)?,
        )
    }

    fn close(mut self, mut result: Result<()>) -> Result<IntegrationOutcome> {
        // A failure before the current member's normal landing path
        // still needs to relinquish its integration slot.
        if let Some(active) = self.active.take() {
            let gate = match result {
                Ok(()) => Gate::Stop(Verdict::HandedOver {
                    reason: "the landing batch must be assembled again".to_owned(),
                    detail: json!({"batch": self.progress.id}),
                }),
                Err(error) => Gate::Abort(error),
            };
            result = self.apply(&active, gate).map(|outcome| {
                if active.run.id() == self.first.run.id() {
                    self.first_outcome = Some(outcome);
                }
            });
        }
        for member in self.members.iter().skip(1) {
            let release = (|| {
                if self.ctx.queue.holds_lease(member.id(), &self.first.token)?
                    && self.ctx.queue.run(member.id())?.status() != RunStatus::Integrating
                {
                    self.ctx
                        .queue
                        .release_lease(member.id(), &self.first.token)?;
                }
                Ok(())
            })();
            result = result.and(release);
        }
        self.progress.stage = None;
        self.progress.finished = true;
        let recorded = self.record();
        let removed = self.ctx.repository.remove_scratch(self.scratch);
        result.and(recorded).and(removed)?;
        Ok(self
            .first_outcome
            .unwrap_or(IntegrationOutcome::NoRunAwaiting))
    }

    /// Take the member's slot, or end the batch when handoff refuses a
    /// new landing. Closing the batch then returns its unstarted leases.
    fn begin(&mut self, index: usize, base: &CommitSha) -> Result<bool> {
        let member = &self.members[index];
        if let Some(request) = &self.active {
            ensure!(
                request.run.id() == member.id(),
                "another member holds the batch's integration slot"
            );
            return Ok(true);
        }
        let queue = &mut *self.ctx.queue;
        ensure!(
            queue.holds_lease(member.id(), &self.first.token)?,
            "batch lost the lease of {}",
            member.id()
        );
        let begun = if self.over_handoff {
            queue.begin_integration_over_handoff(member.id(), &self.first.token, base)
        } else {
            queue.begin_integration(member.id(), &self.first.token, base)
        };
        let run = match begun {
            Ok(run) => run,
            Err(error) if error.is::<crate::application::HandoffAsked>() => {
                info!(run_id = %member.id(), "run {} stays queued: its supervisor was asked to hand off", member.id());
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        self.active = Some(IntegrationRequest {
            requester: self.first.requester.clone(),
            run,
            previous: member.status(),
            main: base.clone(),
            token: self.first.token.clone(),
            held_turn: None,
        });
        Ok(true)
    }

    fn apply(&mut self, request: &IntegrationRequest, gate: Gate) -> Result<IntegrationOutcome> {
        let grant = self.integrator.push_grant()?;
        let pushes =
            RunHistory::from_events(&self.ctx.queue.run_events(request.run.id())?).landing_pushes();
        self.ctx.remote = pushes.then_some(self.remote);
        let previous = self.ctx.queue.act_as(self.integrator.actor.clone());
        let requested_by = self.ctx.queue.request_as(Some(&request.requester));
        let result = self
            .integrator
            .check(&mut *self.ctx.queue, request, self.ctx.build)
            .and_then(|()| land_integrating(self.ctx, &grant, request, gate));
        self.ctx.queue.restore_request(requested_by);
        if let Some(previous) = previous {
            self.ctx.queue.act_as(previous);
        }
        result
    }

    fn finish(&mut self, gate: Gate) -> Result<Applied> {
        let request = self
            .active
            .take()
            .context("batch has no member in its slot")?;
        let outcome = self.apply(&request, gate)?;
        let applied = match &outcome {
            IntegrationOutcome::Integrated { run, .. } => Applied::Landed(
                run.result_commit()
                    .context("landed run has no commit")?
                    .clone(),
            ),
            IntegrationOutcome::HandedOver { .. } | IntegrationOutcome::Held { .. } => {
                Applied::Stopped
            }
            _ => {
                self.progress.removed.push(request.run.id().clone());
                self.record()?;
                Applied::Removed
            }
        };
        if request.run.id() == self.first.run.id() {
            self.first_outcome = Some(outcome);
            self.ctx.steps = None;
        }
        Ok(applied)
    }

    fn run(&mut self) -> Result<()> {
        self.record()?;
        let mut base = self.first.main.clone();
        let branch = self.ctx.repository.landing_branch()?;
        let mut pending = Stages::new(self.members.len());
        let mut accepted = Vec::new();
        while let Some(segment) = pending.take() {
            if self.ctx.repository.landing_branch()? != branch
                || self.ctx.repository.main_head()? != base
            {
                break;
            }
            if !self.begin(segment.range.start, &base)? {
                break;
            }
            let Stack {
                head,
                trees,
                stopped,
            } = self.stack(&base, &segment, &branch)?;
            let segment = match pending.prepared(segment, stopped) {
                Prepared::Verify(segment) => segment,
                Prepared::ResolveFirst(gate) => {
                    if self.ctx.repository.landing_branch()? != branch
                        || self.ctx.repository.main_head()? != base
                    {
                        break;
                    }
                    if !matches!(self.finish(gate)?, Applied::Removed) {
                        break;
                    }
                    continue;
                }
            };
            self.progress.stage = Some(Stage {
                runs: segment
                    .range
                    .clone()
                    .map(|i| self.members[i].id().clone())
                    .collect(),
                depth: segment.depth,
            });
            self.record()?;
            let checked = self.verify(
                &base,
                &head,
                accepted.iter().copied().chain(segment.range.clone()),
            );
            self.progress.stage = None;
            self.record()?;
            if self.ctx.repository.landing_branch()? != branch
                || self.ctx.repository.main_head()? != base
            {
                break;
            }
            let verification = match checked? {
                None => Verification::Passed,
                Some(verdict @ Verdict::Deferred { .. }) => Verification::RunFailed(verdict),
                Some(verdict) => Verification::Interrupted(verdict),
            };
            match pending.verified(&segment, verification) {
                Verified::Split => {}
                Verified::SendBack(verdict) => {
                    let gate = Gate::Proof {
                        branch: branch.clone(),
                        tree: trees[0].clone(),
                        rejection: Some(verdict),
                    };
                    if !matches!(self.finish(gate)?, Applied::Removed) {
                        break;
                    }
                }
                Verified::Stop(verdict) => {
                    self.finish(Gate::Stop(verdict))?;
                    break;
                }
                Verified::Publish => {
                    for (index, tree) in segment.range.zip(trees) {
                        if self.ctx.repository.landing_branch()? != branch
                            || self.ctx.repository.main_head()? != base
                        {
                            return Ok(());
                        }
                        if !self.begin(index, &base)? {
                            return Ok(());
                        }
                        let applied = self.finish(Gate::Proof {
                            branch: branch.clone(),
                            tree: tree.clone(),
                            rejection: None,
                        })?;
                        let Applied::Landed(commit) = applied else {
                            // The verified suffix depended on a member
                            // whose own gate did not pass. Reassemble it.
                            return Ok(());
                        };
                        if self.ctx.repository.tree_of(commit.as_str())? != tree {
                            return Ok(());
                        }
                        base = commit;
                        accepted.push(index);
                    }
                }
            }
        }
        Ok(())
    }

    fn stack(
        &mut self,
        base: &CommitSha,
        segment: &Segment,
        branch: &LandingBranch,
    ) -> Result<Stack> {
        let mut head = base.clone();
        let mut trees = Vec::new();
        for index in segment.range.clone() {
            let run = &self.members[index];
            let repository = super::super::run_git::for_run(self.ctx.repository, run)?;
            let events = self.ctx.queue.run_events(run.id())?;
            let source = binding::landing_head(&*repository, run, &events)?;
            let stopped = |head, trees, gate| {
                Ok(Stack {
                    head,
                    trees,
                    stopped: Some((index, gate)),
                })
            };
            let source = CommitSha::parse(source, "batch member head")?;
            let tree = match repository.merged_tree(head.as_str(), source.as_str())? {
                Ok(tree) => tree,
                Err(files) => {
                    let reason = format!(
                        "rebase onto the landing prefix conflicted in {}; resolve it in the worktree, rerun your checks, and rewrite the receipt",
                        files.join(", ")
                    );
                    let verdict = Verdict::Deferred {
                        reason,
                        detail: Reason::new(ReasonCode::RebaseConflict).on(json!({
                            "main": base, "head": source, "conflicts": files, "aborted": true
                        })),
                    };
                    return stopped(head, trees, Gate::Stop(verdict));
                }
            };
            let stacked = repository.commit_tree(
                &tree,
                head.as_str(),
                &["dagq landing verification".to_owned()],
            )?;
            let task = self.ctx.queue.show(run.task_id())?.task;
            let prepared = if repository.is_ancestor(head.as_str(), source.as_str())?
                && repository.tree_of(source.as_str())? == tree
            {
                &source
            } else {
                &stacked
            };
            if !can_verify(
                &*repository,
                self.ctx.files,
                run,
                &task,
                &events,
                (&head, &source, prepared),
            )? {
                return stopped(head, trees, Gate::Ordinary);
            }
            if let Some(programs) = &self.ctx.programs {
                let scratch_repository = self.ctx.repository.guarded()?;
                // The control-side scratch is independent of the run's
                // branch and can be replaced for each stacked prefix.
                scratch_repository.fresh_scratch(self.scratch, prepared.as_str())?;
                if let Some(verdict) = programs::landing_programs(
                    self.ctx.queue,
                    &*repository,
                    self.ctx.verifier,
                    (self.ctx.files, self.ctx.clock),
                    programs,
                    self.ctx.handing_off,
                    &programs::ProgramsAt {
                        run,
                        worktree: self.scratch,
                        run_dir: Path::new(run.run_dir().context("missing run directory")?),
                        onto: &branch.name,
                        main: &head,
                        rebased: prepared,
                    },
                )? {
                    return stopped(head, trees, Gate::Stop(verdict));
                }
            }
            head = stacked;
            trees.push(tree);
        }
        Ok(Stack {
            head,
            trees,
            stopped: None,
        })
    }

    fn verify(
        &mut self,
        base: &CommitSha,
        head: &CommitSha,
        members: impl Iterator<Item = usize>,
    ) -> Result<Option<Verdict>> {
        let mut plans = Vec::new();
        let mut tasks = Vec::new();
        for index in members {
            let task = self.ctx.queue.show(self.members[index].task_id())?.task;
            plans.push(landing_plan(self.ctx.verifier, &task)?);
            tasks.push(task);
        }
        let commands = verification_commands(plans);
        if commands.is_empty() {
            return Ok(None);
        }
        // The replacement checks the shared tree, including changes
        // from tasks whose own verification plan contains no replacement.
        let known = if commands.iter().any(landing_verification::Planned::replaced) {
            tasks
                .iter()
                .map(|task| KnownFailures::read(&*self.ctx.queue, self.ctx.verifier, task))
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        let request = self
            .active
            .as_ref()
            .context("batch verification has no integration slot")?;
        let repository = self.ctx.repository.guarded()?;
        repository.fresh_scratch(self.scratch, head.as_str())?;
        let previous = self.ctx.queue.act_as(self.integrator.actor.clone());
        let requested_by = self.ctx.queue.request_as(Some(&request.requester));
        let ticking = self.ctx.steps.as_ref().map(|steps| {
            super::super::step_report::Ticking::start(steps.shared.clone(), steps.clock.clone())
        });
        let result = self
            .integrator
            .check(&mut *self.ctx.queue, request, self.ctx.build)
            .and_then(|()| {
                verify_landing(
                    self.ctx,
                    &request.run,
                    self.scratch,
                    (base, head, &self.first.main),
                    &commands,
                    &|| KnownFailures::for_batch(&known),
                )
            });
        drop(ticking);
        self.ctx.queue.restore_request(requested_by);
        // Keep the active member's integrator credential until its
        // result is committed, or another verification of it begins.
        if let Some(previous) = previous {
            self.ctx.queue.pause_integration_actor(previous);
        }
        result
    }
}

/// Verification executes code from the staged tree. Check the same
/// sources as an ordinary landing before using a candidate in that tree.
pub(super) fn can_verify(
    repository: &dyn Repository,
    files: &dyn RunFiles,
    run: &TaskRun,
    task: &Task,
    events: &[RunEvent],
    (base, source, stacked): (&CommitSha, &CommitSha, &CommitSha),
) -> Result<bool> {
    let path = Path::new(run.receipt_path().context("missing receipt path")?);
    let Ok(text) = files.read_to_string(path) else {
        return Ok(false);
    };
    let Ok(receipt) = Receipt::parse(&text) else {
        return Ok(false);
    };
    let evidence = required_of(task.required_evidence(), run.actual_provider());
    let names = receipt.names_commit(source.as_str())
        || RunHistory::from_events(events).landing_rewrote(receipt.commit(), source.as_str());
    if !names
        || receipt_ends_landing(&receipt).is_some()
        || receipt.check_requiring(run.id(), &evidence).is_err()
        || landing_gaps(&receipt, &evidence, task).is_some()
    {
        return Ok(false);
    }
    // A collision is relative to this member's prefix. Finish that
    // prefix first, then use ordinary landing if the collision remains:
    // it renumbers the real worktree and asks for approval of the rewrite,
    // or records why the migration needs a session.
    if !matches!(
        plan_renumber(repository, base, stacked)?,
        RenumberPlan::Unchanged
    ) || !out_of_scope(
        task.paths(),
        &repository.changed_paths(base.as_str(), stacked.as_str())?,
    )
    .is_empty()
    {
        return Ok(false);
    }
    binding::permits_stack(
        repository,
        events,
        binding::Rewrites {
            main: base,
            head: source,
            rebased: stacked,
            moved_by_rebase: true,
            renumbered: false,
        },
    )
}

struct Stack {
    head: CommitSha,
    trees: Vec<String>,
    stopped: Option<(usize, Gate)>,
}

enum Applied {
    Landed(CommitSha),
    Removed,
    Stopped,
}
