//! Goal review (ADR-0047 decision 43): the supervisor takes the open goals
//! whose tasks all ended one at a time through a headless job, applies its
//! verdict (closes the goal as achieved, registers its gaps as drafts of
//! the goal, or asks the inbox) and applies a person's answer to its
//! `approve_goal` ask. None of this takes a run slot; a failure is logged
//! and tried again on a later pass.

use super::*;
use crate::domain::ActorContext;
use crate::{
    application::{
        GoalReviewApply, GoalReviewFailure, GoalReviewJob, job_start_failure,
        prompt::{
            FittedPrompt, GOAL_REVIEW_ACCESS, GoalReviewMaterial, PromptBytes, goal_review_prompt,
        },
    },
    domain::{
        GoalId, Receipt, RunStatus,
        actor_model::{ActorLaunch, JobRoute, ModelRole, job_route, job_wait_text},
        goal_review::{
            GOAL_OPTIONS, GOAL_REVIEW_ASKER, GoalReviewDecision, GoalReviewVerdict, decide,
        },
        headless_job::{JobFailure, JobSession},
        provider_switch::SwitchReason,
    },
};
use serde_json::{Value, json};

/// The goal events the goal review is shown: notes, edits, and what was
/// decided about the goal before.
const GOAL_REVIEW_EVENTS: &[&str] = &[
    "observation",
    event_kind::GOAL_UPDATED,
    event_kind::GOAL_DECIDED,
    event_kind::GOAL_REVIEW_REARMED,
];

/// The goal review job running now: one at a time, queue-wide.
pub(super) struct GoalReviewWatch {
    pub(super) job: GoalReviewJob,
    pub(super) headless: HeadlessJob,
    /// Whether its role names its provider, so that a provider that cannot
    /// be used moves it (ADR-t1063-1 decision 4).
    pub(super) switchable: bool,
    /// What its prompt takes (task 1571), recorded on its end.
    pub(super) prompt_bytes: PromptBytes,
}

