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
                members,
                stopped,
            } = self.stack(&base, &segment, &branch, &accepted)?;
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
                accepted
                    .iter()
                    .map(|(index, _)| *index)
                    .chain(segment.range.clone()),
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
                        tree: members[0].tree.clone(),
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
                    for (index, StackedMember { tree, source }) in segment.range.zip(members) {
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
                        accepted.push((index, source));
                    }
                }
            }
        }
        Ok(())
    }

    /// Keep the verification stack and its conflict checks independent of
    /// the history shown to programs. Select programs from the landing
    /// branch using the verification diff. Only when one is selected, build
    /// the program prefix through this member, including earlier members
    /// that needed no programs themselves. An unrepresentable program
    /// prefix stops the stage for ordinary landing before any program of
    /// the affected member runs.
    fn stack(
        &mut self,
        base: &CommitSha,
        segment: &Segment,
        branch: &LandingBranch,
        accepted: &[(usize, CommitSha)],
    ) -> Result<Stack> {
        let mut head = base.clone();
        let mut members = Vec::new();
        let mut program_prefix = ProgramPrefix {
            head: base.clone(),
            len: 0,
        };
        for index in segment.range.clone() {
            let run = &self.members[index];
            let repository = super::super::run_git::for_run(self.ctx.repository, run)?;
            let events = self.ctx.queue.run_events(run.id())?;
            let source = binding::landing_head(&*repository, run, &events)?;
            let stopped = |head, members, gate| {
                Ok(Stack {
                    head,
                    members,
                    stopped: Some((index, gate)),
                })
            };
            let source = CommitSha::parse(source, "batch member head")?;
            let (member, stacked) = match verification_member(&*repository, &head, &source)? {
                Ok(member) => member,
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
                    return stopped(head, members, Gate::Stop(verdict));
                }
            };
            let task = self.ctx.queue.show(run.task_id())?.task;
            let prepared = verification_prepared(&*repository, &head, &member, &stacked)?;
            if !can_verify(
                &*repository,
                self.ctx.files,
                run,
                &task,
                &events,
                (&head, &source, prepared),
            )? {
                return stopped(head, members, Gate::Ordinary);
            }
            if let Some(programs) = &self.ctx.programs {
                let selected = programs::select(
                    &*repository,
                    self.ctx.verifier,
                    self.ctx.handing_off,
                    base,
                    (&head, prepared),
                );
                let (from, rebased) = match &selected {
                    Ok(selected) => {
                        let prefix = members.iter().chain(std::iter::once(&member));
                        match program_prefix.prepare(
                            self.ctx.repository,
                            base,
                            accepted,
                            self.members[segment.range.start..=index].iter().zip(prefix),
                            selected,
                            &events,
                        )? {
                            ProgramGate::Ordinary => return stopped(head, members, Gate::Ordinary),
                            ProgramGate::Skip => {
                                head = stacked;
                                members.push(member);
                                continue;
                            }
                            ProgramGate::Run { from, rebased } => {
                                self.ctx
                                    .repository
                                    .guarded()?
                                    .fresh_scratch(self.scratch, rebased.as_str())?;
                                (from, rebased)
                            }
                        }
                    }
                    // A selection failure has no program checkout. Keep the
                    // read failure verdict tied to the original run head.
                    Err(_) => (base.clone(), source),
                };
                if let Some(verdict) = programs::run_selected(
                    self.ctx.queue,
                    &*repository,
                    (self.ctx.files, self.ctx.clock),
                    programs,
                    self.ctx.handing_off,
                    &programs::ProgramsAt {
                        run,
                        worktree: self.scratch,
                        run_dir: Path::new(run.run_dir().context("missing run directory")?),
                        onto: &branch.name,
                        landing: base,
                        from: &from,
                        rebased: &rebased,
                    },
                    &events,
                    selected,
                )? {
                    return stopped(head, members, Gate::Stop(verdict));
                }
            }
            head = stacked;
            members.push(member);
        }
        Ok(Stack {
            head,
            members,
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

/// Build the same verification commit regardless of any program history.
fn verification_member(
    repository: &dyn Repository,
    head: &CommitSha,
    source: &CommitSha,
) -> Result<std::result::Result<(StackedMember, CommitSha), Vec<String>>> {
    let tree = match repository.merged_tree(head.as_str(), source.as_str())? {
        Ok(tree) => tree,
        Err(files) => return Ok(Err(files)),
    };
    let stacked = repository.commit_tree(
        &tree,
        head.as_str(),
        &["dagq landing verification".to_owned()],
    )?;
    Ok(Ok((
        StackedMember {
            tree,
            source: source.clone(),
        },
        stacked,
    )))
}

fn verification_prepared<'a>(
    repository: &dyn Repository,
    head: &CommitSha,
    member: &'a StackedMember,
    stacked: &'a CommitSha,
) -> Result<&'a CommitSha> {
    let source = &member.source;
    Ok(
        if repository.is_ancestor(head.as_str(), source.as_str())?
            && repository.tree_of(source.as_str())? == member.tree
        {
            source
        } else {
            stacked
        },
    )
}

