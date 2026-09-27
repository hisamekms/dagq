//! Who acts on the queue (ADR-t728-1): an actor is an [`ActorRole`], whose
//! [`TrustLevel`] the role alone decides, and an actor id, carried in an
//! [`ActorContext`]. The runtime puts the role and the id in the
//! environment of every AI actor it starts (`DAGQ_ROLE`, `DAGQ_ACTOR_ID`,
//! and a worker's `DAGQ_RUN_ID` / `DAGQ_TASK_ID`), and the CLI parses them
//! back before it runs a command. An unknown `DAGQ_ROLE` is refused (fail
//! closed); none at all is the user, a person at a plain terminal, which is
//! advisory on a host: any process can set or unset the variable.

use serde::{Deserialize, Serialize};

use super::{DomainError, RunId, TaskId};

/// The role of a session, in its environment (ADR-0026).
pub const ROLE_ENV: &str = "DAGQ_ROLE";
/// The id of the actor a session is: see [`ActorContext::env`].
pub const ACTOR_ID_ENV: &str = "DAGQ_ACTOR_ID";
/// The run a worker works on.
pub const RUN_ID_ENV: &str = "DAGQ_RUN_ID";
/// The task of the run a worker works on.
pub const TASK_ID_ENV: &str = "DAGQ_TASK_ID";
/// The `DAGQ_ROLE` every headless job ran under before the jobs got their
/// own roles. A job an older binary started still carries it, so it is read
/// as a read-only job for the migration (ADR-t728-1 decision 2).
pub const LEGACY_REVIEWER_ROLE: &str = "reviewer";
/// What a note, mark, finding or ask records as its writer for the user.
pub const WRITTEN_BY_USER: &str = "human";

// The actors of ADR-t728-1 decision 2, spelled as there. `User` has no
// `DAGQ_ROLE`; `desk` joins when goal 48 makes it.
string_enum!(ActorRole {
    User => "user",
    Inbox => "inbox",
    Planner => "planner",
    Worker => "worker",
    ReviewJob => "review-job",
    RecoveryJob => "recovery-job",
    PlanReviewJob => "plan-review-job",
    GoalReviewJob => "goal-review-job",
    Observer => "observer",
    Supervisor => "supervisor",
    Wrapper => "wrapper",
    Integrator => "integrator",
});

// How far an actor is trusted (ADR-t728-1 decision 1): a person, the
// deterministic control plane, or an AI actor whose output is data.
string_enum!(TrustLevel {
    Human => "human",
    TrustedControlPlane => "trusted_control_plane",
    UntrustedAgent => "untrusted_agent",
});

impl ActorRole {
    pub const ALL: [Self; 12] = [
        Self::User,
        Self::Inbox,
        Self::Planner,
        Self::Worker,
        Self::ReviewJob,
        Self::RecoveryJob,
        Self::PlanReviewJob,
        Self::GoalReviewJob,
        Self::Observer,
        Self::Supervisor,
        Self::Wrapper,
        Self::Integrator,
    ];

    /// The trust of the role: fixed by the role, never inferred from a
    /// prompt or a name.
    pub const fn trust(self) -> TrustLevel {
        match self {
            Self::User => TrustLevel::Human,
            Self::Supervisor | Self::Wrapper | Self::Integrator => TrustLevel::TrustedControlPlane,
            Self::Inbox
            | Self::Planner
            | Self::Worker
            | Self::ReviewJob
            | Self::RecoveryJob
            | Self::PlanReviewJob
            | Self::GoalReviewJob
            | Self::Observer => TrustLevel::UntrustedAgent,
        }
    }

    /// One of the supervisor's headless jobs, whose verdict the supervisor
    /// applies: the policy allows them reads only (ADR-0027), and the CLI
    /// refuses the rest with the reviewer's message.
    pub const fn is_headless_job(self) -> bool {
        matches!(
            self,
            Self::ReviewJob | Self::RecoveryJob | Self::PlanReviewJob | Self::GoalReviewJob
        )
    }

    /// `DAGQ_ROLE` of the role; the user has none.
    pub const fn env_value(self) -> Option<&'static str> {
        match self {
            Self::User => None,
            other => Some(other.as_str()),
        }
    }
}

/// The actor a command runs as, or an AI actor the runtime starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActorContext {
    actor_id: String,
    role: ActorRole,
    trust: TrustLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<RunId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<TaskId>,
}

impl ActorContext {
    /// `role` as `actor_id`; the trust is the role's.
    pub fn new(role: ActorRole, actor_id: impl Into<String>) -> Self {
        Self {
            actor_id: actor_id.into(),
            role,
            trust: role.trust(),
            run_id: None,
            task_id: None,
        }
    }

    /// A person at a plain terminal.
    pub fn user() -> Self {
        Self::new(ActorRole::User, ActorRole::User.as_str())
    }

