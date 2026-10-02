//! Plan review (ADR-0041 decisions 11-15, 17): the supervisor takes the
//! submitted proposals one at a time (interrupt first, then the oldest
//! submission) through a headless job, applies its verdict, applies a
//! person's answer to its `approve_plan` ask, delivers a revise to the
//! proposal's planner (or opens a planner of the runtime's for it, within
//! `runtime_planners` of [`LoopSettings::limits`]), tells the inbox of a planner that
//! does not answer, and ends the runtime's planners that are done. None of
//! this takes a run slot; a failure is logged and tried again on a later
//! pass.

use super::*;
use crate::domain::ActorContext;
use crate::domain::EventKind;
use crate::domain::language::with_instruction;
use crate::{
    application::{
        PlanReviewApply, PlanReviewFailure, PlanReviewJob, PlannerHold, RevisingProposal,
        StatusFilter, TaskListItem, TaskQuery, job_start_failure,
        planner::{
            PLANNER_DEBUG_LOG, PlannerLaunch, PlannerProbes, PlannerView, open_runtime_planner,
            planner_last_activity, planner_view,
        },
        planner_idle_marker,
        prompt::{
            DUPLICATE_CANDIDATES, DuplicateCandidates, PLAN_REVIEW_ACCESS, PlanReviewMaterial,
            plan_review_prompt, plan_revise_request, precedent_line,
        },
        screen_idle::{self, Inference, ScreenIdle},
    },
    domain::{
        MAX_PLAN_REVISES, PLAN_OPTIONS, PLAN_REVIEW_ASKER, PlanReviewDecision, PlanReviewVerdict,
        PlannerOrigin, PlannerState, Proposal, ProposalId, Task, TaskDetail,
        actor_model::{ActorLaunch, JobRoute, ModelRole, job_route},
        claim_defer::expected_files,
        next_to_review,
        search::{SearchKind, SearchQuery, SearchRef, any_word_query},
        stats::{LiveSnapshot, SlotSnapshot, StatsQuery, conflicts::ConflictHotspot},
    },
};
use std::collections::BTreeMap;

/// Asks a person answered that the plan review prompt offers as
/// precedents, newest first.
const PRECEDENT_ASKS: usize = 30;

/// Files that conflict often the plan review prompt lists at most.
const HOTSPOT_FILES: usize = 15;

/// Ready and in-progress tasks the plan review prompt lists at most, in
/// summary (task 591); the prompt says how many it left out.
const QUEUED_TASKS: usize = 200;

/// How the error of a plan review no agent ran begins when `--no-claude`
/// leaves no provider for it: the proposal waits for a person
/// (`plan_review_failed`, plan review by hand).
const PLAN_REVIEW_PROVIDER_DISABLED: &str = "provider_disabled: Claude is disabled by --no-claude";

/// The plan review job running now: one at a time, queue-wide.
pub(super) struct PlanReviewWatch {
    pub(super) job: PlanReviewJob,
    pub(super) headless: HeadlessJob,
    /// How many times the proposal was sent back when the job started.
    pub(super) revise_count: u32,
    /// Whether its role names its provider, so that a provider that cannot
    /// be used moves it (ADR-t1063-1 decision 4).
    pub(super) switchable: bool,
}

/// Where the next plan review goes (ADR-t1063-1 decisions 1, 4 and 5,
/// ADR-t1204-1, as ADR-t1207-1 does for the run review).
enum PlanReviewRoute {
    /// Start on this launch; `true` when `[roles.plan_review]` names its
    /// provider, so a provider that cannot be used moves it to the other.
    Start(ActorLaunch, bool),
    /// Wait until a provider can be used.
    Wait,
    /// Under `--no-claude` no provider can review it: a person does, told
    /// why (`plan_review_failed`).
    Manual(String),
}