/// The program history prepared so far in this stage. Members without a
/// selected program need no history until a later program needs their prefix.
struct ProgramPrefix {
    head: CommitSha,
    len: usize,
}

enum ProgramGate {
    Skip,
    Ordinary,
    Run { from: CommitSha, rebased: CommitSha },
}

impl ProgramPrefix {
    fn prepare<'a>(
        &mut self,
        control_repo: &dyn Repository,
        base: &CommitSha,
        accepted: &[(usize, CommitSha)],
        prefix: impl Iterator<Item = (&'a TaskRun, &'a StackedMember)>,
        selected: &[super::super::review_programs::SnapshotProgram],
        events: &[RunEvent],
    ) -> Result<ProgramGate> {
        if selected.is_empty() {
            return Ok(ProgramGate::Skip);
        }
        let mut excluded: Vec<_> = accepted.iter().map(|(_, source)| source.clone()).collect();
        let mut from = self.head.clone();
        for (index, (run, member)) in prefix.enumerate() {
            if index >= self.len {
                let repository = super::super::run_git::for_run(control_repo, run)?;
                let Some(next) =
                    prepare_program_head(&*repository, run, base, &self.head, member, &excluded)?
                else {
                    return Ok(ProgramGate::Ordinary);
                };
                from = std::mem::replace(&mut self.head, next);
                self.len += 1;
            }
            excluded.push(member.source.clone());
        }
        Ok(if programs::passed_before(events, &self.head) {
            ProgramGate::Skip
        } else {
            ProgramGate::Run {
                from,
                rebased: self.head.clone(),
            }
        })
    }
}

/// Take the ordinary review range with the landing branch, minus the
/// histories of already landed batch members and earlier members of this
/// stage. Exclude their original sources, so a squash in an earlier stage
/// still removes the commits it covered. Removed members are absent from
/// both sets.
fn prepare_program_head(
    repository: &dyn Repository,
    run: &TaskRun,
    base: &CommitSha,
    from: &CommitSha,
    member: &StackedMember,
    excluded: &[CommitSha],
) -> Result<Option<CommitSha>> {
    let source = member.source.as_str();
    let range_base = super::super::review::range_base(repository, run, base, source)?;
    if !repository.is_ancestor(&range_base, source)? {
        return Ok(None);
    }
    let commits = repository.commits_for_replay(&range_base, source, excluded)?;
    // Require a nonempty, merge-free range even if we can use the source
    // unchanged. A replay conflict or a different final tree also leaves
    // the member to ordinary landing.
    if commits.is_empty() || commits.iter().any(|(_, merge)| *merge) {
        return Ok(None);
    }
    if repository.is_ancestor(from.as_str(), source)? && repository.tree_of(source)? == member.tree
    {
        return Ok(Some(member.source.clone()));
    }
    let mut prepared = from.clone();
    for (commit, _) in commits {
        let Some(next) = repository.replay_commit(prepared.as_str(), commit.as_str())? else {
            return Ok(None);
        };
        prepared = next;
    }
    Ok((repository.tree_of(prepared.as_str())? == member.tree).then_some(prepared))
}

struct StackedMember {
    tree: String,
    source: CommitSha,
}

struct Stack {
    head: CommitSha,
    members: Vec<StackedMember>,
    stopped: Option<(usize, Gate)>,
}

enum Applied {
    Landed(CommitSha),
    Removed,
    Stopped,
}

#[cfg(test)]
mod tests {
    use super::super::tests::{BatchGit, FakeRepository, ScriptedVerifier, run};
    use super::*;
    use crate::application::review_programs::SnapshotProgram;
    use crate::domain::EventId;
    use crate::domain::review_programs::{BuiltinProgram, BuiltinProgramSetting, ReviewProgram};

