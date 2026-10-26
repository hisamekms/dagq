//! 計画管理 of the supervisor (rule C3 of docs/design/architecture.md):
//! its state ([`PlanningState`]) and the submodules that change it. The
//! state's fields are private to this module, so no other context's
//! submodule can change them; the loop calls the state's methods.

use super::*;
use crate::application::supervisor_service::resend::{HeldRecords, KeptRecord, not_taken};
use crate::domain::session_step::{StepTarget, TargetKind};
use std::collections::{BTreeSet, VecDeque};

pub(super) mod draft_planner;
pub(super) mod finding_planner;
pub(super) mod goal_review;
pub(super) mod plan_review;
pub(super) mod planner_turns;
pub(super) mod recurring;
pub(super) mod replan_apply;
pub(super) mod request_planner;

/// 計画管理's state: the plan review and the goal review running now (one
/// each, queue-wide, outside the run slots), the runtime's planners asked
/// to exit, and what opening a planner reads. Only its submodules change
/// it, through their passes.
pub(super) struct PlanningState {
    /// The plan review job running now (ADR-0041 decision 11).
    plan_review: Option<plan_review::PlanReviewWatch>,
    /// The goal review job running now.
    goal_review: Option<goal_review::GoalReviewWatch>,
    /// The ends (verdicts and failures) of plan reviews that ended which
    /// the queue did not take, oldest first: sent again on the next pass
    /// (ADR-t3102-3 decision 5), each kept in [`Self::verdicts`] until the
    /// queue takes it, so the next supervisor (after an exec or a restart)
    /// sends it and the review does not run again.
    held_plan_reviews: VecDeque<plan_review::HeldPlanReview>,
    /// The same of goal reviews.
    held_goal_reviews: VecDeque<goal_review::HeldGoalReview>,
    /// Where the ends held are kept beside the queue.
    verdicts: Arc<dyn HeldRecords>,
    /// This process's pid, which the verdicts it keeps name: the next
    /// supervisor takes them only from a process that is gone, or from
    /// itself after an exec.
    pid: u32,
    /// The runtime's planners this process asked to exit (their exit
    /// request), and when.
    planner_exits: Vec<(crate::domain::PlannerId, Instant)>,
    /// Whether `[roles.runtime_planner] route`, which the runtime's
    /// planners ignore, was warned of (ADR-t1433-2 decision 3).
    route_setting_warned: bool,
    /// Reads the limit on the improvement proposals running.
    max_improvement_proposals: Arc<dyn Fn() -> Result<usize> + Send + Sync>,
    /// Reads `[[recurring]]`; `None` registers no recurring draft.
    recurring_file: Option<RecurringFile>,
    /// The last error reading `[[recurring]]` warned of, so one error is
    /// warned of once.
    recurring_error: Option<String>,
}

impl PlanningState {
    /// The state of a supervisor of `pid`, holding the verdicts a
    /// supervisor before it kept ([`Self::take_kept_verdicts`]).
    pub(super) fn new(ports: &Ports<'_>, pid: u32) -> Self {
        let mut state = Self {
            plan_review: None,
            goal_review: None,
            held_plan_reviews: VecDeque::new(),
            held_goal_reviews: VecDeque::new(),
            verdicts: ports.held_verdicts.clone(),
            pid,
            planner_exits: Vec::new(),
            route_setting_warned: false,
            max_improvement_proposals: ports.max_improvement_proposals.clone(),
            recurring_file: ports.recurring_file.clone(),
            recurring_error: None,
        };
        state.take_kept_verdicts(&*ports.processes);
        state
    }

    /// A plan or goal review runs, which a loop with no slot waits for
    /// like a run, or an end (verdict or failure) the queue did not take
    /// waits to be sent again, which a loop that stops leaves kept for the
    /// next supervisor.
    pub(super) fn busy(&self, stopping: bool) -> bool {
        self.plan_review.is_some()
            || self.goal_review.is_some()
            || (!stopping
                && (!self.held_plan_reviews.is_empty() || !self.held_goal_reviews.is_empty()))
    }