impl Supervisor<'_> {
    /// One pass of plan review: apply the answers, reap the job and apply
    /// its verdict, start the next one (unless the loop is draining), then
    /// deliver the revises and tend the planners.
    pub(super) fn plan_review_pass(&mut self, options: &LoopSettings, starting: bool) -> bool {
        let mut progressed = false;
        match self.queue.settle_proposals() {
            Ok(settled) => {
                for (proposal, status) in &settled {
                    info!(
                        "proposal {proposal} is {}: none of its tasks waits for plan review any more",
                        status.as_str()
                    );
                }
                progressed |= !settled.is_empty();
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "plan review: could not settle the proposals: {error:#}")
            }
        }
        for (what, result) in [
            ("apply the plan answers", self.apply_plan_answers()),
            ("reap the plan review", self.poll_plan_review()),
        ] {
            match result {
                Ok(done) => progressed |= done,
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "plan review: could not {what}: {error:#}")
                }
            }
        }
        if !starting {
            return progressed;
        }
        // A job reaped above may have hit a login or the usage limit and
        // raised the hold in this pass (task 438): the route reads it, and
        // a role that names its provider may run on Codex while Claude is
        // held (ADR-t1063-1 decision 5).
        if let Err(error) = self.start_plan_review() {
            warn!(error = %format_args!("{error:#}"), "plan review: could not start a plan review: {error:#}");
        }
        // The planners are Claude's sessions: the hold stops them.
        if self.queue_hold.is_none()
            && let Err(error) = self.tend_planners(options)
        {
            warn!(error = %format_args!("{error:#}"), "plan review: could not deliver the revises: {error:#}");
        }
        progressed
    }

    /// Where the next plan review goes. A role that names no provider runs
    /// on Claude as before: it waits while the queue's hold ask holds
    /// Claude, and under `--no-claude` a person reviews it. One that names
    /// its provider starts there when it can be used, else on the other
    /// provider when that one runs the role and can be used, else waits,
    /// or, under `--no-claude`, goes to a person told why; a Codex plan
    /// review that fails under `--no-claude` never moves to Claude.
    fn plan_review_route(&self) -> PlanReviewRoute {
        let role = ModelRole::PlanReview;
        let models = self.role_models(role);
        let launch = models.launch(role);
        if !models.switchable(role) {
            if self.no_claude {
                return PlanReviewRoute::Manual(format!(
                    "{PLAN_REVIEW_PROVIDER_DISABLED}; handle this role manually"
                ));
            }
            return match self.queue_hold {
                Some(_) => PlanReviewRoute::Wait,
                None => PlanReviewRoute::Start(launch, false),
            };
        }
        match job_route(&launch, true, |provider| self.job_unusable(provider)) {
            JobRoute::Start(launch) => PlanReviewRoute::Start(launch, true),
            JobRoute::Wait { .. } if self.no_claude => {
                let codex = self
                    .job_unusable(Provider::Codex)
                    .map_or("unknown", |reason| reason.as_str());
                PlanReviewRoute::Manual(format!(
                    "{PLAN_REVIEW_PROVIDER_DISABLED} and codex cannot be used ({codex}); handle this role manually"
                ))
            }
            JobRoute::Wait { provider, reason } => {
                tracing::debug!(
                    "the plan review waits: {} cannot be used ({}), nor can the other provider",
                    provider.as_str(),
                    reason.as_str()
                );
                PlanReviewRoute::Wait
            }
        }
    }

    /// Start the plan review of the next candidate, when none runs.
    fn start_plan_review(&mut self) -> Result<()> {
        if self.plan_review.is_some() {
            return Ok(());
        }
        let Some(proposal_id) = next_to_review(&self.queue.plan_review_candidates()?) else {
            return Ok(());
        };
        let (launch, switchable, manual) = match self.plan_review_route() {
            PlanReviewRoute::Start(launch, switchable) => (launch, switchable, None),
            PlanReviewRoute::Wait => return Ok(()),
            // Recorded as a job that could not start, on the provider the
            // role names, so that the person sees why.
            PlanReviewRoute::Manual(why) => {
                (self.actor_launch(ModelRole::PlanReview), false, Some(why))
            }
        };
        let Some(job) = self.queue.begin_plan_review(
            proposal_id,
            &self.token,
            &self.layout.plan_reviews_dir,
            &self.layout.repo_root,
            &launch,
        )?
        else {
            return Ok(());
        };
        if let Some(why) = manual {
            let error = format!("the headless plan review could not start: {why}");
            self.fail_plan_review(
                &job,
                &PlanReviewFailure {
                    error,
                    ..PlanReviewFailure::default()
                },
            );
            return Ok(());
        }
        let proposal = self.queue.show_proposal(proposal_id)?;
        match self.spawn_plan_review(&job, &proposal, &launch) {
            Ok(Ok(headless)) => {
                info!(task_id = %job.anchor, "proposal {proposal_id} plan review {} started on {}", job.attempt, launch.provider.as_str());
                self.plan_review = Some(PlanReviewWatch {
                    job,
                    headless,
                    revise_count: proposal.revise_count(),
                    switchable,
                });
            }
            // Its own preparation failed: no provider was tried.
            Err(failed) => {
                let error = format!("the headless plan review could not start: {failed:#}");
                self.fail_plan_review(
                    &job,
                    &PlanReviewFailure {
                        error,
                        ..PlanReviewFailure::default()
                    },
                );
            }
            Ok(Err(failed)) => {
                let error = format!("the headless plan review could not start: {failed:#}");
                let unusable = self.job_provider_failed(
                    launch.provider,
                    job_start_failure(&failed),
                    (&error, &error),
                    &HoldJob::PlanReview(proposal_id),
                    switchable,
                );
                self.fail_plan_review(
                    &job,
                    &PlanReviewFailure {
                        error,
                        unusable,
                        ..PlanReviewFailure::default()
                    },
                );
            }
        }
        Ok(())
    }

    /// Write the prompt into the job's directory and start the headless
    /// job in the repository's checkout, allowed to read only.
    /// The outer error is one of the job's own preparation (its directory,
    /// its prompt), the inner one the start of its provider's process: only
    /// the latter says whether the provider can be used.
    fn spawn_plan_review(
        &mut self,
        job: &PlanReviewJob,
        proposal: &Proposal,
        launch: &ActorLaunch,
    ) -> Result<Result<HeadlessJob>> {
        self.files
            .create_dir_all(&job.dir)
            .with_context(|| format!("create {}", job.dir.display()))?;
        let prompt = self.plan_review_material(proposal)?;
        self.files
            .write(&job.dir.join("prompt.txt"), prompt.as_bytes())?;
        let stdout = job.dir.join("review.out");
        let stderr = job.dir.join("review.err");
        Ok(self.start_plan_review_job(job, launch, &prompt, stdout, stderr))
    }

    /// Start the provider's process of the plan review: Claude's `claude
    /// -p`, or Codex's `codex exec --json` in its read-only sandbox, which
    /// reads the files and the queue as [`PLAN_REVIEW_ACCESS`] allows.
    fn start_plan_review_job(
        &mut self,
        job: &PlanReviewJob,
        launch: &ActorLaunch,
        prompt: &str,
        stdout: PathBuf,
        stderr: PathBuf,
    ) -> Result<HeadlessJob> {
        let agent = self
            .job_agent(launch.provider)
            .with_context(|| format!("no {} runs on this supervisor", launch.provider.as_str()))?;
        let child = self
            .actors_on(agent)
            .spawn(ActorExecutionSpec::new(
                ActorContext::plan_review_job(job.proposal_id, job.attempt),
                WorkspaceAccess::Read(self.layout.repo_root.clone()),
                ActorProgram::Headless {
                    program: HeadlessProgram::Job {
                        cwd: &self.layout.repo_root,
                        prompt,
                        access: PLAN_REVIEW_ACCESS,
                    },
                    session_id: job.session_id.as_deref(),
                    launch: Some(launch),
                    without_mcp: false,
                    without_env: &[],
                    env: Vec::new(),
                    streams: Streams::Files {
                        stdout: &stdout,
                        stderr: &stderr,
                    },
                },
            ))
            .context("start the plan review")?
            .process()?;
        Ok(self.headless_job(
            "plan review",
            child,
            stdout,
            stderr,
            JobSubject {
                kind: headless_job::PLAN_REVIEW,
                label: None,
                run_id: None,
                proposal_id: Some(job.proposal_id),
                goal_id: None,
                attempt: job.attempt,
                provider: launch.provider,
            },
        ))
    }

    /// The plan review prompt of `proposal` from the queue as it is now.
    fn plan_review_material(&mut self, proposal: &Proposal) -> Result<String> {
        let tasks = proposal
            .task_ids()
            .iter()
            .map(|&id| self.queue.show(id))
            .collect::<Result<Vec<_>>>()?;
        let mut goal_ids: Vec<_> = tasks
            .iter()
            .filter_map(|detail| detail.task.goal_id())
            .chain(proposal.goal_ids().iter().copied())
            .collect();
        goal_ids.sort();
        goal_ids.dedup();
        let goals = goal_ids
            .into_iter()
            .map(|id| Ok(self.queue.show_goal(id)?.goal))
            .collect::<Result<Vec<_>>>()?;
        let lint = crate::domain::lint::lint(&self.queue.lint_input(proposal.task_ids())?);
        let mut others = Vec::new();
        for other in self.queue.proposals(false)? {
            if other.id() == proposal.id() {
                continue;
            }
            let tasks = other
                .task_ids()
                .iter()
                .map(|&id| Ok(self.queue.show(id)?.task))
                .collect::<Result<Vec<_>>>()?;
            others.push((other, tasks));
        }
        let page = self.queue.list(&TaskQuery {
            status: StatusFilter::Only(vec![TaskStatus::Ready, TaskStatus::InProgress]),
            limit: QUEUED_TASKS,
            full: true,
            ..TaskQuery::default()
        })?;
        let queued_left_out = page.total.saturating_sub(page.tasks.len());
        let queued = page.tasks;
        let expected = self.plan_expected_files(&tasks, &queued)?;
        let precedents = self.queue.answered_asks(PRECEDENT_ASKS)?;
        let hotspots = self.conflict_hotspots()?;
        let candidates = tasks
            .iter()
            .map(|detail| self.duplicate_candidates(proposal, &detail.task))
            .collect::<Result<Vec<_>>>()?;
        let prompt = plan_review_prompt(&PlanReviewMaterial {
            proposal,
            tasks: &tasks,
            goals: &goals,
            lint: &lint,
            others: &others,
            queued: &queued,
            queued_left_out,
            expected: &expected,
            precedents: &precedents,
            hotspots: &hotspots,
            candidates: &candidates,
            repo_root: &self.layout.repo_root,
        })?;
        Ok(with_instruction(prompt, self.verifier.language().as_ref()))
    }

    /// The files each task of the proposal and each ready or in-progress
    /// task is expected to touch, by the rule of the claim's deferral
    /// (ADR-0069 decisions 1, 2): its declared paths or the files its most
    /// related landed tasks changed, and for an in-progress task also what
    /// its run changed. The proposal's tasks are read afresh, as the
    /// planner may have changed their paths.
    fn plan_expected_files(
        &mut self,
        tasks: &[TaskDetail],
        queued: &[TaskListItem],
    ) -> Result<BTreeMap<TaskId, Vec<String>>> {
        let mut expected = BTreeMap::new();
        for detail in tasks {
            let id = detail.task.id();
            expected.insert(id, self.expected_now(id)?);
        }
        for item in queued {
            expected.insert(item.id, self.expected(item.id)?);
        }
        if queued
            .iter()
            .any(|item| item.status == TaskStatus::InProgress)
        {
            for run in self.runs_in_flight()? {
                if let Some(files) = expected.get_mut(&run.task_id) {
                    *files = expected_files(&run.files, &[]);
                }
            }
        }
        Ok(expected)
    }

    /// Where the plan review starts looking for what `task` duplicates or
    /// what already made its change (goal 29): the tasks `related` ranks
    /// highest, and the tasks and landed commits `search` finds for the
    /// words of its title, at most [`DUPLICATE_CANDIDATES`] of each, none of
    /// them the proposal's own.
    fn duplicate_candidates(
        &self,
        proposal: &Proposal,
        task: &Task,
    ) -> Result<DuplicateCandidates> {
        let own = proposal.task_ids();
        let wanted = DUPLICATE_CANDIDATES + own.len();
        let related = self
            .queue
            .related_tasks(task.id(), wanted)?
            .related
            .into_iter()
            .filter(|candidate| !own.contains(&TaskId::new(candidate.id)))
            .take(DUPLICATE_CANDIDATES)
            .collect();
        let search = match any_word_query(task.title()) {
            Some(terms) => self
                .queue
                .search_documents(&SearchQuery {
                    terms,
                    kinds: vec![SearchKind::Task, SearchKind::Commit],
                    limit: wanted,
                    ..SearchQuery::default()
                })?
                .hits
                .into_iter()
                .filter(|hit| {
                    // A task's hit is its ID; a commit's names its task.
                    let task = match (hit.kind, &hit.id) {
                        (SearchKind::Task, SearchRef::Id(id)) => Some(*id),
                        _ => hit.task_id,
                    };
                    !task.is_some_and(|id| own.contains(&TaskId::new(id)))
                })
                .take(DUPLICATE_CANDIDATES)
                .collect(),
            None => Vec::new(),
        };
        Ok(DuplicateCandidates {
            task_id: task.id(),
            related,
            search,
        })
    }

    /// The files the landings conflicted in, as `stats` counts them over
    /// its default window (goal 31), most conflicts first, at most
    /// [`HOTSPOT_FILES`] of those main still has.
    fn conflict_hotspots(&self) -> Result<Vec<ConflictHotspot>> {
        Ok(self
            .conflict_hotspot_files()?
            .into_iter()
            .filter(|file| file.state != "deleted")
            .take(HOTSPOT_FILES)
            .collect())
    }

    /// Every file the landings conflicted in, as `stats` counts them over
    /// its default window, most conflicts first; its `alert` is judged by
    /// the `[conflicts]` thresholds this process read.
    pub(super) fn conflict_hotspot_files(&self) -> Result<Vec<ConflictHotspot>> {
        let events = self.queue.all_events()?;
        let live = LiveSnapshot {
            history: crate::application::stats::conflict_history(&events, &|since| {
                self.repository.main_history(since)
            }),
            conflicts: self.conflicts,
            ..LiveSnapshot::default()
        };
        let stats = crate::domain::stats::stats(
            &events,
            &self.queue.task_goals()?,
            self.generators.clock.now(),
            SlotSnapshot::default(),
            &StatsQuery::default(),
            &live,
        );
        Ok(stats.conflict_hotspots.files)
    }

    /// Reap the job once it ended and apply its verdict, or record its
    /// failure.
    fn poll_plan_review(&mut self) -> Result<bool> {
        let Some(provider) = self.plan_review.as_ref().map(|w| w.headless.provider) else {
            return Ok(false);
        };
        // The job's reply, session and failure are read by the provider
        // it ran on (ADR-t1063-1 decisions 2, 4 and 6).
        let agent = self.job_agent(provider).unwrap_or(self.reviewer);
        let watch = self.plan_review.as_mut().expect("read above");
        let Some(outcome) = watch.headless.poll(&*self.files, agent)? else {
            return Ok(false);
        };
        let watch = self.plan_review.take().expect("polled above");
        let duration_secs = watch.headless.started.elapsed().as_secs();
        let stdout = self
            .files
            .read_to_string(&watch.headless.stdout)
            .unwrap_or_default();
        let session = agent.job_session(&stdout, watch.headless.started_at);
        let verdict = outcome.and_then(|stdout| PlanReviewVerdict::parse(&stdout));
        // Only a job that failed or printed no verdict is read for a wall:
        // a verdict's own text may quote anything (task 438).
        let failure = verdict.is_err().then(|| self.job_failure(&watch.headless));
        let applied = verdict.map_err(|error| anyhow!(error)).and_then(|verdict| {
            let job = ActorContext::plan_review_job(watch.job.proposal_id, watch.job.attempt);
            self.for_job(&job, |sv| {
                sv.apply_plan_verdict(
                    &watch.job,
                    watch.revise_count,
                    verdict,
                    duration_secs,
                    session.clone(),
                )
            })
        });
        if let Err(error) = applied {
            let error = format!("{error:#}");
            // Stopped at Claude's wall only a person moves: it joins the
            // hold ask, whose `done` submits the proposal again, and its
            // failure is no attention meanwhile (task 438). A role that
            // names its provider moves to the other one instead of
            // waiting; Codex's own words (its `turn.failed` is on its
            // stdout) may say when a usage limit resets.
            let said = format!("{error}\n{stdout}");
            let unusable = failure.and_then(|failure| {
                self.job_provider_failed(
                    watch.headless.provider,
                    failure,
                    (&error, &said),
                    &HoldJob::PlanReview(watch.job.proposal_id),
                    watch.switchable,
                )
            });
            self.fail_plan_review(
                &watch.job,
                &PlanReviewFailure {
                    error,
                    duration_secs,
                    session,
                    unusable,
                },
            );
        }
        Ok(true)
    }

    /// What the runtime makes of the verdict (a revise past
    /// [`MAX_PLAN_REVISES`] is a concern; the precedents it names are
    /// quoted), applied in one transaction; the inbox is told of a new
    /// `approve_plan` ask.
    fn apply_plan_verdict(
        &mut self,
        job: &PlanReviewJob,
        revise_count: u32,
        verdict: PlanReviewVerdict,
        duration_secs: u64,
        session: Option<crate::domain::headless_job::JobSession>,
    ) -> Result<()> {
        let proposal = job.proposal_id;
        let (decision, overridden) = match verdict.verdict {
            PlanReviewDecision::Revise if revise_count >= MAX_PLAN_REVISES => (
                PlanReviewDecision::Concern,
                Some(format!(
                    "proposal {proposal} was sent back {revise_count} times already (at most {MAX_PLAN_REVISES})"
                )),
            ),
            decision => (decision, None),
        };
        let answered = self.queue.answered_asks(usize::MAX >> 1)?;
        let precedents: Vec<String> = verdict
            .precedents
            .iter()
            .filter_map(|id| answered.iter().find(|ask| ask.id == *id))
            .map(precedent_line)
            .collect();
        let mut revise_reasons = verdict.reasons.clone();
        revise_reasons.extend(precedents.iter().cloned());
        let ask = (decision == PlanReviewDecision::Concern).then(|| NewAsk {
            kind: AskKind::ApprovePlan,
            task_id: Some(job.anchor),
            run_id: None,
            question: plan_question(job, &verdict, overridden.as_deref(), &precedents),
            options: PLAN_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
            asked_by: PLAN_REVIEW_ASKER.to_owned(),
            reason_category: AskReason::Scope,
            topics: Vec::new(),
            finding_id: None,
        });
        let applied = self.queue.finish_plan_review(
            job,
            &self.token,
            &PlanReviewApply {
                verdict: verdict.clone(),
                decision,
                overridden,
                revise_reasons,
                ask,
                duration_secs,
                session,
            },
        )?;
        if applied.stale {
            info!(task_id = %job.anchor, "proposal {proposal} moved on or had a task edited during its plan review; the verdict is not applied");
            return Ok(());
        }
        info!(task_id = %job.anchor, "proposal {proposal} plan review {}: {} ({})", job.attempt, decision.as_str(), verdict.summary);
        for reopened in &applied.reopened {
            info!(task_id = %reopened.task_id, "task {} left ready for proposal {} to fix it", reopened.task_id, reopened.proposal_id);
        }
        if let Some(outcome) = &applied.ask {
            // The verdict is applied: a notification that fails is only
            // reported.
            let error = match ask::notify(
                &mut *self.queue,
                &self.layout.main_checkout,
                outcome,
                self.cmux,
            ) {
                Ok(notified) => notified.get("notify_error").map(ToString::to_string),
                Err(error) => Some(format!("{error:#}")),
            };
            if let Some(error) = error {
                warn!(ask_id = %outcome.ask.id, "proposal {proposal}: the inbox was not notified of ask {}: {error}", outcome.ask.id);
            }
        }
        Ok(())
    }

    /// Record the job's failure; the proposal waits for a person
    /// (`plan_review_failed`) and is not reviewed again by itself, unless
    /// its provider could not be used and it moves (ADR-t1063-1 decision
    /// 4).
    fn fail_plan_review(&mut self, job: &PlanReviewJob, failure: &PlanReviewFailure) {
        let error = &failure.error;
        match failure.unusable {
            Some((provider, reason)) => {
                warn!(task_id = %job.anchor, error = %error, "proposal {} plan review {} failed: {error}; {} cannot be used ({}), and the proposal is reviewed again on the other provider", job.proposal_id, job.attempt, provider.as_str(), reason.as_str());
            }
            None => {
                warn!(task_id = %job.anchor, error = %error, "proposal {} plan review {} failed: {error}; it waits for a person", job.proposal_id, job.attempt);
            }
        }
        if let Err(recorded) = self.queue.fail_plan_review(job, &self.token, failure) {
            warn!(error = %format_args!("{recorded:#}"), "proposal {}: the plan review failure could not be recorded: {recorded:#}", job.proposal_id);
        }
    }

    /// Apply the answered `approve_plan` asks (ADR-0041 decision 11).
    fn apply_plan_answers(&mut self) -> Result<bool> {
        let mut applied = false;
        for answered in self.queue.plan_answers()? {
            match self.queue.decide_plan(answered.id) {
                Ok(Some(decided)) => {
                    applied = true;
                    info!(ask_id = %answered.id, "proposal {} is {} as ask {} answered {}", decided.proposal.id(), decided.proposal.status().as_str(), answered.id, decided.answer)
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(ask_id = %answered.id, error = %format_args!("{error:#}"), "the answer of ask {} could not be applied: {error:#}", answered.id)
                }
            }
        }
        Ok(applied)
    }

    /// Take back a revise whose planner is gone, tell the inbox of a revise
    /// no planner answered within the timeout, deliver every revise not
    /// delivered yet, and end the runtime's planners that are done
    /// (ADR-0041 decisions 12, 13).
    fn tend_planners(&mut self, options: &LoopSettings) -> Result<()> {
        if self.no_claude {
            return Ok(());
        }
        let now = self.generators.clock.now();
        let timeout = i64::try_from(options.planner_timeout.as_secs())?;
        let mut views = self.planner_views()?;
        for revise in self.queue.revising_proposals()? {
            let proposal = revise.proposal.id();
            match (revise.sent_at, revise.planner_id) {
                (Some(sent_at), Some(planner_id)) => {
                    let gone = views
                        .iter()
                        .find(|view| view.planner.id == planner_id)
                        .is_none_or(|view| self.planner_gone(view));
                    if gone {
                        info!(
                            "proposal {proposal}: planner {planner_id} is gone before it submitted again; the revise goes to another planner"
                        );
                        self.queue.revise_lost(
                            proposal,
                            Some(planner_id),
                            "its planner is gone before it submitted again",
                        )?;
                    } else if revise.unresponsive_at.is_none() && now - sent_at > timeout {
                        warn!(
                            "proposal {proposal}: planner {planner_id} did not submit it again within {timeout} seconds; the inbox is told"
                        );
                        self.queue.planner_unresponsive(
                            proposal,
                            Some(planner_id),
                            now - sent_at,
                            &[],
                        )?;
                    }
                }
                // A delivery claimed and never recorded (its supervisor
                // died in between) is delivered again.
                (Some(sent_at), None) if now - sent_at > HEARTBEAT_TIMEOUT_SECS => {
                    self.queue
                        .revise_lost(proposal, None, "its delivery was never recorded")?;
                }
                (None, _)
                    if revise.unresponsive_at.is_none()
                        && revise.revised_at.is_some_and(|at| now - at > timeout) =>
                {
                    let waited = now - revise.revised_at.unwrap_or(now);
                    let holders = self.planner_holds(&views)?;
                    warn!(
                        "proposal {proposal}: its revise waited {waited} seconds for a planner; the inbox is told"
                    );
                    self.queue
                        .planner_unresponsive(proposal, None, waited, &holders)?;
                }
                _ => {}
            }
        }
        let mut runtime_open = views
            .iter()
            .filter(|view| view.planner.origin == PlannerOrigin::Runtime && view.alive)
            .count();
        // A revise with no planner that waited at the limit past the
        // timeout (task 884).
        let mut starved = None;
        for revise in self.queue.revising_proposals()? {
            if revise.sent_at.is_some() {
                continue;
            }
            let proposal = &revise.proposal;
            let owner = proposal
                .owner()
                .workspace_id
                .as_deref()
                .and_then(|workspace| {
                    views
                        .iter()
                        .find(|view| view.planner.workspace_id.as_deref() == Some(workspace))
                });
            match owner {
                Some(view) if view.alive => {
                    // A planner at work gets the revise once it is idle.
                    if view.state != PlannerState::Idle
                        || !self.queue.claim_revise(proposal.id())?
                    {
                        continue;
                    }
                    let workspace = view.planner.workspace_id.clone().unwrap_or_default();
                    let text = with_instruction(
                        plan_revise_request(proposal.id(), &revise.reasons),
                        self.verifier.language().as_ref(),
                    );
                    self.stamp_planner_input(view);
                    if let Err(error) =
                        submit_input(self.cmux, self.signals, &workspace, Input::Text(&text))
                    {
                        warn!(error = %format_args!("{error:#}"), "proposal {}: the revise could not be typed into workspace {workspace}: {error:#}", proposal.id());
                        self.queue
                            .revise_lost(proposal.id(), None, "the typing failed")?;
                        continue;
                    }
                    info!(
                        "proposal {}: the revise went to planner {} in workspace {workspace}",
                        proposal.id(),
                        view.planner.id
                    );
                    self.queue
                        .revise_sent(proposal.id(), view.planner.id, &workspace, None)?;
                }
                // Not listed, yet its session runs: it is not given up on
                // that evidence; the timeout tells the inbox.
                Some(view) if !self.planner_gone(view) => {}
                _ if runtime_open < self.limits.runtime_planners.value => {
                    if !self.queue.claim_revise(proposal.id())? {
                        continue;
                    }
                    let opened = match self.open_planner_for(proposal, &revise.reasons) {
                        Ok(opened) => opened,
                        Err(error) => {
                            self.queue.revise_lost(
                                proposal.id(),
                                None,
                                "the planner could not be opened",
                            )?;
                            return Err(error);
                        }
                    };
                    runtime_open += 1;
                    let workspace = opened.planner.workspace_id.clone().unwrap_or_default();
                    info!(
                        "proposal {}: opened planner {} in workspace {workspace} for its revise",
                        proposal.id(),
                        opened.planner.id
                    );
                    self.queue.revise_sent(
                        proposal.id(),
                        opened.planner.id,
                        &workspace,
                        Some(&opened.launch),
                    )?;
                    views = self.planner_views()?;
                }
                // At the limit: the revise waits for a runtime planner to
                // end, and past the timeout frees a place (task 884).
                _ => {
                    if starved.is_none() && revise.revised_at.is_some_and(|at| now - at > timeout) {
                        starved = Some(proposal.id());
                    }
                }
            }
        }
        // Then the drafts the runtime or a job registered (ADR-0041
        // decision 16), within the same limit.
        self.deliver_planner_answers(&views, &mut runtime_open)?;
        self.open_draft_planners(&mut runtime_open)?;
        // Then the findings marked for a proposal (ADR-0044 decision 19),
        // within the same limit, once those whose proposal ended are
        // settled.
        self.settle_findings()?;
        self.open_finding_planners(&mut runtime_open)?;
        self.tell_of_silent_planners(timeout, &views)?;
        self.end_runtime_planners(&views)?;
        if let Some(proposal) = starved {
            self.release_runtime_planner(proposal, timeout, &views)?;
        }
        Ok(())
    }

    /// The runtime's planners alive, each with why it is not ended
    /// ([`Self::busy_reasons`]): what holds the limit a revise with no
    /// planner waits on (task 884).
    fn planner_holds(&mut self, views: &[PlannerView]) -> Result<Vec<PlannerHold>> {
        let revising = self.queue.revising_proposals()?;
        views
            .iter()
            .filter(|view| view.planner.origin == PlannerOrigin::Runtime && view.alive)
            .map(|view| {
                Ok(PlannerHold {
                    planner_id: view.planner.id,
                    state: view.state.as_str().to_owned(),
                    busy: self
                        .busy_reasons(view, &revising)?
                        .into_iter()
                        .map(PlannerBusy::as_str)
                        .collect(),
                    proposal_id: view.planner.proposal_id,
                    draft_task_id: view.planner.draft_task_id,
                    finding_id: view.planner.finding_id,
                })
            })
            .collect()
    }

    /// Free a place under the limit for the revise of `proposal`, which
    /// waited past the timeout with no planner (task 884): ask one idle
    /// planner of the runtime's to exit whose busy reasons do not include
    /// a question waiting on a person or an answer not typed yet, and nothing of which was seen within
    /// the timeout (no input, no idle marker). One at a time: none while a
    /// planner of the runtime's is already asked to exit, which frees its
    /// place anyway.
    fn release_runtime_planner(
        &mut self,
        proposal: ProposalId,
        timeout: i64,
        views: &[PlannerView],
    ) -> Result<()> {
        let now = self.generators.clock.now();
        let runtime = |view: &&PlannerView| view.planner.origin == PlannerOrigin::Runtime;
        if views.iter().filter(runtime).any(|view| {
            self.planner_exits
                .iter()
                .any(|(id, _)| *id == view.planner.id)
        }) {
            return Ok(());
        }
        let revising = self.queue.revising_proposals()?;
        for view in views
            .iter()
            .filter(runtime)
            .filter(|view| view.alive && view.state == PlannerState::Idle)
        {
            let Some(workspace) = view.planner.workspace_id.clone() else {
                continue;
            };
            let busy = self.busy_reasons(view, &revising)?;
            // An answer not typed yet may be left to the inbox (its typing
            // failed), which types it into this workspace by hand.
            if busy.is_empty()
                || busy.contains(&PlannerBusy::QuestionOpen)
                || busy.contains(&PlannerBusy::AnswerUndelivered)
            {
                continue;
            }
            let last = planner_last_activity(&*self.files, &view.dir, view.planner.created_at)?;
            if now - last <= timeout {
                continue;
            }
            let id = view.planner.id;
            let reasons: Vec<_> = busy.into_iter().map(PlannerBusy::as_str).collect();
            let reason = format!(
                "planner {id} of the runtime was idle for {} seconds holding a place ({}) while the revise of proposal {proposal} waited past {timeout} seconds for one; it is asked to exit",
                now - last,
                reasons.join(", ")
            );
            submit_input(self.cmux, self.signals, &workspace, Input::Exit)?;
            self.planner_exits.push((id, Instant::now()));
            self.queue.record_queue_event(
                EventKind::PlannerReleased,
                json!({
                    "planner_id": id,
                    "workspace_id": workspace,
                    "proposal_id": proposal,
                    "busy": reasons,
                    "last_activity": last,
                    "idle_since": view.idle_since,
                    "reason": reason,
                }),
            )?;
            warn!("{reason}");
            return Ok(());
        }
        Ok(())
    }

    /// Why the planner of `view` is not ended: at work, a revise of its
    /// own, or its `planner_question` ([`Self::question_wait`]), or already
    /// asked to exit. Empty for an idle planner that is done.
    fn busy_reasons(
        &mut self,
        view: &PlannerView,
        revising: &[RevisingProposal],
    ) -> Result<Vec<PlannerBusy>> {
        let id = view.planner.id;
        let workspace = view.planner.workspace_id.as_deref();
        let mut busy = Vec::new();
        if view.state != PlannerState::Idle {
            busy.push(PlannerBusy::AtWork);
        }
        if revising.iter().any(|revise| revise.planner_id == Some(id)) {
            busy.push(PlannerBusy::Revise);
        }
        if revising.iter().any(|revise| {
            revise.sent_at.is_none()
                && workspace.is_some()
                && revise.proposal.owner().workspace_id.as_deref() == workspace
        }) {
            busy.push(PlannerBusy::RevisePending);
        }
        if let Some(wait) = self.question_wait(view)? {
            busy.push(wait);
        }
        if self.planner_exits.iter().any(|(sent, _)| *sent == id) {
            busy.push(PlannerBusy::Exiting);
        }
        Ok(busy)
    }

    /// Tell the inbox, once per planner, of a planner of the runtime's
    /// (for a draft, a finding or a revise) nothing was seen of within
    /// `timeout` seconds (task 805): at work by what the runtime can tell,
    /// with no input, no idle marker and no idle screen since. It is the
    /// backstop for an idle marker its hook could not write on a screen
    /// cmux cannot read, or reads in a state it does not know. It is not
    /// closed: a person looks at it. A planner a revise went to is timed by
    /// its revise, and one that waits on its `planner_question` waits on a
    /// person; a person's planner is never timed.
    fn tell_of_silent_planners(&mut self, timeout: i64, views: &[PlannerView]) -> Result<()> {
        let now = self.generators.clock.now();
        let revising = self.queue.revising_proposals()?;
        for view in views.iter().filter(|view| {
            view.planner.origin == PlannerOrigin::Runtime
                && view.alive
                && view.state != PlannerState::Idle
        }) {
            let id = view.planner.id;
            if revising.iter().any(|revise| revise.planner_id == Some(id)) {
                continue;
            }
            let last = planner_last_activity(&*self.files, &view.dir, view.planner.created_at)?;
            let waited = now - last;
            if waited <= timeout || self.question_wait(view)?.is_some() {
                continue;
            }
            let reason = format!(
                "planner {id} of the runtime showed nothing (no input, no idle marker, no idle screen) for {waited} seconds; it is left open for a person to look at"
            );
            let payload = json!({
                "subject": "planner",
                "planner_id": id,
                "origin": view.planner.origin.as_str(),
                "workspace_id": view.planner.workspace_id,
                "draft_task_id": view.planner.draft_task_id,
                "finding_id": view.planner.finding_id,
                "state": view.state.as_str(),
                "last_activity": last,
                "waited_secs": waited,
                "reason": reason,
            });
            if self.queue.planner_silent(id, payload)? {
                warn!("{reason}; the inbox is told");
            }
        }
        Ok(())
    }

    /// Whether a planner's session is over: it exited, its wrapper stopped
    /// heartbeating, or its workspace is not listed and its wrapper is dead
    /// (a workspace not listed alone is no evidence: its session is judged
    /// by its wrapper, as a worker's is).
    fn planner_gone(&self, view: &PlannerView) -> bool {
        match view.state {
            PlannerState::Exited | PlannerState::Lost => true,
            PlannerState::Closed => view
                .planner
                .wrapper_pid
                .is_none_or(|pid| view.planner.exited_at.is_some() || !self.processes.alive(pid)),
            _ => false,
        }
    }

    /// The planners not closed, each judged by [`planner_view`], the
    /// captures of a screen standing in for a missing idle marker kept
    /// (ADR-t803-1). A span the screen was first inferred idle over is
    /// recorded as `idle_inferred` once.
    pub(super) fn planner_views(&self) -> Result<Vec<PlannerView>> {
        let probes = PlannerProbes {
            cmux: self.cmux,
            processes: &*self.processes,
            files: &*self.files,
            signals: self.signals,
            clock: &*self.generators.clock,
            planners_dir: &self.layout.planners_dir,
            screen_idle_threshold: self.stall.screen_idle(),
            screen_idle: ScreenIdle::Record(&self.screen_spans),
        };
        let views = self
            .queue
            .planners(false)?
            .into_iter()
            .map(|planner| planner_view(&probes, planner))
            .collect::<Result<Vec<_>>>()?;
        for view in &views {
            if let Some(inference) = view.idle_inferred.filter(|inference| inference.unrecorded)
                && let Err(error) = self.record_planner_idle_inferred(view, &inference)
            {
                // A queue that cannot take the event holds up nothing
                // else; the next pass records it.
                warn!(error = %format_args!("{error:#}"), "planner {}: idle_inferred could not be recorded: {error:#}", view.planner.id);
            }
        }
        Ok(views)
    }

    /// Stamp a text the supervisor is about to type into the planner of
    /// `view`, so a marker from before it no longer counts and the screen
    /// span restarts (ADR-t803-1). A stamp that cannot be written is
    /// logged: the typed text still changes the transcript the span keeps.
    pub(super) fn stamp_planner_input(&self, view: &PlannerView) {
        if let Err(error) =
            screen_idle::record_supervisor_input(&*self.files, &planner_idle_marker(&view.dir))
        {
            warn!(error = %error, "planner {}: the stamp of the typed text could not be written: {error}", view.planner.id);
        }
    }

    /// Record `idle_inferred` for the planner of `view`, with the line of
    /// its agent's debug log that says its idle hook failed, if there is
    /// one, and note its span recorded.
    fn record_planner_idle_inferred(
        &self,
        view: &PlannerView,
        inference: &Inference,
    ) -> Result<()> {
        let marker = planner_idle_marker(&view.dir);
        let mut payload = json!({
            "planner_id": view.planner.id,
            "origin": view.planner.origin.as_str(),
            "workspace_id": view.planner.workspace_id,
            "source": inference.source,
            "marker": inference.marker.as_str(),
            "since": inference.since,
            "since_ms": inference.since_ms,
            "observed_secs": inference.observed_secs,
            "observed_ms": inference.observed_ms,
            "captures": inference.captures,
            "background_running": inference.background_running,
        });
        if let Some(line) = screen_idle::hook_failure(
            &*self.files,
            self.signals,
            &view.dir.join(PLANNER_DEBUG_LOG),
        ) {
            payload["hook_error"] = json!(line);
        }
        self.queue
            .record_queue_event(EventKind::IdleInferred, payload)?;
        info!(
            "planner {} has no fresh idle marker ({}); its screen looks idle since {}",
            view.planner.id,
            inference.marker.as_str(),
            inference.since
        );
        self.screen_spans.mark_recorded(&*self.files, &marker);
        Ok(())
    }

    /// Open a planner of the runtime's for `proposal` with its reasons.
    fn open_planner_for(
        &mut self,
        proposal: &Proposal,
        reasons: &[String],
    ) -> Result<crate::application::planner::OpenedPlanner> {
        let tasks = proposal
            .task_ids()
            .iter()
            .map(|&id| Ok(self.queue.show(id)?.task))
            .collect::<Result<Vec<_>>>()?;
        open_runtime_planner(&self.planner_launch(), proposal.id(), &tasks, reasons)
    }

    /// What opening a planner of the runtime's works with.
    pub(super) fn planner_launch(&self) -> PlannerLaunch<'_> {
        let layout = self.layout;
        PlannerLaunch {
            queue: &*self.queue,
            cmux: self.cmux,
            files: &*self.files,
            db: &layout.db,
            queue_hash: &layout.queue_hash,
            planners_dir: &layout.planners_dir,
            repo_root: &layout.repo_root,
            runner: &layout.runner,
            claude: &layout.claude,
            plugin_dir: layout.plugin_dir.as_deref(),
            language: self.verifier.language(),
            roles: self.verifier.role_models().unwrap_or_else(|error| {
                warn!(error = %format_args!("{error:#}"), "[roles] could not be read; the planner starts as before: {error:#}");
                Default::default()
            }),
        }
    }

    /// End the runtime's planners that are done: one idle with no revise of
    /// its own left is asked to `/exit` once, and closed after the exit
    /// timeout if it does not; one whose session is over is given up and
    /// its workspace closed. A person's planner is never closed by the
    /// runtime.
    fn end_runtime_planners(&mut self, views: &[PlannerView]) -> Result<()> {
        let revising = self.queue.revising_proposals()?;
        for view in views
            .iter()
            .filter(|view| view.planner.origin == PlannerOrigin::Runtime)
        {
            let id = view.planner.id;
            let workspace = view.planner.workspace_id.clone();
            let asked = self
                .planner_exits
                .iter()
                .find(|(sent, _)| *sent == id)
                .map(|(_, at)| at.elapsed());
            let overdue = asked.is_some_and(|elapsed| elapsed > self.cmux.exit_timeout());
            if self.planner_gone(view) || overdue {
                if overdue {
                    warn!(
                        "planner {id} of the runtime did not exit within {} seconds of /exit; its workspace is closed",
                        self.cmux.exit_timeout().as_secs()
                    );
                }
                if let Some(workspace) = &workspace
                    && self.cmux.exists(workspace)?
                {
                    self.cmux.close(workspace)?;
                }
                self.queue.close_planner(id, None)?;
                self.planner_exits.retain(|(sent, _)| *sent != id);
                info!(
                    "planner {id} of the runtime ended ({})",
                    view.state.as_str()
                );
                continue;
            }
            if self.busy_reasons(view, &revising)?.is_empty()
                && let Some(workspace) = &workspace
            {
                submit_input(self.cmux, self.signals, workspace, Input::Exit)?;
                self.planner_exits.push((id, Instant::now()));
                info!("planner {id} of the runtime is done; asked it to exit");
            }
        }
        Ok(())
    }
}