    const BASE: &str = "1111111111111111111111111111111111111111";
    const A: &str = "2222222222222222222222222222222222222222";
    const B: &str = "3333333333333333333333333333333333333333";
    const STACKED: &str = "4444444444444444444444444444444444444444";
    const COPIED_A: &str = "5555555555555555555555555555555555555555";
    const COPIED_B: &str = "6666666666666666666666666666666666666666";
    const SQUASH: &str = "7777777777777777777777777777777777777777";
    const REWORDED: &str = "8888888888888888888888888888888888888888";
    const OTHER_COPY: &str = "9999999999999999999999999999999999999999";

    fn sha(text: &str) -> CommitSha {
        CommitSha::parse(text, "fixture").unwrap()
    }

    fn repository(git: BatchGit) -> FakeRepository {
        let mut repository = FakeRepository::sound();
        repository.batch = Some(git);
        repository
    }

    fn member(source: &str, tree: &str) -> StackedMember {
        StackedMember {
            source: sha(source),
            tree: tree.to_owned(),
        }
    }

    fn prefix(base: &str) -> ProgramPrefix {
        ProgramPrefix {
            head: sha(base),
            len: 0,
        }
    }

    fn program(paths: &str) -> ReviewProgram {
        ReviewProgram {
            name: "project".to_owned(),
            script: "scripts/review.sh".to_owned(),
            paths: vec![paths.to_owned()],
            args: vec![],
            timeout_secs: None,
            builtin: None,
        }
    }

    fn select_programs(
        repository: &dyn Repository,
        configured: Option<ReviewProgram>,
        (head, prepared): (&str, &str),
    ) -> Vec<SnapshotProgram> {
        let mut verifier = ScriptedVerifier::new(vec![], false);
        verifier.review.builtins.push(BuiltinProgramSetting {
            program: BuiltinProgram::Secrets,
            enabled: Some(false),
            paths: None,
        });
        verifier.review.programs.extend(configured);
        programs::select(
            repository,
            &verifier,
            None,
            &sha(BASE),
            (&sha(head), &sha(prepared)),
        )
        .unwrap()
    }

