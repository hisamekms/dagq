//! Goal review (ADR-0047 decision 43): the supervisor takes the open goals
//! whose tasks all ended one at a time through a headless job, applies its
//! verdict (closes the goal as achieved, registers its gaps as drafts of
//! the goal, or asks the inbox) and applies a person's answer to its
//! `approve_goal` ask. None of this takes a run slot; a failure is logged
//! and tried again on a later pass.

use super::*;
use crate::domain::ActorContext;
use crate::domain::language::with_instruction;
use crate::{
    application::{
        GoalReviewApply, GoalReviewJob,
        prompt::{GoalReviewMaterial, PLAN_REVIEW_TOOLS, goal_review_prompt},
    },
    domain::{
        GoalId, Receipt, RunStatus,
        actor_model::{ActorLaunch, ModelRole},
        goal_review::{
            GOAL_OPTIONS, GOAL_REVIEW_ASKER, GoalReviewDecision, GoalReviewVerdict, decide,
        },
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
        // raised the hold in this pass (task 438).
        if starting
            && self.queue_hold.is_none()
            && let Err(error) = self.start_goal_review()
        {
            warn!(error = %format_args!("{error:#}"), "goal review: could not start a goal review: {error:#}");
        }
        progressed
    }

    /// Start the goal review of the first candidate, when none runs.
    fn start_goal_review(&mut self) -> Result<()> {
        if self.goal_review.is_some() {
            return Ok(());
        }
        let Some(&goal) = self.queue.goal_review_candidates()?.first() else {
            return Ok(());
        };
        let launch = self.actor_launch(ModelRole::GoalReview);
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
            Ok(headless) => {
                info!("goal {goal} goal review {} started", job.attempt);
                self.goal_review = Some(GoalReviewWatch { job, headless });
            }
            Err(error) => {
                let error = format!("the headless goal review could not start: {error:#}");
                self.fail_goal_review(&job, &error, 0);
            }
        }
        Ok(())
    }

    /// Write the prompt into the job's directory and start the headless
    /// job in the repository's checkout, allowed to read only.
    fn spawn_goal_review(
        &mut self,
        job: &GoalReviewJob,
        launch: &ActorLaunch,
    ) -> Result<HeadlessJob> {
        self.files
            .create_dir_all(&job.dir)
            .with_context(|| format!("create {}", job.dir.display()))?;
        let prompt = self.goal_review_material(job)?;
        self.files
            .write(&job.dir.join("prompt.txt"), prompt.as_bytes())?;
        let stdout = job.dir.join("review.out");
        let stderr = job.dir.join("review.err");
        let child = self
            .actors()
            .spawn(ActorExecutionSpec::new(
                ActorContext::goal_review_job(job.goal_id, job.attempt),
                WorkspaceAccess::Read(self.layout.repo_root.clone()),
                ActorProgram::Headless {
                    program: HeadlessProgram::Job {
                        cwd: &self.layout.repo_root,
                        prompt: &prompt,
                        allowed_tools: PLAN_REVIEW_TOOLS,
                    },
                    session_id: Some(&job.session_id),
                    launch: Some(launch),
                    without_mcp: false,
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
                label: None,
                run_id: None,
                proposal_id: None,
                goal_id: Some(job.goal_id),
                attempt: job.attempt,
            },
        ))
    }

    /// The goal review prompt of the job's goal from the queue as it is now.
    fn goal_review_material(&mut self, job: &GoalReviewJob) -> Result<String> {
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
        Ok(with_instruction(
            goal_review_prompt(&GoalReviewMaterial {
                goal: serde_json::to_value(&detail.goal)?,
                tasks,
                events,
                previous,
                gaps_in_a_row: job.gaps_in_a_row,
                repo_root: &self.layout.repo_root,
            }),
            self.verifier.language().as_ref(),
        ))
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
    fn poll_goal_review(&mut self) -> Result<bool> {
        let Some(watch) = self.goal_review.as_mut() else {
            return Ok(false);
        };
        let Some(outcome) = watch.headless.poll(&*self.files)? else {
            return Ok(false);
        };
        let watch = self.goal_review.take().expect("polled above");
        let duration_secs = watch.headless.started.elapsed().as_secs();
        let verdict = outcome.and_then(|stdout| GoalReviewVerdict::parse(&stdout));
        // Only a job that failed or printed no verdict is read for a wall:
        // a verdict's own text may quote anything (task 438).
        let wall = verdict
            .is_err()
            .then(|| self.job_wall(&watch.headless))
            .flatten();
        let applied = verdict.map_err(|error| anyhow!(error)).and_then(|verdict| {
            let job = ActorContext::goal_review_job(watch.job.goal_id, watch.job.attempt);
            self.for_job(&job, |sv| {
                sv.apply_goal_verdict(&watch.job, verdict, duration_secs)
            })
        });
        if let Err(error) = applied {
            let error = format!("{error:#}");
            // Stopped at a wall only a person moves: it joins the hold ask,
            // whose `done` rearms the goal review, and its failure is no
            // attention meanwhile (task 438).
            if let Some(wall) = wall {
                self.raise_job_wall(wall, &HoldJob::GoalReview(watch.job.goal_id), &error);
            }
            self.fail_goal_review(&watch.job, &error, duration_secs);
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
    ) -> Result<()> {
        let goal = job.goal_id;
        let (decision, overridden) = decide(verdict.verdict, job.gaps_in_a_row);
        let ask = (decision == GoalReviewDecision::Ask).then(|| NewAsk {
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
                warn!(ask_id = %outcome.ask.id, "goal {goal}: the inbox was not notified of ask {}: {error}", outcome.ask.id);
            }
        }
        Ok(())
    }

    /// Record the job's failure; the goal waits for a person
    /// (`goal_review_failed`) and is not reviewed again by itself until its
    /// tasks change.
    fn fail_goal_review(&mut self, job: &GoalReviewJob, error: &str, duration_secs: u64) {
        warn!(error = %error, "goal {} goal review {} failed: {error}; it waits for a person", job.goal_id, job.attempt);
        if let Err(recorded) = self
            .queue
            .fail_goal_review(job, &self.token, error, duration_secs)
        {
            warn!(error = %format_args!("{recorded:#}"), "goal {}: the goal review failure could not be recorded: {recorded:#}", job.goal_id);
        }
    }

    /// Apply the answered `approve_goal` asks (ADR-0047 decision 43).
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
        Ok(applied)
    }
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
}