    /// One actor of `role` among others: `<role>:<instance>`, such as
    /// `planner:7` or `review-job:<run>:2`.
    pub fn instance(role: ActorRole, instance: impl std::fmt::Display) -> Self {
        Self::new(role, format!("{}:{instance}", role.as_str()))
    }

    /// The worker (or its resume) of `run` on `task`.
    pub fn worker(run: &RunId, task: TaskId) -> Self {
        Self::instance(ActorRole::Worker, run).with_run(run.clone(), task)
    }

    /// The review job `attempt` of `run`.
    pub fn review_job(run: &RunId, attempt: impl std::fmt::Display) -> Self {
        Self::instance(ActorRole::ReviewJob, format_args!("{run}:{attempt}"))
    }

    /// The recovery job `attempt` of `alert` on `run`.
    pub fn recovery_job(run: &RunId, alert: &str, attempt: impl std::fmt::Display) -> Self {
        Self::instance(
            ActorRole::RecoveryJob,
            format_args!("{run}:{alert}:{attempt}"),
        )
    }

    /// The plan review `attempt` of a proposal.
    pub fn plan_review_job(
        proposal: impl std::fmt::Display,
        attempt: impl std::fmt::Display,
    ) -> Self {
        Self::instance(
            ActorRole::PlanReviewJob,
            format_args!("{proposal}:{attempt}"),
        )
    }

    /// The goal review `attempt` of a goal.
    pub fn goal_review_job(goal: impl std::fmt::Display, attempt: impl std::fmt::Display) -> Self {
        Self::instance(ActorRole::GoalReviewJob, format_args!("{goal}:{attempt}"))
    }

    pub fn with_run(mut self, run: RunId, task: TaskId) -> Self {
        self.run_id = Some(run);
        self.task_id = Some(task);
        self
    }

    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    pub fn role(&self) -> ActorRole {
        self.role
    }

    pub fn trust(&self) -> TrustLevel {
        self.trust
    }

    pub fn run_id(&self) -> Option<&RunId> {
        self.run_id.as_ref()
    }

    pub fn task_id(&self) -> Option<TaskId> {
        self.task_id
    }

    /// The actor of a process from its environment (`env` reads one
    /// variable). No `DAGQ_ROLE`, or an empty one, is the user. A value
    /// that is not a role other than the user is an error, and so is an
    /// unreadable run or task id: nothing runs as an actor it cannot name.
    /// The legacy `reviewer` is read as a review job. Without
    /// `DAGQ_ACTOR_ID` (a session opened before it) the id is the value of
    /// `DAGQ_ROLE`.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self, DomainError> {
        let Some(value) = env(ROLE_ENV).filter(|value| !value.is_empty()) else {
            return Ok(Self::user());
        };
        let role = match value.as_str() {
            LEGACY_REVIEWER_ROLE => ActorRole::ReviewJob,
            other => match other.parse::<ActorRole>() {
                Ok(ActorRole::User) | Err(_) => {
                    return Err(DomainError::UnknownValue {
                        kind: ROLE_ENV,
                        value,
                    });
                }
                Ok(role) => role,
            },
        };
        let actor_id = env(ACTOR_ID_ENV)
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| value.clone());
        let mut actor = Self::new(role, actor_id);
        if let Some(run) = env(RUN_ID_ENV).filter(|run| !run.is_empty()) {
            actor.run_id = Some(RunId::new(run)?);
        }
        if let Some(task) = env(TASK_ID_ENV).filter(|task| !task.is_empty()) {
            let id = task
                .trim()
                .parse::<i64>()
                .map_err(|_| DomainError::UnknownValue {
                    kind: TASK_ID_ENV,
                    value: task.clone(),
                })?;
            actor.task_id = Some(TaskId::new(id));
        }
        Ok(actor)
    }

    /// The environment that makes a process this actor: its role (none
    /// for the user), its id, and its run and task when it has them.
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if let Some(role) = self.role.env_value() {
            env.push((ROLE_ENV.to_owned(), role.to_owned()));
        }
        env.push((ACTOR_ID_ENV.to_owned(), self.actor_id.clone()));
        if let Some(run) = &self.run_id {
            env.push((RUN_ID_ENV.to_owned(), run.to_string()));
        }
        if let Some(task) = self.task_id {
            env.push((TASK_ID_ENV.to_owned(), task.to_string()));
        }
        env
    }

    /// The writer a note, mark, finding or ask (`asked_by`) records: the
    /// role, or [`WRITTEN_BY_USER`] for the user, as before the type.
    pub fn written_by(&self) -> &'static str {
        self.role.env_value().unwrap_or(WRITTEN_BY_USER)
    }

    /// Who an answer records as `answered_by`: the role, or
    /// [`super::ANSWERED_BY_PERSON`] for the user.
    pub fn answered_by(&self) -> &'static str {
        self.role.env_value().unwrap_or(super::ANSWERED_BY_PERSON)
    }
}

/// Set `name` to `value` in `env`, replacing an earlier value.
pub fn set_env(env: &mut Vec<(String, String)>, name: &str, value: String) {
    match env.iter_mut().find(|(key, _)| key == name) {
        Some((_, old)) => *old = value,
        None => env.push((name.to_owned(), value)),
    }
}