    fn passed(run: &TaskRun, head: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: Some(run.task_id()),
            goal_id: None,
            run_id: Some(run.id().clone()),
            kind: EventKind::IntegrateProgramsFinished.into(),
            payload: json!({"outcome": "passed", "head": head}),
            created_at: String::new(),
            actor: None,
        }
    }

    #[test]
    fn verification_uses_its_own_stack_even_if_program_history_could_merge() {
        let repo = repository(BatchGit {
            ancestors: vec![(BASE, A), (A, B)],
            trees: vec![(A, "x=1")],
            merges: vec![(BASE, A, "x=1"), (A, B, "x=2")],
            stacked: Some(STACKED),
            ..Default::default()
        });
        let base = sha(BASE);
        let (member, next) = verification_member(&repo, &base, &sha(A))
            .unwrap()
            .ok()
            .unwrap();
        assert_eq!(member.tree, "x=1");
        assert_eq!(
            verification_prepared(&repo, &base, &member, &next).unwrap(),
            &sha(A)
        );
        assert_eq!(next, sha(STACKED));
        // B descends from A. The synthetic verification prefix still has
        // the original x=0 as its common ancestor with B.
        let conflict = verification_member(&repo, &next, &sha(B))
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(conflict, ["x"]);
        assert_eq!(
            *repo.calls.borrow(),
            [
                format!("merge {BASE} {A}"),
                format!("stack x=1 {BASE}"),
                format!("is_ancestor {BASE} {A}"),
                format!("merge {STACKED} {B}"),
            ]
        );
    }

    #[test]
    fn selection_uses_the_landing_version_and_the_verification_diff() {
        let repo = repository(BatchGit::default());
        let selected = select_programs(&repo, Some(program("src/**")), (STACKED, COPIED_B));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].script, format!("{BASE}:scripts/review.sh"));
        assert_eq!(
            *repo.calls.borrow(),
            [
                format!("read {BASE} dagq.toml"),
                format!("changed {STACKED} {COPIED_B}"),
                format!("read {BASE} scripts/review.sh"),
            ]
        );
        // The built-in's argument binding uses the program's parent and
        // head after selection, even though the path comparison above
        // used the verification commits.
        let verifier = ScriptedVerifier::new(vec![], false);
        let builtin = programs::select(
            &repo,
            &verifier,
            None,
            &sha(BASE),
            (&sha(STACKED), &sha(COPIED_B)),
        )
        .unwrap()
        .pop()
        .unwrap()
        .with_range(COPIED_A, B);
        assert_eq!(builtin.program.args, [COPIED_A, B]);
    }

    #[test]
    fn replay_excludes_original_sources_of_accepted_and_preceding_members() {
        let repo = repository(BatchGit {
            base: Some(BASE),
            ancestors: vec![(BASE, A), (BASE, B)],
            ranges: vec![(A, vec![(A, false)]), (B, vec![(B, false)])],
            trees: vec![
                (COPIED_A, "first tree"),
                (COPIED_B, "second tree"),
                (OTHER_COPY, "second tree"),
            ],
            replay: vec![
                (SQUASH, A, COPIED_A),
                (SQUASH, B, OTHER_COPY),
                (COPIED_A, B, COPIED_B),
            ],
            ..Default::default()
        });
        let run = run(Path::new("/run"));
        let selected = select_programs(&repo, Some(program("src/**")), (STACKED, B));
        let first = member(A, "first tree");
        let second = member(B, "second tree");
        let mut history = prefix(SQUASH);
        let accepted = [(0, sha(A))];
        let gate = history
            .prepare(
                &repo,
                &sha(SQUASH),
                &accepted,
                std::iter::once((&run, &second)),
                &selected,
                &[],
            )
            .unwrap();
        assert!(matches!(gate, ProgramGate::Run { from, rebased }
            if from == sha(SQUASH) && rebased == sha(OTHER_COPY)));
        assert!(repo.calls.borrow().contains(&format!("base {SQUASH} {B}")));
        assert!(
            repo.calls
                .borrow()
                .contains(&format!("range {BASE} {B} exclude {A}"))
        );

        repo.calls.borrow_mut().clear();
        let gate = prefix(SQUASH)
            .prepare(
                &repo,
                &sha(SQUASH),
                &[],
                [(&run, &first), (&run, &second)].into_iter(),
                &selected,
                &[],
            )
            .unwrap();
        assert!(matches!(gate, ProgramGate::Run { from, rebased }
            if from == sha(COPIED_A) && rebased == sha(COPIED_B)));
        let ranges: Vec<_> = repo
            .calls
            .borrow()
            .iter()
            .filter(|call| call.starts_with("range "))
            .cloned()
            .collect();
        assert_eq!(
            ranges,
            [
                format!("range {BASE} {A} exclude "),
                format!("range {BASE} {B} exclude {A}"),
            ]
        );

        // When A was removed it is absent both from accepted and from
        // this stage's prefix. B's range calculation no longer excludes it.
        repo.calls.borrow_mut().clear();
        prefix(SQUASH)
            .prepare(
                &repo,
                &sha(SQUASH),
                &[],
                std::iter::once((&run, &second)),
                &selected,
                &[],
            )
            .unwrap();
        assert!(
            repo.calls
                .borrow()
                .contains(&format!("range {BASE} {B} exclude "))
        );
    }

    #[test]
    fn invalid_history_uses_ordinary_landing_before_the_fast_path_or_any_program() {
        let run = run(Path::new("/run"));
        let member = member(A, "first");
        for (ancestors, range) in [
            (vec![], vec![(A, false)]),
            (vec![(BASE, A)], vec![]),
            // Descendant and same tree, but its range contains a merge.
            (vec![(BASE, A)], vec![(A, true)]),
        ] {
            let repo = repository(BatchGit {
                base: Some(BASE),
                ancestors,
                ranges: vec![(A, range)],
                trees: vec![(A, "first")],
                ..Default::default()
            });
            let selected = select_programs(&repo, Some(program("src/**")), (BASE, A));
            repo.calls.borrow_mut().clear();
            let gate = prefix(BASE)
                .prepare(
                    &repo,
                    &sha(BASE),
                    &[],
                    std::iter::once((&run, &member)),
                    &selected,
                    &[],
                )
                .unwrap();
            assert!(matches!(gate, ProgramGate::Ordinary));
            let calls = repo.calls.borrow();
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call.starts_with("is_ancestor "))
                    .count(),
                1
            );
            assert!(!calls.iter().any(|call| call.starts_with("replay ")));
        }
    }

    #[test]
    fn unselected_programs_skip_history_but_a_later_program_needs_the_prefix() {
        let repo = repository(BatchGit {
            base: Some(BASE),
            ancestors: vec![(BASE, A), (BASE, B)],
            ranges: vec![(A, vec![(A, true)]), (B, vec![(B, false)])],
            ..Default::default()
        });
        let run = run(Path::new("/run"));
        let first = member(A, "first");
        let second = member(B, "second");
        let mut history = prefix(BASE);
        for configured in [None, Some(program("unrelated/**"))] {
            let selected = select_programs(&repo, configured, (BASE, A));
            assert!(selected.is_empty());
            repo.calls.borrow_mut().clear();
            let gate = history
                .prepare(
                    &repo,
                    &sha(BASE),
                    &[],
                    std::iter::once((&run, &first)),
                    &selected,
                    &[],
                )
                .unwrap();
            assert!(matches!(gate, ProgramGate::Skip));
            assert!(repo.calls.borrow().is_empty());
        }
        let selected = select_programs(&repo, Some(program("src/**")), (STACKED, B));
        repo.calls.borrow_mut().clear();
        let gate = history
            .prepare(
                &repo,
                &sha(BASE),
                &[],
                [(&run, &first), (&run, &second)].into_iter(),
                &selected,
                &[],
            )
            .unwrap();
        assert!(matches!(gate, ProgramGate::Ordinary));
        assert_eq!(
            *repo.calls.borrow(),
            [
                format!("base {BASE} {A}"),
                format!("is_ancestor {BASE} {A}"),
                format!("range {BASE} {A} exclude "),
            ]
        );
    }

    #[test]
    fn replay_conflict_or_a_different_final_tree_uses_ordinary_landing() {
        let run = run(Path::new("/run"));
        let member = member(B, "verification tree");
        for replay in [
            vec![(SQUASH, A, COPIED_A)],
            vec![(SQUASH, A, COPIED_A), (COPIED_A, B, COPIED_B)],
        ] {
            let repo = repository(BatchGit {
                base: Some(BASE),
                ancestors: vec![(BASE, B)],
                ranges: vec![(B, vec![(A, false), (B, false)])],
                trees: vec![(COPIED_B, "different tree")],
                replay,
                ..Default::default()
            });
            let selected = select_programs(&repo, Some(program("src/**")), (STACKED, B));
            let gate = prefix(SQUASH)
                .prepare(
                    &repo,
                    &sha(SQUASH),
                    &[],
                    std::iter::once((&run, &member)),
                    &selected,
                    &[],
                )
                .unwrap();
            assert!(matches!(gate, ProgramGate::Ordinary));
            assert!(repo.calls.borrow().ends_with(&[
                format!("replay {SQUASH} {A}"),
                format!("replay {COPIED_A} {B}"),
            ]));
        }
    }

    #[test]
    fn program_pass_is_reused_only_for_the_same_prepared_sha() {
        let run = run(Path::new("/run"));
        let fixture = |source, prepared| {
            repository(BatchGit {
                base: Some(BASE),
                ancestors: vec![(BASE, source)],
                ranges: vec![(source, vec![(source, false)])],
                trees: vec![(prepared, "second")],
                replay: vec![(COPIED_A, source, prepared), (SQUASH, source, prepared)],
                ..Default::default()
            })
        };
        let events = [passed(&run, COPIED_B)];
        for (from, source, prepared) in [
            (COPIED_A, B, COPIED_B),
            (COPIED_A, REWORDED, OTHER_COPY),
            (SQUASH, B, OTHER_COPY),
        ] {
            let repo = fixture(source, prepared);
            let selected = select_programs(&repo, Some(program("src/**")), (STACKED, source));
            let gate = prefix(from)
                .prepare(
                    &repo,
                    &sha(BASE),
                    &[],
                    std::iter::once((&run, &member(source, "second"))),
                    &selected,
                    &events,
                )
                .unwrap();
            if prepared == COPIED_B {
                assert!(matches!(gate, ProgramGate::Skip));
            } else {
                assert!(matches!(gate, ProgramGate::Run { from: parent, rebased }
                    if parent == sha(from) && rebased == sha(prepared)));
            }
        }

        let repo = repository(BatchGit {
            base: Some(BASE),
            ancestors: vec![(BASE, B), (A, B)],
            ranges: vec![(B, vec![(B, false)])],
            trees: vec![(B, "second")],
            ..Default::default()
        });
        let selected = select_programs(&repo, Some(program("src/**")), (STACKED, B));
        let member = member(B, "second");
        assert!(matches!(
            prefix(BASE).prepare(&repo, &sha(BASE), &[], std::iter::once((&run, &member)), &selected, &[]).unwrap(),
            ProgramGate::Run { rebased, .. } if rebased == sha(B)
        ));
        assert!(matches!(
            prefix(A)
                .prepare(
                    &repo,
                    &sha(A),
                    &[],
                    std::iter::once((&run, &member)),
                    &selected,
                    &[passed(&run, B)]
                )
                .unwrap(),
            ProgramGate::Skip
        ));
        assert!(
            !repo
                .calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("replay "))
        );
    }
}