/// Why a planner of the runtime's is not ended (task 884), as the
/// `busy` of a [`PlannerHold`] and of `planner_released` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlannerBusy {
    /// Not idle: at work, or not seen idle.
    AtWork,
    /// A revise went to it and it did not submit again yet.
    Revise,
    /// The revise of a proposal whose workspace is its own waits for it to
    /// be idle.
    RevisePending,
    /// Its `planner_question` waits for a person's answer.
    QuestionOpen,
    /// The answer of its `planner_question` waits to be typed.
    AnswerUndelivered,
    /// The answer was typed into it and it has not stopped since.
    AnswerTyped,
    /// It was asked to exit.
    Exiting,
}

impl PlannerBusy {
    fn as_str(self) -> &'static str {
        match self {
            Self::AtWork => "at_work",
            Self::Revise => "revise",
            Self::RevisePending => "revise_pending",
            Self::QuestionOpen => "planner_question_open",
            Self::AnswerUndelivered => "planner_answer_undelivered",
            Self::AnswerTyped => "planner_answer_typed",
            Self::Exiting => "exiting",
        }
    }
}

impl Supervisor<'_> {
    /// Whether, and how, a planner of the runtime's still waits on a
    /// `planner_question` about its draft or finding: one not answered
    /// yet, one answered whose answer is not typed yet, or one whose answer
    /// was typed into this planner's workspace and its agent has not
    /// stopped since the typing (task 884). The typing is timed by its
    /// claim (`planner_answer_claimed`), taken before it, not by the ask's
    /// close after it, so an agent that took the answer up and stopped
    /// before the close is done; an answer a new planner carried in its
    /// prompt is timed by the planner's opening. An ask closed without a
    /// typing holds nothing.
    fn question_wait(&mut self, view: &PlannerView) -> Result<Option<PlannerBusy>> {
        let (draft, finding) = (view.planner.draft_task_id, view.planner.finding_id);
        if draft.is_none() && finding.is_none() {
            return Ok(None);
        }
        // The drafts of its bundle (ADR-t807-1).
        let drafts = match draft {
            Some(_) => self.queue.planner_draft_tasks(view.planner.id)?,
            None => Vec::new(),
        };
        // A finding's planner asks about the finding, a draft's about the
        // drafts of its bundle.
        let asks: Vec<_> = self
            .queue
            .asks(crate::application::AskQuery {
                all: true,
                ..Default::default()
            })?
            .into_iter()
            .filter(|ask| {
                ask.kind == AskKind::PlannerQuestion
                    && match finding {
                        Some(finding) => ask.finding_id == Some(finding),
                        None => {
                            ask.finding_id.is_none()
                                && ask.task_id.is_some_and(|task| drafts.contains(&task))
                        }
                    }
            })
            .collect();
        if asks
            .iter()
            .any(|ask| ask.closed_at.is_none() && ask.answered_at.is_none())
        {
            return Ok(Some(PlannerBusy::QuestionOpen));
        }
        if asks.iter().any(|ask| ask.closed_at.is_none()) {
            return Ok(Some(PlannerBusy::AnswerUndelivered));
        }
        let Some(workspace) = view.planner.workspace_id.as_deref() else {
            return Ok(None);
        };
        for ask in &asks {
            if !self.queue.ask_delivered_to(ask.id, workspace)? {
                continue;
            }
            // An agent idle since before the typing has not taken the
            // answer up yet. Its second counts as before: the views of a
            // pass are taken before the pass types, and an agent does not
            // take an answer up within the second it was typed.
            let typed = self
                .queue
                .answer_claimed_at(ask.id, workspace)?
                .unwrap_or(view.planner.created_at);
            if view.idle_since.is_none_or(|since| since <= typed) {
                return Ok(Some(PlannerBusy::AnswerTyped));
            }
        }
        Ok(None)
    }
}

/// The question of the `approve_plan` ask a concern opens.
fn plan_question(
    job: &PlanReviewJob,
    verdict: &PlanReviewVerdict,
    overridden: Option<&str>,
    precedents: &[String],
) -> String {
    let mut question = format!(
        "Plan review of proposal {proposal} (its first task is {anchor}) needs a person: {summary}",
        proposal = job.proposal_id,
        anchor = job.anchor,
        summary = verdict.summary,
    );
    if let Some(why) = overridden {
        question.push_str(&format!(
            "\nIt answered {}, but {why}.",
            verdict.verdict.as_str()
        ));
    }
    if !verdict.reasons.is_empty() {
        question.push_str("\nReasons:");
        for reason in &verdict.reasons {
            question.push_str(&format!("\n- {reason}"));
        }
    }
    for precedent in precedents {
        question.push_str(&format!("\n- {precedent}"));
    }
    question.push_str(&format!(
        "\nPlan review material: {}/prompt.txt\nready: make the proposal's tasks ready as they are. send_back: send it back to its planner (answer `send_back: <your reason>` to add yours). cancel: cancel the proposal's tasks.",
        job.dir.display()
    ));
    question
}