/// `env` with every variable of `actor` set (see [`set_env`]).
pub fn with_actor(mut env: Vec<(String, String)>, actor: &ActorContext) -> Vec<(String, String)> {
    for (name, value) in actor.env() {
        set_env(&mut env, &name, value);
    }
    env
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn parse(pairs: &[(&str, &str)]) -> Result<ActorContext, DomainError> {
        let env: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        ActorContext::from_env(|name| env.get(name).cloned())
    }

    #[test]
    fn the_role_alone_decides_the_trust() {
        for role in ActorRole::ALL {
            let expected = match role {
                ActorRole::User => TrustLevel::Human,
                ActorRole::Supervisor | ActorRole::Wrapper | ActorRole::Integrator => {
                    TrustLevel::TrustedControlPlane
                }
                _ => TrustLevel::UntrustedAgent,
            };
            assert_eq!(role.trust(), expected, "{role:?}");
            assert_eq!(ActorContext::new(role, "anyone").trust(), expected);
            assert_eq!(role.as_str().parse::<ActorRole>().unwrap(), role);
        }
        let jobs: Vec<_> = ActorRole::ALL
            .into_iter()
            .filter(|role| role.is_headless_job())
            .map(ActorRole::as_str)
            .collect();
        assert_eq!(
            jobs,
            [
                "review-job",
                "recovery-job",
                "plan-review-job",
                "goal-review-job"
            ]
        );
    }

    #[test]
    fn no_role_is_the_user_and_an_unknown_role_fails_closed() {
        for env in [&[][..], &[("DAGQ_ROLE", "")], &[("DAGQ_ACTOR_ID", "x")]] {
            let actor = parse(env).unwrap();
            assert_eq!(actor, ActorContext::user());
            assert_eq!(actor.trust(), TrustLevel::Human);
            assert_eq!(actor.written_by(), "human");
            assert_eq!(actor.answered_by(), "person");
        }
        for value in ["inboxes", "user", "Worker", "reviewers", "person", "human"] {
            let error = parse(&[("DAGQ_ROLE", value)]).unwrap_err();
            assert_eq!(error.to_string(), format!("unknown DAGQ_ROLE: {value}"));
        }
        assert!(parse(&[("DAGQ_ROLE", "worker"), ("DAGQ_RUN_ID", " ")]).is_err());
        assert!(parse(&[("DAGQ_ROLE", "worker"), ("DAGQ_TASK_ID", "seven")]).is_err());
    }

    #[test]
    fn the_environment_carries_the_actor_back_and_forth() {
        let run = RunId::new("r1").unwrap();
        let worker = ActorContext::worker(&run, TaskId::new(7));
        assert_eq!(worker.actor_id(), "worker:r1");
        let env = worker.env();
        assert_eq!(
            env,
            [
                ("DAGQ_ROLE".to_owned(), "worker".to_owned()),
                ("DAGQ_ACTOR_ID".to_owned(), "worker:r1".to_owned()),
                ("DAGQ_RUN_ID".to_owned(), "r1".to_owned()),
                ("DAGQ_TASK_ID".to_owned(), "7".to_owned()),
            ]
        );
        let pairs: Vec<_> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let parsed = parse(&pairs).unwrap();
        assert_eq!(parsed, worker);
        assert_eq!(parsed.run_id(), Some(&run));
        assert_eq!(parsed.task_id(), Some(TaskId::new(7)));
        assert_eq!(parsed.written_by(), "worker");
        assert_eq!(parsed.answered_by(), "worker");
        assert_eq!(
            ActorContext::user().env(),
            [("DAGQ_ACTOR_ID".to_owned(), "user".to_owned())]
        );

        // A session from before the id is named by its role.
        let inbox = parse(&[("DAGQ_ROLE", "inbox")]).unwrap();
        assert_eq!(
            (inbox.role(), inbox.actor_id()),
            (ActorRole::Inbox, "inbox")
        );
        // The legacy reviewer reads as a review job.
        let reviewer = parse(&[("DAGQ_ROLE", "reviewer")]).unwrap();
        assert_eq!(reviewer.role(), ActorRole::ReviewJob);
        assert_eq!(reviewer.actor_id(), "reviewer");
        assert!(reviewer.role().is_headless_job());

        let mut env = vec![("DAGQ_ROLE".to_owned(), "worker".to_owned())];
        set_env(&mut env, "DAGQ_QUEUE", "q".into());
        let env = with_actor(env, &ActorContext::instance(ActorRole::Planner, 3));
        assert_eq!(
            env,
            [
                ("DAGQ_ROLE".to_owned(), "planner".to_owned()),
                ("DAGQ_QUEUE".to_owned(), "q".to_owned()),
                ("DAGQ_ACTOR_ID".to_owned(), "planner:3".to_owned()),
            ]
        );
    }
}