    /// Hold the review ends kept beside the queue that are this process's
    /// to send ([`verdicts_to_take`]), oldest first; one that does not read
    /// is let go. At the start and again before a review starts, so the
    /// end a supervisor kept is taken once that supervisor is gone, and
    /// applied rather than its row interrupted and its review run again
    /// (ADR-t3102-3 decision 5).
    fn take_kept_verdicts(&mut self, processes: &dyn ProcessControl) {
        let kept = match self.verdicts.kept() {
            Ok(kept) => kept,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the review verdicts kept for the queue could not be read: {error:#}");
                return;
            }
        };
        let held: BTreeSet<String> = self
            .held_plan_reviews
            .iter()
            .filter_map(plan_review::HeldPlanReview::kept_key)
            .chain(
                self.held_goal_reviews
                    .iter()
                    .filter_map(goal_review::HeldGoalReview::kept_key),
            )
            .map(str::to_owned)
            .collect();
        for (key, verdict) in verdicts_to_take(kept, &held, self.pid, |pid| processes.alive(pid)) {
            match verdict {
                KeptVerdict::Plan(held) => {
                    info!("a plan review's end a supervisor before this one kept is sent here");
                    self.held_plan_reviews.push_back(*held);
                }
                KeptVerdict::Goal(held) => {
                    info!("a goal review's end a supervisor before this one kept is sent here");
                    self.held_goal_reviews.push_back(*held);
                }
                KeptVerdict::Others => {}
                KeptVerdict::Unread(error) => {
                    warn!("a kept review verdict does not read and is let go: {error}");
                    self.forget_verdict(Some(&key));
                }
            }
        }
    }

    /// Keep `verdict`, the end (verdict or failure) of the review `op`
    /// names ([`PLAN_VERDICT`], [`GOAL_VERDICT`]), beside the queue until
    /// the queue takes it: its key, or `None` when it could not be kept,
    /// and is then lost if this process ends before the queue takes it.
    fn keep_verdict(&self, op: &str, verdict: &impl serde::Serialize) -> Option<String> {
        let kept = verdict_record(op, self.pid, verdict)
            .map_err(anyhow::Error::from)
            .and_then(|record| {
                self.verdicts
                    .keep(crate::domain::queue_service::UseCase::Supervisor, &record)
            });
        kept.inspect_err(|error| {
            warn!(error = %format_args!("{error:#}"), "the {op} the queue did not take could not be kept, and is lost if this supervisor ends first: {error:#}");
        })
        .ok()
    }

    /// Let go of the kept verdict `key`, taken or failed.
    fn forget_verdict(&self, key: Option<&str>) {
        if let Some(key) = key
            && let Err(error) = self.verdicts.forget(key)
        {
            warn!(error = %format_args!("{error:#}"), "a kept review verdict could not be let go, and the next supervisor finds its row finished: {error:#}");
        }
    }
}

/// The `op` of a kept plan review's end, its verdict or its failure.
pub(super) const PLAN_VERDICT: &str = "plan_review_verdict";
/// The `op` of a kept goal review's end.
pub(super) const GOAL_VERDICT: &str = "goal_review_verdict";

/// The record kept of `verdict` of the review `op` names, by the
/// supervisor of `pid`.
fn verdict_record(
    op: &str,
    pid: u32,
    verdict: &impl serde::Serialize,
) -> serde_json::Result<serde_json::Value> {
    Ok(serde_json::json!({"op": op, "pid": pid, "verdict": serde_json::to_value(verdict)?}))
}

/// What a supervisor of `pid` makes of each of the review ends `kept`
/// beside the queue ([`kept_verdict`]), by its key, but those it holds
/// already (`held`, their keys), which it read before: none is held twice.
fn verdicts_to_take(
    kept: Vec<KeptRecord>,
    held: &BTreeSet<String>,
    pid: u32,
    alive: impl Fn(u32) -> bool,
) -> Vec<(String, KeptVerdict)> {
    kept.into_iter()
        .filter(|record| !held.contains(&record.key))
        .map(|record| (record.key.clone(), kept_verdict(&record, pid, &alive)))
        .collect()
}