impl Supervisor<'_> {
    /// One pass of goal review: apply the answers, reap the job and apply
    /// its verdict, and start the next one unless the loop is draining.
    pub(super) fn goal_review_pass(&mut self, starting: bool) -> bool {
        let mut progressed = false;
        for (what, result) in [
            ("apply the goal answers", self.apply_goal_answers()),
            ("reap the goal review", self.poll_goal_review()),
        ] {
            match result {
                Ok(done) => progressed |= done,
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "goal review: could not {what}: {error:#}")
                }
            }
        }
        // A job reaped above may have hit a login or the usage limit and
        // raised the hold in this pass (task 438): the route reads it.
        if starting && let Err(error) = self.start_goal_review() {
            warn!(error = %format_args!("{error:#}"), "goal review: could not start a goal review: {error:#}");
        }
        progressed
    }

    /// Where the next goal review starts (ADR-t1063-1 decisions 1, 4 and
    /// 5), or `None` while it waits. A role that names no provider runs on
    /// Claude as before, waiting while the queue's hold ask holds Claude;
    /// one that names its provider starts there when it can be used (this
    /// supervisor has it and it is not held), else on the other provider
    /// when that one runs the role and can be used (unless
    /// `[provider_fallback] jobs` is off, ADR-t1857-1), else waits.
    fn goal_review_route(&self) -> Option<(ActorLaunch, bool)> {
        let role = ModelRole::GoalReview;
        let models = self.role_models(role);
        goal_review_route_of(
            models.launch(role),
            (models.switchable(role), self.provider.fallback.jobs),
            self.no_claude || self.queue_hold.is_some(),
            |provider| self.job_unusable(provider),
        )
    }

    /// Why a headless job cannot start on `provider` now: this supervisor
    /// has no agent for it (no Codex found that runs), or it is held.
    pub(super) fn job_unusable(&self, provider: Provider) -> Option<SwitchReason> {
        job_unusable_of(
            self.provider_held(provider),
            self.job_agent(provider).is_some(),
        )
    }

    /// Start the goal review of the first candidate, when none runs.
    fn start_goal_review(&mut self) -> Result<()> {
        if self.goal_review.is_some() {
            return Ok(());
        }
        let Some((launch, switchable)) = self.goal_review_route() else {
            return Ok(());
        };
        let Some(&goal) = self.queue.goal_review_candidates()?.first() else {
            return Ok(());
        };
        let Some(job) = self.queue.begin_goal_review(
            goal,
            &self.token,
            &self.layout.goal_reviews_dir,
            &self.layout.repo_root,
            &launch,
        )?
        else {
            return Ok(());
        };
        match self.spawn_goal_review(&job, &launch) {
            Ok((prompt_bytes, Ok(headless))) => {
                info!(
                    "goal {goal} goal review {} started on {} with a prompt of {} bytes",
                    job.attempt,
                    launch.provider.as_str(),
                    prompt_bytes.total
                );
                self.goal_review = Some(GoalReviewWatch {
                    job,
                    headless,
                    switchable,
                    prompt_bytes,
                });
            }
            // Its own preparation failed: no provider was tried.
            Err(failed) => {
                let error = format!("the headless goal review could not start: {failed:#}");
                self.fail_goal_review(
                    &job,
                    &GoalReviewFailure {
                        error,
                        ..GoalReviewFailure::default()
                    },
                );
            }
            Ok((prompt_bytes, Err(failed))) => {
                let error = format!("the headless goal review could not start: {failed:#}");
                let unusable = self.job_provider_failed(
                    launch.provider,
                    job_start_failure(&failed),
                    (&error, &error),
                    &HoldJob::GoalReview(goal),
                    switchable,
                );
                self.fail_goal_review(
                    &job,
                    &GoalReviewFailure {
                        error,
                        unusable,
                        prompt_bytes: Some(prompt_bytes),
                        ..GoalReviewFailure::default()
                    },
                );
            }
        }
        Ok(())
    }

    /// A headless job of `provider` that failed with `failure` (its start
    /// or its output): Claude's login or usage limit raises the queue's
    /// hold ask as before (task 438), and, for a role that names its
    /// provider (`switchable`), a provider that cannot be used for another
    /// reason is held like a worker's (Codex's walls and any agent that
    /// did not start, ADR-t1063-1 decision 5). The provider and why, when
    /// the job moves to the other provider (ADR-t1063-1 decision 4), or,
    /// with `[provider_fallback] jobs` off, waits for this one to be
    /// usable again (ADR-t1857-1).
    /// `error` is the job's failure, `said` it with the job's output,
    /// which may say when a usage limit resets.
    pub(super) fn job_provider_failed(
        &mut self,
        provider: Provider,
        failure: JobFailure,
        (error, said): (&str, &str),
        job: &HoldJob,
        switchable: bool,
    ) -> Option<(Provider, SwitchReason)> {
        let (wall, unusable) = job_failure_route(provider, failure, switchable);
        if let Some(wall) = wall {
            self.raise_job_wall(wall, job, error);
        }
        let (reason, hold) = unusable?;
        if hold && let Err(held) = self.hold_provider(provider, reason, None, said) {
            warn!(error = %format_args!("{held:#}"), "{} could not be held after the headless {} failed: {held:#}", provider.as_str(), job.entry());
        }
        Some((provider, reason))
    }

    /// Write the prompt into the job's directory and start the headless
    /// job in the repository's checkout, allowed to read only.
    /// The outer error is one of the job's own preparation (its directory,
    /// its prompt), the inner one the start of its provider's process: only
    /// the latter says whether the provider can be used.
    fn spawn_goal_review(
        &mut self,
        job: &GoalReviewJob,
        launch: &ActorLaunch,
    ) -> Result<(PromptBytes, Result<HeadlessJob>)> {
        self.files
            .create_dir_all(&job.dir)
            .with_context(|| format!("create {}", job.dir.display()))?;
        let prompt = self.goal_review_material(job)?;
        self.files
            .write(&job.dir.join("prompt.txt"), prompt.text.as_bytes())?;
        let stdout = job.dir.join("review.out");
        let stderr = job.dir.join("review.err");
        let started = self.start_goal_review_job(job, launch, &prompt.text, stdout, stderr);
        Ok((prompt.bytes, started))
    }

    /// Start the provider's process of the goal review.
    fn start_goal_review_job(
        &mut self,
        job: &GoalReviewJob,
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
                ActorContext::goal_review_job(job.goal_id, job.attempt),
                WorkspaceAccess::Read(self.layout.repo_root.clone()),
                ActorProgram::Headless {
                    program: HeadlessProgram::Job {
                        cwd: &self.layout.repo_root,
                        prompt,
                        access: GOAL_REVIEW_ACCESS,
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
            .context("start the goal review")?
            .process()?;
        Ok(self.headless_job(
            "goal review",
            child,
            stdout,
            stderr,
            JobSubject {
                kind: headless_job::GOAL_REVIEW,
                job: headless_job::JobKind::Agent,
                review_stage: false,
                label: None,
                run_id: None,
                proposal_id: None,
                goal_id: Some(job.goal_id),
                attempt: job.attempt,
                provider: launch.provider,
            },
        ))
    }

    /// The goal review prompt of the job's goal from the queue as it is
    /// now, held to its limits (task 1571).
    fn goal_review_material(&mut self, job: &GoalReviewJob) -> Result<FittedPrompt> {
        let detail = self.queue.show_goal(job.goal_id)?;
        let mut tasks = Vec::new();
        for task in &detail.tasks {
            let shown = self.queue.show(task.id)?;
            let mut value = json!({
                "id": shown.task.id(),
                "title": shown.task.title(),
                "status": shown.task.status(),
                "description": shown.task.description(),
                "acceptance": shown.task.acceptance(),
                "duplicate_of": shown.duplicate_of,
            });
            if let Some(run) = shown
                .runs
                .iter()
                .rev()
                .find(|run| run.status() == RunStatus::Integrated)
            {
                value["landed"] = self.landed(run);
            }
            tasks.push(value);
        }
        let events = detail
            .events
            .iter()
            .filter(|event| GOAL_REVIEW_EVENTS.contains(&event.kind.as_str()))
            .map(|event| json!({"kind": event.kind, "at": event.created_at, "payload": event.payload}))
            .collect();
        let previous = self
            .queue
            .goal_reviews(job.goal_id)?
            .into_iter()
            .map(|review| serde_json::to_value(review).map_err(Into::into))
            .collect::<Result<Vec<Value>>>()?;
        Ok(goal_review_prompt(&GoalReviewMaterial {
            goal: serde_json::to_value(&detail.goal)?,
            tasks,
            follow_ups: detail.follow_up_memberships.clone(),
            events,
            previous,
            gaps_in_a_row: job.gaps_in_a_row,
            repo_root: &self.layout.repo_root,
        })
        .with_language(self.verifier.language().as_ref()))
    }

    /// What landed for a task: its integrated run, the commit and what
    /// its receipt says, or that the receipt could not be read.
    fn landed(&self, run: &TaskRun) -> Value {
        let mut landed = json!({
            "run_id": run.id(),
            "result_commit": run.result_commit(),
        });
        let receipt = run
            .receipt_path()
            .map(PathBuf::from)
            .or_else(|| run.run_dir().map(|dir| Path::new(dir).join("receipt.json")))
            .and_then(|path| self.files.read_to_string(&path).ok())
            .and_then(|text| Receipt::parse(&text).ok());
        match receipt {
            Some(receipt) => {
                let check = |check: &crate::domain::ReceiptCheck| json!({"status": check.status(), "evidence_or_reason": check.evidence_or_reason()});
                landed["summary"] = json!(receipt.summary());
                landed["tests"] = check(receipt.tests());
                landed["e2e"] = check(receipt.e2e());
                landed["subagent_review"] = check(receipt.subagent_review());
                landed["follow_ups"] = receipt.follow_ups().cloned().unwrap_or(Value::Null);
            }
            None => landed["summary"] = json!("(receipt unavailable)"),
        }
        landed
    }

    /// Reap the job once it ended and apply its verdict, or record its
    /// failure.
    pub(super) fn poll_goal_review(&mut self) -> Result<bool> {
        let Some(provider) = self.goal_review.as_ref().map(|w| w.headless.provider) else {
            return Ok(false);
        };
        // The job's reply, session and failure are read by the provider
        // it ran on (ADR-t1063-1 decisions 2, 4 and 6).
        let agent = self.job_agent(provider).unwrap_or(self.reviewer);
        let watch = self.goal_review.as_mut().expect("read above");
        let Some(outcome) = watch.headless.poll(&*self.files, agent)? else {
            return Ok(false);
        };
        let watch = self.goal_review.take().expect("polled above");
        let duration_secs = watch.headless.started.elapsed().as_secs();
        let stdout = self
            .files
            .read_to_string(&watch.headless.stdout)
            .unwrap_or_default();
        let session = agent.job_session(&stdout, watch.headless.started_at);
        let verdict = outcome.and_then(|stdout| GoalReviewVerdict::parse(&stdout));
        // Only a job that failed or printed no verdict is read for a wall:
        // a verdict's own text may quote anything (task 438).
        let failure = verdict.is_err().then(|| self.job_failure(&watch.headless));
        let applied = verdict.map_err(|error| anyhow!(error)).and_then(|verdict| {
            let job = ActorContext::goal_review_job(watch.job.goal_id, watch.job.attempt);
            self.for_job(&job, |sv| {
                sv.apply_goal_verdict(
                    &watch.job,
                    verdict,
                    duration_secs,
                    session.clone(),
                    &watch.prompt_bytes,
                )
            })
        });
        if let Err(error) = applied {
            let error = format!("{error:#}");
            // Stopped at a wall only a person moves: it joins the hold ask,
            // whose `done` rearms the goal review, and its failure is no
            // attention meanwhile (task 438). A role that names its
            // provider moves to the other one instead of waiting.
            // The provider's own words (Codex's `turn.failed` is on its
            // stdout) may say when a usage limit resets.
            let said = format!("{error}\n{stdout}");
            let unusable = failure.and_then(|failure| {
                self.job_provider_failed(
                    watch.headless.provider,
                    failure,
                    (&error, &said),
                    &HoldJob::GoalReview(watch.job.goal_id),
                    watch.switchable,
                )
            });
            self.fail_goal_review(
                &watch.job,
                &GoalReviewFailure {
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

    /// What the runtime makes of the verdict (a `gaps` past the limit in a
    /// row is an `ask`), applied in one transaction; the inbox is told of
    /// a new `approve_goal` ask.
    fn apply_goal_verdict(
        &mut self,
        job: &GoalReviewJob,
        verdict: GoalReviewVerdict,
        duration_secs: u64,
        session: Option<JobSession>,
        prompt_bytes: &PromptBytes,
    ) -> Result<()> {
        let goal = job.goal_id;
        let (decision, overridden) = decide(verdict.verdict, job.gaps_in_a_row);
        let ask = (decision == GoalReviewDecision::Ask).then(|| NewAsk {
            recommendation: None,
            confidence: None,
            kind: AskKind::ApproveGoal,
            task_id: Some(job.anchor),
            run_id: None,
            question: goal_question(goal, &verdict, overridden.as_deref()),
            options: goal_options(&verdict),
            asked_by: GOAL_REVIEW_ASKER.to_owned(),
            reason_category: match verdict.reason_category {
                Some(AskReason::Discard) => AskReason::Discard,
                _ => AskReason::Scope,
            },
            topics: Vec::new(),
            finding_id: None,
            request_id: None,
        });
        let applied = self.queue.finish_goal_review(
            job,
            &self.token,
            &GoalReviewApply {
                verdict: verdict.clone(),
                decision,
                overridden,
                ask,
                duration_secs,
                session,
                prompt_bytes: Some(prompt_bytes.clone()),
            },
        )?;
        if applied.stale {
            info!(
                "goal {goal} or its tasks changed during its goal review; the verdict is not applied"
            );
            return Ok(());
        }
        info!(
            "goal {goal} goal review {}: {} ({})",
            job.attempt,
            decision.as_str(),
            verdict.summary
        );
        for task in &applied.gap_tasks {
            info!(task_id = %task, "goal {goal}: gap registered as draft task {task}");
        }
        Ok(())
    }

    /// Record the job's failure; the goal waits for a person
    /// (`goal_review_failed`) and is not reviewed again by itself until its
    /// tasks change, unless its provider could not be used and it moves
    /// (ADR-t1063-1 decision 4).
    fn fail_goal_review(&mut self, job: &GoalReviewJob, failure: &GoalReviewFailure) {
        let error = &failure.error;
        match failure.unusable {
            Some((provider, reason)) => {
                let next = again_on(self.provider.fallback.jobs, provider);
                warn!(error = %error, "goal {} goal review {} failed: {error}; {} cannot be used ({}), and the goal is reviewed again {next}", job.goal_id, job.attempt, provider.as_str(), reason.as_str());
            }
            None => {
                warn!(error = %error, "goal {} goal review {} failed: {error}; it waits for a person", job.goal_id, job.attempt);
            }
        }
        if let Err(recorded) = self.queue.fail_goal_review(job, &self.token, failure) {
            warn!(error = %format_args!("{recorded:#}"), "goal {}: the goal review failure could not be recorded: {recorded:#}", job.goal_id);
        }
    }

    /// Apply the answered `approve_goal` asks (ADR-0047 decision 43) and
    /// `correct_goal` asks (ADR-t1504-2 decision 9).
    fn apply_goal_answers(&mut self) -> Result<bool> {
        let mut applied = false;
        for answered in self.queue.goal_answers()? {
            match self.queue.decide_goal(answered.id) {
                Ok(Some(decided)) => {
                    applied = true;
                    info!(ask_id = %answered.id, "goal {}: ask {} answered {} applied", decided.goal_id, answered.id, decided.answer)
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(ask_id = %answered.id, error = %format_args!("{error:#}"), "the answer of ask {} could not be applied: {error:#}", answered.id)
                }
            }
        }
        // A person's answer about a goal closed as achieved whose
        // follow-up was judged required after it (ADR-t1504-2 decision 9).
        for answered in self.queue.correction_answers()? {
            match self.queue.decide_correction(answered.id) {
                Ok(Some(decided)) => {
                    applied = true;
                    info!(ask_id = %answered.id, "goal {}: ask {} answered {} applied", decided["goal_id"], answered.id, decided["decision"])
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(ask_id = %answered.id, error = %format_args!("{error:#}"), "the answer of ask {} could not be applied: {error:#}", answered.id)
                }
            }
        }
        Ok(applied)
    }
}

/// Where the next goal review starts (ADR-t1063-1 decisions 1, 4 and 5),
/// or `None` while it waits, given its role's `launch`, whether the role
/// names its provider (`switchable`) and `[provider_fallback] jobs`
/// (`fallback`), whether Claude waits for the queue's hold ask or
/// `--no-claude` (`claude_waits`) and why each provider cannot be used now
/// (`unusable`). A role that names no provider runs on Claude as before,
/// waiting while Claude waits; one that names its provider starts there
/// when it can be used, else on the other provider when that one runs the
/// role and can be used and the fallback is on (ADR-t1857-1), else waits.
fn goal_review_route_of(
    launch: ActorLaunch,
    (switchable, fallback): (bool, bool),
    claude_waits: bool,
    unusable: impl Fn(Provider) -> Option<SwitchReason>,
) -> Option<(ActorLaunch, bool)> {
    if !switchable {
        return (!claude_waits).then_some((launch, false));
    }
    match job_route(&launch, true, fallback, unusable) {
        JobRoute::Start(launch) => Some((launch, true)),
        JobRoute::Wait { provider, reason } => {
            tracing::debug!(
                "the goal review waits: {}",
                job_wait_text(provider, reason, fallback)
            );
            None
        }
    }
}

/// Why a headless job cannot start on a provider `held` for that reason,
/// if it is, and whose agent this supervisor has (`has_agent`): its hold,
/// else no agent for it (no Codex found that runs).
pub(super) fn job_unusable_of(held: Option<SwitchReason>, has_agent: bool) -> Option<SwitchReason> {
    held.or((!has_agent).then_some(SwitchReason::ExecutableMissing))
}

/// Where a job whose `provider` could not be used is started again, as a
/// log line says it: on the other provider, or, with `[provider_fallback]
/// jobs` off (`fallback` false), on `provider` once its hold ends
/// (ADR-t1857-1).
pub(super) fn again_on(fallback: bool, provider: Provider) -> String {
    if fallback {
        "on the other provider".to_owned()
    } else {
        format!(
            "on {} once its hold ends ([provider_fallback] jobs is false)",
            provider.as_str()
        )
    }
}

/// What a headless job of `provider` that failed with `failure` leads to
/// (task 438, ADR-t1063-1 decisions 4 and 5): the wall Claude's job raises
/// the queue's hold ask for, and, for a role that names its provider
/// (`switchable`), why `provider` cannot be used and whether it is held for
/// that (not when the hold ask holds it already). `[provider_fallback] jobs`
/// (ADR-t1857-1) does not change either: whether the job moves to the other
/// provider or waits for `provider` is the caller's route.
pub(super) fn job_failure_route(
    provider: Provider,
    failure: JobFailure,
    switchable: bool,
) -> (
    Option<crate::domain::queue_hold::Wall>,
    Option<(SwitchReason, bool)>,
) {
    let wall = failure.wall().filter(|_| provider == Provider::Claude);
    let unusable = failure
        .switch_reason()
        .filter(|_| switchable)
        .map(|reason| (reason, wall.is_none()));
    (wall, unusable)
}

/// The question of the `approve_goal` ask: the job's question (its summary
/// when blank), why the runtime asks instead of registering gaps, the
/// acceptance items not met, and the gaps it found.
fn goal_question(goal: GoalId, verdict: &GoalReviewVerdict, overridden: Option<&str>) -> String {
    let asked = if verdict.question.trim().is_empty() {
        verdict.summary.trim()
    } else {
        verdict.question.trim()
    };
    let mut question = format!("Goal {goal}: {}", or_none(asked));
    if let Some(why) = overridden {
        question.push_str(&format!("\n{why}; a person decides the goal."));
    }
    let unmet: Vec<&str> = verdict
        .criteria
        .iter()
        .filter(|criterion| !criterion.met)
        .map(|criterion| criterion.criterion.as_str())
        .collect();
    if !unmet.is_empty() {
        question.push_str("\nNot met:");
        for criterion in unmet {
            question.push_str(&format!("\n- {criterion}"));
        }
    }
    if !verdict.gaps.is_empty() {
        question.push_str("\nGaps the review found (answer `gaps` to register them):");
        for gap in &verdict.gaps {
            question.push_str(&format!("\n- {}", gap.title));
        }
    }
    question
}

/// [`GOAL_OPTIONS`] and the job's own options, each once.
fn goal_options(verdict: &GoalReviewVerdict) -> Vec<String> {
    let mut options: Vec<String> = GOAL_OPTIONS.iter().map(|o| (*o).to_owned()).collect();
    for option in &verdict.options {
        let option = option.trim();
        if !option.is_empty() && !options.iter().any(|o| o == option) {
            options.push(option.to_owned());
        }
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::goal_review::{GoalCriterion, GoalGap};

    fn verdict() -> GoalReviewVerdict {
        GoalReviewVerdict {
            verdict: GoalReviewDecision::Ask,
            criteria: vec![
                GoalCriterion {
                    criterion: "(1) docs".into(),
                    met: false,
                    evidence: Vec::new(),
                },
                GoalCriterion {
                    criterion: "(2) code".into(),
                    met: true,
                    evidence: Vec::new(),
                },
            ],
            gaps: vec![GoalGap {
                title: "write the docs".into(),
                description: String::new(),
                criterion: "(1) docs".into(),
            }],
            summary: "docs are missing".into(),
            question: String::new(),
            options: vec!["split".into(), "achieved".into(), " ".into()],
            reason_category: None,
        }
    }

    #[test]
    fn the_question_names_what_is_not_met() {
        let question = goal_question(GoalId::new(7), &verdict(), Some("gaps 3 times"));
        assert!(question.starts_with("Goal 7: docs are missing"));
        assert!(question.contains("gaps 3 times; a person decides"));
        assert!(question.contains("- (1) docs"));
        assert!(!question.contains("(2) code"));
        assert!(question.contains("- write the docs"));
    }

    #[test]
    fn options_add_the_jobs_own_once() {
        assert_eq!(
            goal_options(&verdict()),
            ["achieved", "abandoned", "gaps", "keep_open", "split"]
        );
    }

    /// Where the goal review starts, from the supervisor's state as values
    /// (ADR-t1063-1 decisions 1, 4 and 5): `--no-claude`, the queue's hold
    /// ask on Claude, the providers' own holds and whether a Codex runs.
    struct Providers {
        no_claude: bool,
        fallback: bool,
        queue_hold: Option<SwitchReason>,
        codex_hold: Option<SwitchReason>,
        codex: bool,
    }

    impl Providers {
        const READY: Self = Self {
            no_claude: false,
            fallback: true,
            queue_hold: None,
            codex_hold: None,
            codex: true,
        };

        fn route(&self, table: Option<Provider>) -> Option<ActorLaunch> {
            use crate::application::supervise::provider::provider_held_of;
            use crate::domain::actor_model::RoleModels;
            let mut models = RoleModels::default();
            if let Some(provider) = table {
                models.entry(ModelRole::GoalReview).provider = Some(provider);
            }
            let unusable = |provider| {
                let own = match provider {
                    Provider::Claude => None,
                    Provider::Codex => self.codex_hold,
                };
                job_unusable_of(
                    provider_held_of(provider, self.no_claude, self.queue_hold, own),
                    match provider {
                        Provider::Claude => !self.no_claude,
                        Provider::Codex => self.codex,
                    },
                )
            };
            goal_review_route_of(
                models.launch(ModelRole::GoalReview),
                (models.switchable(ModelRole::GoalReview), self.fallback),
                self.no_claude || self.queue_hold.is_some(),
                unusable,
            )
            .map(|(launch, switchable)| {
                assert_eq!(switchable, table.is_some());
                launch
            })
        }
    }

    fn moved(launch: &ActorLaunch) -> (Provider, Option<Provider>, Option<SwitchReason>) {
        (launch.provider, launch.switched_from, launch.switch_reason)
    }

    #[test]
    fn the_goal_review_starts_on_a_provider_it_can_use_or_waits() {
        use Provider::{Claude, Codex};
        use SwitchReason::*;
        // A role that names no provider runs on Claude as before, and
        // waits while Claude is held or disabled.
        let ready = Providers::READY;
        assert_eq!(
            ready.route(None),
            Some(ActorLaunch::default_of(ModelRole::GoalReview))
        );
        let claude_held = Providers {
            queue_hold: Some(UsageLimit),
            ..Providers::READY
        };
        assert_eq!(claude_held.route(None), None);
        // Without a Codex that runs, a Codex goal review starts on Claude.
        let no_codex = Providers {
            codex: false,
            ..Providers::READY
        };
        assert_eq!(
            moved(&no_codex.route(Some(Codex)).unwrap()),
            (Claude, Some(Codex), Some(ExecutableMissing))
        );
        // A Codex held for its login moves it to Claude.
        let codex_held = Providers {
            codex_hold: Some(Authentication),
            ..Providers::READY
        };
        assert_eq!(
            moved(&codex_held.route(Some(Codex)).unwrap()),
            (Claude, Some(Codex), Some(Authentication))
        );
        // Claude's open hold ask does not stop a role that names Claude
        // once Codex can take it.
        assert_eq!(
            moved(&claude_held.route(Some(Claude)).unwrap()),
            (Codex, Some(Claude), Some(UsageLimit))
        );
        // Both held: it waits.
        let both = Providers {
            queue_hold: Some(UsageLimit),
            codex_hold: Some(UsageLimit),
            ..Providers::READY
        };
        assert_eq!(both.route(Some(Claude)), None);
        // `--no-claude`: on Codex, never back to Claude.
        let no_claude = Providers {
            no_claude: true,
            ..Providers::READY
        };
        assert_eq!(
            moved(&no_claude.route(Some(Codex)).unwrap()),
            (Codex, None, None)
        );
        for codex in [
            Providers {
                codex_hold: Some(Authentication),
                ..no_claude
            },
            Providers {
                codex: false,
                ..no_claude
            },
        ] {
            assert_eq!(codex.route(Some(Codex)), None);
        }
        assert_eq!(no_claude.route(None), None);
    }

    /// With `[provider_fallback] jobs` off a goal review whose role names
    /// its provider waits for that provider when it cannot be used, and
    /// starts there once it can; `--no-claude` still sends a Claude one to
    /// Codex (ADR-t1857-1).
    #[test]
    fn with_the_fallback_off_the_goal_review_waits_for_its_provider() {
        use Provider::{Claude, Codex};
        use SwitchReason::*;
        let off = Providers {
            fallback: false,
            ..Providers::READY
        };
        // Codex held or missing: it waits instead of moving to Claude.
        for codex in [
            Providers {
                codex_hold: Some(UsageLimit),
                ..off
            },
            Providers {
                codex_hold: Some(LaunchFailed),
                ..off
            },
            Providers {
                codex: false,
                ..off
            },
        ] {
            assert_eq!(codex.route(Some(Codex)), None);
        }
        // Claude held by its hold ask: it waits instead of moving to Codex.
        let claude_held = Providers {
            queue_hold: Some(Authentication),
            ..off
        };
        assert_eq!(claude_held.route(Some(Claude)), None);
        // Once the hold ends it starts on its own provider.
        assert_eq!(moved(&off.route(Some(Codex)).unwrap()), (Codex, None, None));
        assert_eq!(
            moved(&off.route(Some(Claude)).unwrap()),
            (Claude, None, None)
        );
        // `--no-claude` still sends a Claude role to Codex.
        let no_claude = Providers {
            no_claude: true,
            ..off
        };
        assert_eq!(
            moved(&no_claude.route(Some(Claude)).unwrap()),
            (Codex, Some(Claude), Some(Disabled))
        );
        // A role that names no provider is as before.
        assert_eq!(
            off.route(None),
            Some(ActorLaunch::default_of(ModelRole::GoalReview))
        );
    }

    /// A failed job's provider: Claude's login or usage limit raises the
    /// queue's hold ask, and a role that names its provider holds a
    /// provider not held by the ask; Codex raises no ask.
    #[test]
    fn a_failed_job_raises_the_hold_ask_or_holds_its_provider() {
        use crate::domain::queue_hold::Wall;
        use Provider::{Claude, Codex};
        assert_eq!(
            job_failure_route(Claude, JobFailure::UsageLimit, true),
            (
                Some(Wall::UsageLimit),
                Some((SwitchReason::UsageLimit, false))
            )
        );
        assert_eq!(
            job_failure_route(Claude, JobFailure::Authentication, false),
            (Some(Wall::Authentication), None)
        );
        assert_eq!(
            job_failure_route(Claude, JobFailure::LaunchFailed, true),
            (None, Some((SwitchReason::LaunchFailed, true)))
        );
        assert_eq!(
            job_failure_route(Codex, JobFailure::Authentication, true),
            (None, Some((SwitchReason::Authentication, true)))
        );
        assert_eq!(
            job_failure_route(Codex, JobFailure::UsageLimit, false),
            (None, None)
        );
        assert_eq!(
            job_failure_route(Claude, JobFailure::Other, true),
            (None, None)
        );
    }

    /// `[roles.goal_review]` gives the job its model and effort, recorded
    /// with `dagq.toml` as the source (ADR-0079 decision 7).
    #[test]
    fn a_role_table_gives_the_goal_review_its_model_and_effort() {
        use crate::domain::actor_model::RoleModels;
        let mut models = RoleModels::default();
        models.entry(ModelRole::GoalReview).model = Some("claude-sonnet-5".into());
        models.entry(ModelRole::GoalReview).effort = Some("high".into());
        let launch = models.launch(ModelRole::GoalReview);
        assert_eq!(launch.arguments(), Some(("claude-sonnet-5", "high")));
        assert_eq!(
            launch.to_value(),
            json!({"role": "goal_review", "provider": "claude", "model": "claude-sonnet-5",
                   "effort": "high", "source": "dagq.toml"})
        );
        assert!(!models.switchable(ModelRole::GoalReview));
    }
}
