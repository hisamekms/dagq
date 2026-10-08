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
use crate::domain::EventKind;
use crate::domain::actor_model::RoleModels;
use crate::domain::language::with_instruction;
use crate::domain::provider_switch::SwitchReason;
use crate::domain::{ActorContext, Ask, AskConfidence, PlannerId};
use crate::{
    application::{
        PlanReviewApply, PlanReviewFailure, PlanReviewJob, PlannerHold, RevisingProposal,
        StatusFilter, TaskListItem, TaskQuery, job_start_failure,
        planner::{
            self, PlannerLaunch, PlannerProbes, PlannerView, open_runtime_planner,
            planner_last_activity, planner_view, retired_workspace,
        },
        planner_idle_marker,
        prompt::{
            DUPLICATE_CANDIDATES, DuplicateCandidates, PLAN_REVIEW_ACCESS, PRECEDENT_ASKS,
            PlanReviewMaterial, PlanReviewPrompt, PromptBytes, plan_review_prompt,
            plan_revise_request, precedent_line,
        },
        screen_idle,
    },
    domain::{
        PLAN_OPTIONS, PLAN_REVIEW_ASKER, PlanReviewDecision, PlanReviewVerdict, PlannerCloseCode,
        PlannerOrigin, PlannerState, Proposal, ProposalId, Task, TaskDetail,
        actor_model::{ActorLaunch, JobRoute, ModelRole, job_route, job_wait_text},
        next_to_review,
        plan_review::{
            PlanConcernDecision, PlanConcernEscalation, PlanRecommendation, PlanVerdictDecision,
            decide_verdict,
        },
        search::{SearchKind, SearchQuery, SearchRef, any_word_query},
        stats::{LiveSnapshot, SlotSnapshot, StatsQuery, conflicts::ConflictHotspot},
    },
};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

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
    /// What its prompt took, recorded when it ends (task 1561).
    pub(super) prompt_bytes: PromptBytes,
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
    /// provider when that one runs the role and can be used (unless
    /// `[provider_fallback] jobs` is off, ADR-t1857-1), else waits,
    /// or, under `--no-claude`, goes to a person told why; a Codex plan
    /// review that fails under `--no-claude` never moves to Claude.
    fn plan_review_route(&self) -> PlanReviewRoute {
        let role = ModelRole::PlanReview;
        let models = self.role_models(role);
        plan_review_route(
            models.launch(role),
            models.switchable(role),
            self.no_claude,
            self.queue_hold.is_some(),
            self.fallback.jobs,
            |provider| self.job_unusable(provider),
        )
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
            Ok((prompt_bytes, Ok(headless))) => {
                info!(task_id = %job.anchor, "proposal {proposal_id} plan review {} started on {} with a prompt of {} bytes", job.attempt, launch.provider.as_str(), prompt_bytes.total);
                self.plan_review = Some(PlanReviewWatch {
                    job,
                    headless,
                    revise_count: proposal.revise_count(),
                    switchable,
                    prompt_bytes,
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
            Ok((prompt_bytes, Err(failed))) => {
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
                        prompt_bytes: Some(prompt_bytes),
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
    ) -> Result<(PromptBytes, Result<HeadlessJob>)> {
        self.files
            .create_dir_all(&job.dir)
            .with_context(|| format!("create {}", job.dir.display()))?;
        let prompt = self.plan_review_material(proposal)?;
        self.files
            .write(&job.dir.join("prompt.txt"), prompt.text.as_bytes())?;
        let stdout = job.dir.join("review.out");
        let stderr = job.dir.join("review.err");
        let started = self.start_plan_review_job(job, launch, &prompt.text, stdout, stderr);
        Ok((prompt.bytes, started))
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

    /// The plan review prompt of `proposal` from the queue as it is now,
    /// within its limits (task 1561).
    fn plan_review_material(&mut self, proposal: &Proposal) -> Result<PlanReviewPrompt> {
        let tasks = proposal
            .task_ids()
            .iter()
            .map(|&id| self.queue.show(id))
            .collect::<Result<Vec<_>>>()?;
        // A follow_up's source goal too, whose acceptance its membership
        // judgement is checked against (ADR-t1504-2 decision 7).
        let source_goals = tasks.iter().flat_map(|detail| {
            let registered = detail
                .origin
                .as_ref()
                .filter(|origin| origin.origin == crate::domain::DraftOrigin::FollowUp)
                .and_then(|origin| origin.material["source_goal_id"].as_i64());
            let judged = detail
                .membership_judgements
                .iter()
                .filter_map(|row| row["source_goal_id"].as_i64());
            registered
                .into_iter()
                .chain(judged)
                .map(crate::domain::GoalId::new)
        });
        let mut goal_ids: Vec<_> = tasks
            .iter()
            .filter_map(|detail| detail.task.goal_id())
            .chain(proposal.goal_ids().iter().copied())
            .chain(source_goals)
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
        let language = self.verifier.language();
        let origin = self.queue.proposal_origin(proposal.id())?;
        plan_review_prompt(&PlanReviewMaterial {
            proposal,
            origin,
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
            language: language.as_ref(),
        })
    }

    /// The files each task of the proposal and each ready or in-progress
    /// task is expected to touch, by the rule of the claim's deferral
    /// (ADR-0069 decisions 1, 2, ADR-t1981-1): its declared concrete paths
    /// or the files its most related landed tasks changed, and for an
    /// in-progress task also what its run changed. The proposal's tasks are
    /// read afresh, as the planner may have changed their paths.
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
                    // Already expected files and a diff, not declared
                    // paths: deduplicated, not filtered again.
                    files.clear();
                    for file in &run.files {
                        if !files.contains(file) {
                            files.push(file.clone());
                        }
                    }
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
            .related_tasks(task.id(), &[], wanted)?
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
    pub(super) fn poll_plan_review(&mut self) -> Result<bool> {
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
                    &watch.prompt_bytes,
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
                    prompt_bytes: Some(watch.prompt_bytes.clone()),
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
        prompt_bytes: &PromptBytes,
    ) -> Result<()> {
        let proposal = job.proposal_id;
        // A sure concern is applied as its recommendation; the rest wait
        // for a person (ADR-t451-1 decision 4).
        let decided = decide_verdict(proposal, &verdict, revise_count);
        let precedents = quoted_precedents(
            &verdict.precedents,
            &self.queue.answered_asks(usize::MAX >> 1)?,
        );
        let mut revise_reasons = verdict.reasons.clone();
        revise_reasons.extend(precedents.iter().cloned());
        let ask = plan_ask(job, &verdict, &decided, &precedents);
        let PlanVerdictDecision {
            decision,
            overridden,
            concern,
        } = decided;
        let applied = self.queue.finish_plan_review(
            job,
            &self.token,
            &PlanReviewApply {
                verdict: verdict.clone(),
                decision,
                overridden,
                revise_reasons,
                ask,
                concern,
                duration_secs,
                session,
                prompt_bytes: Some(prompt_bytes.clone()),
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
                let next = super::goal_review::again_on(self.fallback.jobs, provider);
                warn!(task_id = %job.anchor, error = %error, "proposal {} plan review {} failed: {error}; {} cannot be used ({}), and the proposal is reviewed again {next}", job.proposal_id, job.attempt, provider.as_str(), reason.as_str());
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
        self.close_person_planners();
        if self.no_claude {
            return Ok(());
        }
        let now = self.generators.clock.now();
        let timeout = i64::try_from(options.planner_timeout.as_secs())?;
        let mut views = self.planner_views()?;
        for revise in self.queue.revising_proposals()? {
            let proposal = revise.proposal.id();
            let gone = |planner_id: PlannerId| {
                views
                    .iter()
                    .find(|view| view.planner.id == planner_id)
                    .is_none_or(|view| self.planner_gone(view))
            };
            match revise_watch(
                (revise.sent_at, revise.planner_id),
                revise.unresponsive_at,
                revise.revised_at,
                gone,
                now,
                timeout,
            ) {
                ReviseWatch::PlannerGone(planner_id) => {
                    info!(
                        "proposal {proposal}: planner {planner_id} is gone before it submitted again; the revise goes to another planner"
                    );
                    self.queue.revise_lost(
                        proposal,
                        Some(planner_id),
                        "its planner is gone before it submitted again",
                    )?;
                }
                ReviseWatch::PlannerSilent { planner_id, waited } => {
                    warn!(
                        "proposal {proposal}: planner {planner_id} did not submit it again within {timeout} seconds; the inbox is told"
                    );
                    self.queue
                        .planner_unresponsive(proposal, Some(planner_id), waited, &[])?;
                }
                ReviseWatch::DeliveryLost => {
                    self.queue
                        .revise_lost(proposal, None, "its delivery was never recorded")?;
                }
                ReviseWatch::NoPlanner { waited } => {
                    let holders = self.planner_holds(&views)?;
                    warn!(
                        "proposal {proposal}: its revise waited {waited} seconds for a planner; the inbox is told"
                    );
                    self.queue
                        .planner_unresponsive(proposal, None, waited, &holders)?;
                }
                ReviseWatch::Nothing => {}
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
                    // One that waits for Claude gets it after its retry
                    // (ADR-t1394-2 decision 5).
                    if view.state != PlannerState::Idle
                        || self.planner_at_wall(view).is_some()
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
                        self.send_to_planner(view, &workspace, Input::Text(&text), "revise")
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
        // The planning requests a person made through the inbox
        // (ADR-t1394-1) come before the runtime's own drafts and findings:
        // a person waits on them.
        self.open_request_planners(&mut runtime_open)?;
        self.open_draft_planners(&mut runtime_open)?;
        // Then the findings marked for a proposal (ADR-0044 decision 19),
        // within the same limit, once those whose proposal ended are
        // settled.
        self.settle_findings()?;
        self.open_finding_planners(&mut runtime_open)?;
        // A headless planner's turn that failed at Claude waits for it, and
        // one its wrapper stopped at its limit tells the inbox
        // (ADR-t1394-2 decisions 3 and 5).
        self.tend_planner_walls(&views)?;
        self.tell_of_stopped_planner_turns(&views)?;
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
                    request_id: view.planner.request_id,
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
            // An answer not sent yet (its delivery failed) is left to the
            // inbox to settle. One that waits for Claude is not done either (ADR-t1394-2
            // decision 5).
            if busy.is_empty()
                || busy.contains(&PlannerBusy::QuestionOpen)
                || busy.contains(&PlannerBusy::AnswerUndelivered)
                || busy.contains(&PlannerBusy::ProviderWall)
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
            self.send_to_planner(view, &workspace, Input::Exit, "exit")?;
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
        if self.planner_at_wall(view).is_some() {
            busy.push(PlannerBusy::ProviderWall);
        }
        if self.planner_exits.iter().any(|(sent, _)| *sent == id) {
            busy.push(PlannerBusy::Exiting);
        }
        Ok(busy)
    }

    /// Whether a planner's session is over: it exited, its wrapper stopped
    /// heartbeating, or its session (its background wrapper, or a person's
    /// workspace) is gone and its wrapper is dead (a session not found
    /// alone is no evidence: it is judged by its wrapper, as a worker's
    /// is). A planner of the runtime's an older binary opened in a
    /// workspace is over: the interactive route is retired (ADR-t1433-2
    /// decision 3) and nothing is sent to it.
    fn planner_gone(&self, view: &PlannerView) -> bool {
        if retired_workspace(&view.planner) {
            return true;
        }
        match view.state {
            PlannerState::Exited | PlannerState::Lost => true,
            PlannerState::Closed => view
                .planner
                .wrapper_pid
                .is_none_or(|pid| view.planner.exited_at.is_some() || !self.processes.alive(pid)),
            _ => false,
        }
    }

    /// The planners not closed, each judged by [`planner_view`].
    pub(super) fn planner_views(&self) -> Result<Vec<PlannerView>> {
        let probes = PlannerProbes {
            sessions: self.sessions,
            processes: &*self.processes,
            files: &*self.files,
            signals: self.signals,
            clock: &*self.generators.clock,
            planners_dir: &self.layout.planners_dir,
        };
        self.queue
            .planners(false)?
            .into_iter()
            .map(|planner| planner_view(&probes, planner))
            .collect()
    }

    /// Stamp a request the supervisor is about to write for the planner of
    /// `view`, so an idle marker from before it no longer counts. A stamp
    /// that cannot be written is logged.
    pub(super) fn stamp_planner_input(&self, view: &PlannerView) {
        if let Err(error) =
            screen_idle::record_supervisor_input(&*self.files, &planner_idle_marker(&view.dir))
        {
            warn!(error = %error, "planner {}: the stamp of the typed text could not be written: {error}", view.planner.id);
        }
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

    /// What opening a planner of the runtime's works with. `[roles]` is
    /// read as each planner opens; an old `[roles.runtime_planner] route`
    /// in it is warned of once per supervisor
    /// ([`Self::warn_ignored_route_setting`]).
    pub(super) fn planner_launch(&self) -> PlannerLaunch<'_> {
        let layout = self.layout;
        let roles = self.verifier.role_models().unwrap_or_else(|error| {
            warn!(error = %format_args!("{error:#}"), "[roles] could not be read; the planner starts as before: {error:#}");
            Default::default()
        });
        self.warn_ignored_route_setting(&roles);
        PlannerLaunch {
            queue: &*self.queue,
            backend: self.sessions,
            files: &*self.files,
            db: &layout.db,
            planners_dir: &layout.planners_dir,
            repo_root: &layout.repo_root,
            runner: &layout.runner,
            claude: &layout.claude,
            plugin_dir: layout.plugin_dir.as_deref(),
            language: self.verifier.language(),
            roles,
            turn_limits: self.stall.turn_limits(),
        }
    }

    /// Warn once per supervisor that `[roles.runtime_planner] route` of
    /// `dagq.toml`, which `roles` was read with, chooses nothing: the key is
    /// accepted and ignored, whatever its value, and the runtime's planners
    /// run headless only (ADR-t1433-2 decision 3, handled as ADR-t1433-3
    /// decision 2 handles `[headless] wrapper`).
    fn warn_ignored_route_setting(&self, roles: &RoleModels) {
        let warned = self.route_setting_warned.load(Ordering::Relaxed);
        if let Some(route) = warns_of_ignored_route(warned, roles)
            && !self.route_setting_warned.swap(true, Ordering::Relaxed)
        {
            warn!(
                "[roles.runtime_planner] route = {route:?} of dagq.toml is ignored: the runtime's planners run headless only, in the background (ADR-t1433-2)"
            );
        }
    }

    /// Close the row of every person's planner still open, without cmux
    /// (ADR-t1433-2 decision 5, [`planner::close_person_planners`]), so a
    /// revise or an answer for what it owned goes to a new planner of the
    /// runtime's. A queue that cannot close one is logged; the next pass
    /// tries again.
    fn close_person_planners(&mut self) {
        match planner::close_person_planners(&*self.queue) {
            Ok(closed) => {
                for id in closed {
                    info!(
                        "planner {id} of a person, opened before dagq plan was abolished, is closed without cmux; a person closes its workspace"
                    );
                }
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the planners of a person could not be closed: {error:#}");
            }
        }
    }

    /// End the runtime's planners that are done: one idle with no revise of
    /// its own left is asked to exit once (the exit request in its
    /// `turns/`, ADR-t1394-2 decision 2), and its background wrapper is
    /// stopped after the exit timeout if it does not; one whose session is
    /// over is given up, its background wrapper stopped if it still runs,
    /// with `planner_closed` (ADR-t1300-1). One an older binary opened in a
    /// workspace is closed without cmux, its workspace left for a person to
    /// close (ADR-t1433-2 decision 3). A person's planner is closed by
    /// [`Self::close_person_planners`] (decision 5).
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
            let overdue = asked.is_some_and(|elapsed| elapsed > self.sessions.exit_timeout());
            if self.planner_gone(view) || overdue {
                if overdue {
                    warn!(
                        "planner {id} of the runtime did not exit within {} seconds of its exit request; its wrapper is stopped",
                        self.sessions.exit_timeout().as_secs()
                    );
                }
                // Only a background wrapper is stopped: the runtime does
                // not close a workspace (ADR-t1433-2 decision 3).
                let mut workspace_closed = false;
                if let Some(handle) = workspace
                    .as_deref()
                    .filter(|handle| crate::domain::background_wrapper::is_background(handle))
                    && self.sessions.exists(handle)?
                {
                    stop_session(self.sessions, handle, StopRoute::Planner)?;
                    workspace_closed = true;
                }
                let (code, reason) = if retired_workspace(&view.planner) {
                    (
                        PlannerCloseCode::RuntimeSessionGone,
                        format!(
                            "planner {id} of the runtime was opened in a workspace by an older binary; the runtime calls no cmux for its planners any more (ADR-t1433-2), so its record is closed and its workspace is left for a person to close"
                        ),
                    )
                } else if overdue {
                    (
                        PlannerCloseCode::RuntimeExitTimedOut,
                        format!(
                            "planner {id} of the runtime did not exit within {} seconds of its exit request",
                            self.sessions.exit_timeout().as_secs()
                        ),
                    )
                } else {
                    // A wrapper in the background is its handle: once it
                    // ended on its agent's exit, the handle is gone too
                    // (ADR-t1404-1 decision 10).
                    let background_exited = view.planner.exited_at.is_some()
                        && workspace
                            .as_deref()
                            .is_some_and(crate::domain::background_wrapper::is_background);
                    match view.state {
                        PlannerState::Exited => (
                            PlannerCloseCode::RuntimeExited,
                            format!("planner {id} of the runtime: its agent exited"),
                        ),
                        PlannerState::Closed if background_exited => (
                            PlannerCloseCode::RuntimeExited,
                            format!(
                                "planner {id} of the runtime: its agent exited and its background wrapper ended"
                            ),
                        ),
                        PlannerState::Lost => (
                            PlannerCloseCode::RuntimeLost,
                            format!(
                                "planner {id} of the runtime: its wrapper is lost or never registered"
                            ),
                        ),
                        _ => (
                            PlannerCloseCode::RuntimeSessionGone,
                            format!(
                                "planner {id} of the runtime: its background wrapper is gone and its wrapper is done"
                            ),
                        ),
                    }
                };
                self.queue.end_planner(
                    id,
                    &planner::planner_closed_payload(
                        &view.planner,
                        code,
                        workspace_closed,
                        &reason,
                    ),
                )?;
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
                self.send_to_planner(view, workspace, Input::Exit, "exit")?;
                self.planner_exits.push((id, Instant::now()));
                info!("planner {id} of the runtime is done; asked it to exit");
            }
        }
        Ok(())
    }
}

/// What a pass makes of a revise on its way to a planner (ADR-0041
/// decisions 12, 13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviseWatch {
    /// The planner it went to is gone before it submitted again: the
    /// revise goes to another planner.
    PlannerGone(PlannerId),
    /// The planner it went to did not submit again within the timeout:
    /// the inbox is told, once.
    PlannerSilent {
        planner_id: PlannerId,
        waited: i64,
    },
    /// A delivery claimed and never recorded (its supervisor died in
    /// between): it is delivered again.
    DeliveryLost,
    /// No planner took it within the timeout: the inbox is told, once.
    NoPlanner {
        waited: i64,
    },
    Nothing,
}

/// What a pass at `now` makes of a revise (`sent`: when its delivery was
/// claimed and the planner it went to; `unresponsive_at`: when the inbox
/// was told; `revised_at`: since when it waits), `planner_gone` judging
/// whether a planner's session is over, with the planner timeout of
/// `timeout` seconds.
fn revise_watch(
    sent: (Option<i64>, Option<PlannerId>),
    unresponsive_at: Option<i64>,
    revised_at: Option<i64>,
    planner_gone: impl Fn(PlannerId) -> bool,
    now: i64,
    timeout: i64,
) -> ReviseWatch {
    match sent {
        (Some(sent_at), Some(planner_id)) => {
            if planner_gone(planner_id) {
                ReviseWatch::PlannerGone(planner_id)
            } else if unresponsive_at.is_none() && now - sent_at > timeout {
                ReviseWatch::PlannerSilent {
                    planner_id,
                    waited: now - sent_at,
                }
            } else {
                ReviseWatch::Nothing
            }
        }
        (Some(sent_at), None) if now - sent_at > HEARTBEAT_TIMEOUT_SECS => {
            ReviseWatch::DeliveryLost
        }
        (None, _)
            if unresponsive_at.is_none() && revised_at.is_some_and(|at| now - at > timeout) =>
        {
            ReviseWatch::NoPlanner {
                waited: now - revised_at.unwrap_or(now),
            }
        }
        _ => ReviseWatch::Nothing,
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
    /// Headless, its last turn failed at Claude's login, usage limit or
    /// start, and it waits for Claude (ADR-t1394-2 decision 5).
    ProviderWall,
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
            Self::ProviderWall => "provider_wall",
        }
    }
}

impl Supervisor<'_> {
    /// Whether, and how, a planner of the runtime's still waits on a
    /// `planner_question` about its draft or finding: one not answered
    /// yet, one answered whose answer is not delivered yet, or one whose
    /// answer was delivered to this planner and not taken up yet (task 884):
    /// a headless planner takes it up once a turn carrying it finished;
    /// without a record of that request, once it stopped after the delivery,
    /// timed by its claim (`planner_answer_claimed`), taken before it, not
    /// by the ask's close after it, so an agent that took the answer up and
    /// stopped before the close is done; an answer a new planner carried in its prompt is timed
    /// by the planner's opening. An ask closed without a delivery holds
    /// nothing.
    fn question_wait(&mut self, view: &PlannerView) -> Result<Option<PlannerBusy>> {
        let (draft, finding, request) = (
            view.planner.draft_task_id,
            view.planner.finding_id,
            view.planner.request_id,
        );
        if draft.is_none() && finding.is_none() && request.is_none() {
            return Ok(None);
        }
        // The drafts of its bundle (ADR-t807-1).
        let drafts = match draft {
            Some(_) => self.queue.planner_draft_tasks(view.planner.id)?,
            None => Vec::new(),
        };
        // A request's planner asks about the request or a draft a planner
        // of it added (ADR-t2015-1), a finding's about the finding, a
        // draft's about the drafts of its bundle.
        let mut asks = Vec::new();
        for ask in self.queue.asks(crate::application::AskQuery {
            all: true,
            ..Default::default()
        })? {
            let its = ask.kind == AskKind::PlannerQuestion
                && match (finding, request) {
                    (_, Some(request)) => {
                        ask.request_id == Some(request)
                            || (ask.request_id.is_none()
                                && ask.finding_id.is_none()
                                && ask.task_id.is_some()
                                // Only what can hold it: not closed, or
                                // delivered to it.
                                && (ask.closed_at.is_none()
                                    || match view.planner.workspace_id.as_deref() {
                                        Some(workspace) => {
                                            self.queue.ask_delivered_to(ask.id, workspace)?
                                        }
                                        None => false,
                                    })
                                && self.queue.answer_request(&ask)? == Some(request))
                    }
                    (Some(finding), None) => ask.finding_id == Some(finding),
                    (None, None) => {
                        ask.finding_id.is_none()
                            && ask.task_id.is_some_and(|task| drafts.contains(&task))
                    }
                };
            if its {
                asks.push(ask);
            }
        }
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
            // A headless planner took the answer up once a turn carrying
            // it finished otherwise than at Claude's wall (the turn of its
            // request, or the provider retry after it): its turns are
            // recorded, and a stub's turn may end within the second the
            // answer was claimed.
            if let Some(taken) = self.headless_answer_taken(view, ask.id)? {
                if !taken {
                    return Ok(Some(PlannerBusy::AnswerTyped));
                }
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

/// Where the next plan review goes, for a role whose `launch` names its
/// provider when `switchable`: a role that names none runs on Claude, waits
/// while the queue's hold ask (`held`) holds it, and goes to a person under
/// `--no-claude`; one that names its provider goes by [`job_route`] (to the
/// other provider when its own cannot be used, `unusable` saying why, and
/// `[provider_fallback] jobs` lets it), and under `--no-claude` to a person
/// told why when it would wait.
fn plan_review_route(
    launch: ActorLaunch,
    switchable: bool,
    no_claude: bool,
    held: bool,
    fallback_jobs: bool,
    unusable: impl Fn(Provider) -> Option<SwitchReason>,
) -> PlanReviewRoute {
    if !switchable {
        if no_claude {
            return PlanReviewRoute::Manual(format!(
                "{PLAN_REVIEW_PROVIDER_DISABLED}; handle this role manually"
            ));
        }
        return if held {
            PlanReviewRoute::Wait
        } else {
            PlanReviewRoute::Start(launch, false)
        };
    }
    match job_route(&launch, true, fallback_jobs, &unusable) {
        JobRoute::Start(launch) => PlanReviewRoute::Start(launch, true),
        JobRoute::Wait { .. } if no_claude => {
            let codex = unusable(Provider::Codex).map_or("unknown", |reason| reason.as_str());
            PlanReviewRoute::Manual(format!(
                "{PLAN_REVIEW_PROVIDER_DISABLED} and codex cannot be used ({codex}); handle this role manually"
            ))
        }
        JobRoute::Wait { provider, reason } => {
            tracing::debug!(
                "the plan review waits: {}",
                job_wait_text(provider, reason, fallback_jobs)
            );
            PlanReviewRoute::Wait
        }
    }
}

/// The lines quoting the asks a verdict names as its precedents, of those
/// `answered`, in the verdict's order; an ask not answered is left out.
fn quoted_precedents(precedents: &[AskId], answered: &[Ask]) -> Vec<String> {
    precedents
        .iter()
        .filter_map(|id| answered.iter().find(|ask| ask.id == *id))
        .map(precedent_line)
        .collect()
}

/// The `approve_plan` ask the job's verdict opens when the runtime's
/// decision is a `concern` (ADR-0041 decision 11), carrying the concern's
/// recommendation, confidence and the reason a person is needed
/// (ADR-t451-1 decision 4); none for any other decision.
fn plan_ask(
    job: &PlanReviewJob,
    verdict: &PlanReviewVerdict,
    decided: &PlanVerdictDecision,
    precedents: &[String],
) -> Option<NewAsk> {
    let concern = decided.concern;
    (decided.decision == PlanReviewDecision::Concern).then(|| NewAsk {
        recommendation: concern
            .and_then(|decided| decided.recommendation)
            .map(|recommended| recommended.as_str().to_owned()),
        confidence: concern.and_then(|decided| decided.confidence),
        kind: AskKind::ApprovePlan,
        task_id: Some(job.anchor),
        run_id: None,
        question: plan_question(
            job,
            verdict,
            decided.overridden.as_deref(),
            concern.as_ref(),
            precedents,
        ),
        options: PLAN_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
        asked_by: PLAN_REVIEW_ASKER.to_owned(),
        reason_category: concern.map_or(AskReason::Scope, |decided| decided.ask_reason()),
        topics: Vec::new(),
        finding_id: None,
        request_id: None,
    })
}

/// The question of the `approve_plan` ask a concern opens.
fn plan_question(
    job: &PlanReviewJob,
    verdict: &PlanReviewVerdict,
    overridden: Option<&str>,
    concern: Option<&PlanConcernDecision>,
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
    if let Some(decided) = concern {
        let recommended = decided
            .recommendation
            .map_or("nothing", PlanRecommendation::as_str);
        let confidence = decided.confidence.map_or("none", AskConfidence::as_str);
        let because = decided
            .escalated_because
            .map_or("", PlanConcernEscalation::as_str);
        question.push_str(&format!(
            "
It recommends {recommended} (confidence {confidence}); left to a person: {because}."
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

/// The value of `[roles.runtime_planner] route` that `roles` was read with,
/// when it calls for the warning that it is ignored (ADR-t1433-2 decision
/// 3): once per supervisor (`warned`), and whatever the value, `headless`
/// included, since no value chooses anything.
fn warns_of_ignored_route(warned: bool, roles: &RoleModels) -> Option<&str> {
    roles.ignored_planner_route().filter(|_| !warned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::MAX_PLAN_REVISES;

    /// Any value of the old key is warned of, and only once; no key is
    /// not.
    #[test]
    fn the_old_route_key_is_warned_of_whatever_its_value_and_only_once() {
        assert_eq!(warns_of_ignored_route(false, &RoleModels::default()), None);
        for value in ["interactive", "headless", "screen"] {
            let mut roles = RoleModels::default();
            roles.ignore_planner_route(value.to_owned());
            assert_eq!(warns_of_ignored_route(false, &roles), Some(value));
            assert_eq!(warns_of_ignored_route(true, &roles), None);
        }
    }

    fn launch(provider: Provider) -> ActorLaunch {
        ActorLaunch {
            provider,
            ..ActorLaunch::default_of(ModelRole::PlanReview)
        }
    }

    /// Where a plan review starts, waits or goes to a person, by the role,
    /// the providers that can be used and `--no-claude` (moved from the
    /// integration tests without_codex_the_plan_review_starts_on_claude,
    /// no_claude_plan_review_waits_for_manual_handling_and_opens_no_planner
    /// and the ok and missing cases of
    /// no_claude_plan_review_runs_on_codex_and_never_falls_back).
    #[test]
    fn a_plan_review_starts_where_its_role_and_the_providers_let_it() {
        let claude = launch(Provider::Claude);
        let codex = launch(Provider::Codex);
        let usable = |_: Provider| None;
        let route = |launch: &ActorLaunch,
                     switchable,
                     no_claude,
                     held,
                     unusable: &dyn Fn(Provider) -> Option<SwitchReason>| {
            match plan_review_route(launch.clone(), switchable, no_claude, held, true, unusable) {
                PlanReviewRoute::Start(launch, switchable) => {
                    format!(
                        "start {} {switchable} {:?}",
                        launch.provider.as_str(),
                        launch.switch_reason
                    )
                }
                PlanReviewRoute::Wait => "wait".to_owned(),
                PlanReviewRoute::Manual(why) => why,
            }
        };
        // A role that names no provider: Claude, waiting while held, a
        // person's under --no-claude whatever the hold.
        assert_eq!(
            route(&claude, false, false, false, &usable),
            "start claude false None"
        );
        assert_eq!(route(&claude, false, false, true, &usable), "wait");
        for held in [false, true] {
            assert_eq!(
                route(&claude, false, true, held, &usable),
                "provider_disabled: Claude is disabled by --no-claude; handle this role manually"
            );
        }
        // One that names Codex starts there, or on Claude when no Codex
        // runs, with why.
        assert_eq!(
            route(&codex, true, false, false, &usable),
            "start codex true None"
        );
        let missing = |provider: Provider| {
            (provider == Provider::Codex).then_some(SwitchReason::ExecutableMissing)
        };
        assert_eq!(
            route(&codex, true, false, false, &missing),
            "start claude true Some(ExecutableMissing)"
        );
        // Under --no-claude it runs on Codex and never moves to Claude.
        let no_claude =
            |provider: Provider| (provider == Provider::Claude).then_some(SwitchReason::Disabled);
        assert_eq!(
            route(&codex, true, true, false, &no_claude),
            "start codex true None"
        );
        for reason in [
            SwitchReason::ExecutableMissing,
            SwitchReason::Authentication,
        ] {
            let neither = move |provider: Provider| match provider {
                Provider::Claude => Some(SwitchReason::Disabled),
                Provider::Codex => Some(reason),
            };
            assert_eq!(
                route(&codex, true, true, false, &neither),
                format!(
                    "provider_disabled: Claude is disabled by --no-claude and codex cannot be used ({}); handle this role manually",
                    reason.as_str()
                )
            );
            // Without --no-claude it waits for one.
            assert_eq!(route(&codex, true, false, false, &neither), "wait");
        }
    }

    fn job() -> PlanReviewJob {
        PlanReviewJob {
            id: 4,
            proposal_id: ProposalId::new(1),
            attempt: 1,
            anchor: TaskId::new(2),
            dir: PathBuf::from("/q/plan-reviews/4"),
            session_id: None,
        }
    }

    /// The `approve_plan` ask of a concern: the recommendation, the
    /// confidence, the reason a person is needed and the question, for
    /// each reason the runtime leaves a concern to a person and for a
    /// revise past the limit (moved from the integration tests
    /// low_scope_discard_and_the_old_shape_wait_for_a_person_with_the_recommendation,
    /// a_sure_send_back_is_a_revise_counted_toward_the_limit_then_a_person_decides,
    /// a_revise_past_the_limit_is_a_concern and
    /// a_concern_asks_the_inbox_and_the_supervisor_applies_the_answers).
    #[test]
    fn a_concern_left_to_a_person_asks_with_its_recommendation_and_why() {
        let concern = |extra: &str| {
            PlanReviewVerdict::parse(&format!(
                r#"{{"verdict":"concern","reasons":["looks already implemented"],"summary":"maybe done"{extra}}}"#
            ))
            .unwrap()
        };
        let cases = [
            (
                r#","recommendation":"ready","confidence":"low""#,
                0,
                Some("ready"),
                Some(AskConfidence::Low),
                AskReason::Scope,
                "It recommends ready (confidence low); left to a person: low_confidence.",
            ),
            (
                r#","recommendation":"ready","confidence":"high","reason_category":"scope""#,
                0,
                Some("ready"),
                Some(AskConfidence::High),
                AskReason::Scope,
                "It recommends ready (confidence high); left to a person: scope.",
            ),
            (
                r#","recommendation":"cancel","confidence":"high","reason_category":"discard""#,
                0,
                Some("cancel"),
                Some(AskConfidence::High),
                AskReason::Discard,
                "It recommends cancel (confidence high); left to a person: discard.",
            ),
            (
                "",
                0,
                None,
                None,
                AskReason::Scope,
                "It recommends nothing (confidence none); left to a person: no_recommendation.",
            ),
            (
                r#","recommendation":"send_back","confidence":"high""#,
                MAX_PLAN_REVISES,
                Some("send_back"),
                Some(AskConfidence::High),
                AskReason::Scope,
                "It recommends send_back (confidence high); left to a person: revise_limit.",
            ),
        ];
        for (extra, revises, recommendation, confidence, reason, says) in cases {
            let verdict = concern(extra);
            let decided = decide_verdict(ProposalId::new(1), &verdict, revises);
            let ask =
                plan_ask(&job(), &verdict, &decided, &[]).unwrap_or_else(|| panic!("{extra}"));
            assert_eq!(ask.kind, AskKind::ApprovePlan);
            assert_eq!(ask.task_id, Some(TaskId::new(2)));
            assert_eq!(ask.options, ["ready", "send_back", "cancel"]);
            assert_eq!(ask.asked_by, "plan_review");
            assert_eq!(ask.recommendation.as_deref(), recommendation, "{extra}");
            assert_eq!(ask.confidence, confidence, "{extra}");
            assert_eq!(ask.reason_category, reason, "{extra}");
            assert!(ask.question.contains(says), "{says} in {}", ask.question);
            assert!(
                ask.question
                    .contains("\nReasons:\n- looks already implemented"),
                "{}",
                ask.question
            );
        }
        // A revise past the limit says why it is a concern.
        let revise = PlanReviewVerdict::parse(
            r#"{"verdict":"revise","reasons":["still vague"],"summary":"vague"}"#,
        )
        .unwrap();
        let decided = decide_verdict(ProposalId::new(1), &revise, MAX_PLAN_REVISES);
        let precedents = ["precedent: ask 3 (blocked) asked: q — a person answered: a".to_owned()];
        let ask = plan_ask(&job(), &revise, &decided, &precedents).unwrap();
        assert_eq!(
            ask.question,
            "Plan review of proposal 1 (its first task is 2) needs a person: vague\n\
             It answered revise, but proposal 1 was sent back 2 times already (at most 2).\n\
             Reasons:\n- still vague\n\
             - precedent: ask 3 (blocked) asked: q — a person answered: a\n\
             Plan review material: /q/plan-reviews/4/prompt.txt\n\
             ready: make the proposal's tasks ready as they are. send_back: send it back to its planner (answer `send_back: <your reason>` to add yours). cancel: cancel the proposal's tasks."
        );
        assert_eq!(ask.reason_category, AskReason::Scope);
        assert_eq!(ask.recommendation, None);
        // A pass, a revise within the limit and a sure concern open none.
        for (text, revises) in [
            (r#"{"verdict":"pass","reasons":[],"summary":"ok"}"#, 0),
            (r#"{"verdict":"revise","reasons":["x"],"summary":"s"}"#, 1),
            (
                r#"{"verdict":"concern","reasons":["x"],"summary":"s","recommendation":"ready","confidence":"high"}"#,
                0,
            ),
        ] {
            let verdict = PlanReviewVerdict::parse(text).unwrap();
            let decided = decide_verdict(ProposalId::new(1), &verdict, revises);
            assert!(
                plan_ask(&job(), &verdict, &decided, &[]).is_none(),
                "{text}"
            );
        }
    }

    /// The precedents a verdict names are quoted from the answered asks,
    /// in its order; one not answered is left out.
    #[test]
    fn a_verdicts_precedents_are_quoted_from_the_answered_asks() {
        let ask = |id: i64, answer: &str| -> Ask {
            serde_json::from_value(serde_json::json!({
                "id": id, "kind": "blocked", "task_id": null, "run_id": null,
                "question": "task 9 changes a type", "options": [], "answer": answer,
                "asked_by": "observer", "reason_category": "scope", "created_at": 1,
                "answered_at": 2, "closed_at": null
            }))
            .unwrap()
        };
        let answered = [ask(3, "drop that line"), ask(5, "keep it")];
        assert_eq!(
            quoted_precedents(&[AskId::new(5), AskId::new(4), AskId::new(3)], &answered),
            [
                "precedent: ask 5 (blocked) asked: task 9 changes a type — a person answered: keep it",
                "precedent: ask 3 (blocked) asked: task 9 changes a type — a person answered: drop that line",
            ]
        );
    }

    /// A revise on its way to a planner: its planner gone, silent past
    /// the timeout (told once), a delivery never recorded, or no planner
    /// taking it past the timeout (told once) (moved from the part of the
    /// integration test a_revise_without_a_live_planner_opens_planners_within_the_limit
    /// that waited past the timeout).
    #[test]
    fn a_revise_is_given_to_another_planner_or_told_to_the_inbox_by_its_times() {
        let planner = PlannerId::new(7);
        let alive = |_: PlannerId| false;
        let gone = |_: PlannerId| true;
        let (now, timeout) = (1_000, 60);
        let watch = |sent, unresponsive, revised, gone: &dyn Fn(PlannerId) -> bool| {
            revise_watch(sent, unresponsive, revised, gone, now, timeout)
        };
        assert_eq!(
            watch((Some(990), Some(planner)), None, Some(980), &gone),
            ReviseWatch::PlannerGone(planner)
        );
        assert_eq!(
            watch((Some(990), Some(planner)), None, Some(980), &alive),
            ReviseWatch::Nothing
        );
        assert_eq!(
            watch((Some(900), Some(planner)), None, Some(900), &alive),
            ReviseWatch::PlannerSilent {
                planner_id: planner,
                waited: 100
            }
        );
        assert_eq!(
            watch((Some(900), Some(planner)), Some(990), Some(900), &alive),
            ReviseWatch::Nothing
        );
        assert_eq!(
            watch(
                (Some(now - HEARTBEAT_TIMEOUT_SECS - 1), None),
                None,
                Some(0),
                &alive
            ),
            ReviseWatch::DeliveryLost
        );
        assert_eq!(
            watch(
                (Some(now - HEARTBEAT_TIMEOUT_SECS), None),
                None,
                Some(0),
                &alive
            ),
            ReviseWatch::Nothing
        );
        assert_eq!(
            watch((None, None), None, Some(939), &alive),
            ReviseWatch::NoPlanner { waited: 61 }
        );
        assert_eq!(
            watch((None, None), None, Some(940), &alive),
            ReviseWatch::Nothing
        );
        assert_eq!(
            watch((None, None), Some(990), Some(900), &alive),
            ReviseWatch::Nothing
        );
        assert_eq!(
            watch((None, None), None, None, &alive),
            ReviseWatch::Nothing
        );
    }
}