/// What a supervisor of `pid` makes of a review's end kept beside the
/// queue.
#[derive(Debug)]
enum KeptVerdict {
    Plan(Box<plan_review::HeldPlanReview>),
    Goal(Box<goal_review::HeldGoalReview>),
    /// A live supervisor's, which it applies itself, or a record of a
    /// kind this binary does not read.
    Others,
    /// One that does not read, with why.
    Unread(String),
}

/// Whether the kept `record` is the end (verdict or failure) of a review
/// this supervisor of `pid` sends (ADR-t3102-3 decision 5): one its process
/// kept before an exec (the same pid) or one of a process gone (`alive`
/// says whether a pid runs); a live supervisor's is its own to send.
fn kept_verdict(record: &KeptRecord, pid: u32, alive: impl Fn(u32) -> bool) -> KeptVerdict {
    let params = &record.params;
    let keeper = params["pid"]
        .as_u64()
        .and_then(|keeper| u32::try_from(keeper).ok());
    let Some(keeper) = keeper else {
        return KeptVerdict::Unread("it names no pid".into());
    };
    if keeper != pid && alive(keeper) {
        return KeptVerdict::Others;
    }
    let verdict = params["verdict"].clone();
    let read = |error: serde_json::Error| KeptVerdict::Unread(error.to_string());
    match params["op"].as_str() {
        Some(PLAN_VERDICT) => match serde_json::from_value(verdict) {
            Ok(held) => KeptVerdict::Plan(Box::new(plan_review::HeldPlanReview::kept(
                held,
                record.key.clone(),
            ))),
            Err(error) => read(error),
        },
        Some(GOAL_VERDICT) => match serde_json::from_value(verdict) {
            Ok(held) => KeptVerdict::Goal(Box::new(goal_review::HeldGoalReview::kept(
                held,
                record.key.clone(),
            ))),
            Err(error) => read(error),
        },
        // Another binary's kind of record: left to the one that reads it.
        _ => KeptVerdict::Others,
    }
}

/// What becomes of a review's end, its verdict or its failure, sent to the
/// queue (ADR-t3102-3 decision 5): applied or recorded; held, to be sent
/// again on the next pass, while the queue did not take it (the service
/// did not answer, or the queue stayed busy past its wait: the job's
/// process is gone, so its end is all there is, and the queue takes a job
/// no longer running once); or refused, with the error the queue refused
/// it with, which fails a verdict's job.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum VerdictSent {
    Applied,
    Held,
    Failed(String),
}

/// [`VerdictSent`] of the end whose sending gave `sent`.
pub(super) fn verdict_sent(sent: Result<()>) -> VerdictSent {
    match sent {
        Ok(()) => VerdictSent::Applied,
        Err(error) if not_taken(&error) => VerdictSent::Held,
        Err(error) => VerdictSent::Failed(format!("{error:#}")),
    }
}

/// Issue the telemetry token of the job of `kind` and `id` (a plan review,
/// a goal review) as its agent is started (ADR-t1662-3 decision 11), and
/// register to it `session_id`, the session named ahead (Claude's): one
/// not issued or not registered is logged, and the job goes on without
/// telemetry.
fn issue_job_telemetry(
    env: &contexts::PlanningEnv<'_>,
    kind: TargetKind,
    id: i64,
    session_id: Option<&str>,
) {
    let telemetry = env.jobs.target_telemetry(&env.pass);
    let target = StepTarget::of(kind, id);
    telemetry.issue(&*env.pass.queue, &target);
    telemetry.register(&target, session_id);
}

/// Register to the job of `kind` and `id` the session its agent named
/// itself, as its output read at its end names it (a Codex thread), before
/// its token is revoked ([`end_job_telemetry`]).
fn register_job_session(
    env: &contexts::PlanningEnv<'_>,
    kind: TargetKind,
    id: i64,
    session: Option<&crate::domain::headless_job::JobSession>,
) {
    let named = session.filter(|session| session.named);
    env.jobs.target_telemetry(&env.pass).register(
        &StepTarget::of(kind, id),
        named.and_then(|session| session.session_id.as_deref()),
    );
}

