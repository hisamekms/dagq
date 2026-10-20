//! 計画管理 of the supervisor (rule C3 of docs/design/architecture.md):
//! its state ([`PlanningState`]) and the submodules that change it. The
//! state's fields are private to this module, so no other context's
//! submodule can change them; the loop calls the state's methods.

use super::*;
use crate::domain::session_step::{StepTarget, TargetKind};

pub(super) mod draft_planner;
pub(super) mod finding_planner;
pub(super) mod goal_review;
pub(super) mod plan_review;
pub(super) mod planner_turns;
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
    /// The runtime's planners this process asked to exit (their exit
    /// request), and when.
    planner_exits: Vec<(crate::domain::PlannerId, Instant)>,
    /// Whether `[roles.runtime_planner] route`, which the runtime's
    /// planners ignore, was warned of (ADR-t1433-2 decision 3).
    route_setting_warned: bool,
    /// Reads the limit on the improvement proposals running.
    max_improvement_proposals: Arc<dyn Fn() -> Result<usize> + Send + Sync>,
}

impl PlanningState {
    pub(super) fn new(ports: &Ports<'_>) -> Self {
        Self {
            plan_review: None,
            goal_review: None,
            planner_exits: Vec::new(),
            route_setting_warned: false,
            max_improvement_proposals: ports.max_improvement_proposals.clone(),
        }
    }

    /// A plan or goal review runs, which a loop with no slot waits for
    /// like a run.
    pub(super) const fn busy(&self) -> bool {
        self.plan_review.is_some() || self.goal_review.is_some()
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
    env.jobs.target_telemetry(&env.pass).end(
        &*env.pass.queue,
        &StepTarget::of(kind, id),
        crate::domain::telemetry::TokenRevocation::JobEnded,
    );
}