/// Revoke the telemetry token of the job of `kind` and `id` at its end
/// (its exit read, its failure, its stop for a handoff); one not revoked is
/// left to the sweep, and the service refuses it once the row finished.
fn end_job_telemetry(env: &contexts::PlanningEnv<'_>, kind: TargetKind, id: i64) {
    // What its steps could not send yet (a Codex thread registered only at
    // its end) goes while its token holds.
    env.jobs.resend_steps();
    env.jobs.target_telemetry(&env.pass).end(
        &*env.pass.queue,
        &StepTarget::of(kind, id),
        crate::domain::telemetry::TokenRevocation::JobEnded,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kept record that names no pid, or a verdict that does not read,
    /// is no verdict to apply, whoever reads it; one of a kind this binary
    /// does not read is left for the binary that does.
    #[test]
    fn a_kept_record_that_is_no_reviews_verdict_does_not_read() {
        let record = |params: serde_json::Value| KeptRecord {
            key: "k".into(),
            use_case: crate::domain::queue_service::UseCase::Supervisor,
            params,
        };
        for params in [
            serde_json::json!({"op": PLAN_VERDICT, "verdict": {}}),
            serde_json::json!({"op": PLAN_VERDICT, "pid": 7, "verdict": {"job": 1}}),
            serde_json::json!({"op": GOAL_VERDICT, "pid": 7, "verdict": null}),
        ] {
            assert!(
                matches!(
                    kept_verdict(&record(params.clone()), 7, |_| false),
                    KeptVerdict::Unread(_)
                ),
                "{params}"
            );
        }
        let other_kind = record(serde_json::json!({"op": "another_kind", "pid": 7, "verdict": {}}));
        assert!(matches!(
            kept_verdict(&other_kind, 7, |_| false),
            KeptVerdict::Others
        ));
    }

    /// The ends a supervisor takes are those of its own process (an exec)
    /// and of processes gone, each once: one it holds already is not read
    /// again, and a live supervisor's is taken once it is gone.
    #[test]
    fn a_kept_verdict_is_taken_once_and_once_its_keeper_is_gone() {
        let record = |key: &str, pid: u32| KeptRecord {
            key: key.into(),
            use_case: crate::domain::queue_service::UseCase::Supervisor,
            params: serde_json::json!({"op": PLAN_VERDICT, "pid": pid, "verdict": {}}),
        };
        let kept = || vec![record("own", 7), record("gone", 8), record("live", 9)];
        let taken = |held: &[&str], alive: u32| -> Vec<String> {
            let held: BTreeSet<String> = held.iter().map(|key| (*key).to_owned()).collect();
            verdicts_to_take(kept(), &held, 7, |pid| pid == alive)
                .into_iter()
                .filter(|(_, verdict)| !matches!(verdict, KeptVerdict::Others))
                .map(|(key, _)| key)
                .collect()
        };
        assert_eq!(taken(&[], 9), ["own", "gone"]);
        assert_eq!(taken(&["own"], 9), ["gone"]);
        assert_eq!(taken(&["own", "gone"], 9), Vec::<String>::new());
        assert_eq!(taken(&["own", "gone"], 0), ["live"]);
    }

    /// A verdict the service did not answer, or the queue was busy for
    /// past its wait, is held, wherever in the error's chain that is said;
    /// one the queue refused is its job's failure; one taken is applied.
    #[test]
    fn a_verdict_the_service_did_not_answer_is_held_and_one_refused_fails_its_job() {
        use crate::application::supervisor_service::ServiceUnreachable;
        assert_eq!(verdict_sent(Ok(())), VerdictSent::Applied);
        let gone = anyhow::Error::new(ServiceUnreachable::not_done("no socket"))
            .context("finish_plan_review");
        assert_eq!(verdict_sent(Err(gone)), VerdictSent::Held);
        let busy = anyhow::anyhow!("database is locked")
            .context(crate::application::QueueBusy)
            .context("finish_goal_review");
        assert_eq!(verdict_sent(Err(busy)), VerdictSent::Held);
        let refused = anyhow::anyhow!("the proposal is gone");
        assert_eq!(
            verdict_sent(Err(refused)),
            VerdictSent::Failed("the proposal is gone".into())
        );
    }
}
