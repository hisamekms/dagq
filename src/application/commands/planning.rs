//! The planning commands (task 732): goals, tasks before they run, their
//! dependencies and proposals. Each is authorized as the caller before it
//! reaches the store, with the task's status and the proposal's owner read
//! from the store, so the planner's rules (tasks in draft, submitted or
//! ready only; its own proposal only) apply to what the queue holds rather
//! than to what the command line says. The policy is the
//! [`crate::domain::StaticPolicy`]'s (`docs/design/authorization.md`).

use anyhow::Result;
use serde_json::Value;
use tracing::warn;

use crate::domain::{
    ActorContext, AuthorizationError, Authorizer, Capability, FindingId, Goal, GoalEdit, GoalId,
    GoalVerdict, NewGoal, NewTask, Priority, Proposal, ProposalId, Resource, Submission, Task,
    TaskAction, TaskDetail, TaskEdit, TaskId, TaskStatus, authorization::DenyReason,
};

/// What the planning commands read and change of the queue.
pub trait PlanningStore {
    /// The status of `task`; a missing task is an error.
    fn task_status(&self, task: TaskId) -> Result<TaskStatus>;
    /// The actor id of the planner that owns `proposal`, `None` when the
    /// queue did not record it; a missing proposal is an error.
    fn proposal_owner(&self, proposal: ProposalId) -> Result<Option<String>>;
    /// Record a refusal as the queue event `authorization_denied`.
    fn record_denial(&self, payload: Value) -> Result<()>;

    fn add(&mut self, task: NewTask) -> Result<Task>;
    fn edit_task(&mut self, task: TaskId, edit: TaskEdit) -> Result<Task>;
    fn set_goal(&mut self, task: TaskId, goal: Option<GoalId>) -> Result<Task>;
    fn set_paths(&mut self, task: TaskId, paths: Vec<String>) -> Result<Task>;
    fn set_priority(&mut self, task: TaskId, priority: Priority) -> Result<Task>;
    fn transition(&mut self, task: TaskId, action: TaskAction) -> Result<Task>;
    fn cancel_duplicate(&mut self, task: TaskId, duplicate_of: TaskId) -> Result<Task>;
    fn add_dependency(&mut self, task: TaskId, on: Dependency) -> Result<()>;
    fn remove_dependency(&mut self, task: TaskId, on: Dependency) -> Result<()>;
    fn show(&mut self, task: TaskId) -> Result<TaskDetail>;
    fn submit(&mut self, submission: Submission, findings: &[FindingId]) -> Result<Proposal>;
    fn withdraw_proposal(&mut self, proposal: ProposalId) -> Result<Proposal>;
    fn add_goal(&mut self, goal: NewGoal) -> Result<Goal>;
    fn edit_goal(&mut self, goal: GoalId, edit: GoalEdit) -> Result<Goal>;
    fn ready_goal(&mut self, goal: GoalId) -> Result<Goal>;
    fn close_goal(&mut self, goal: GoalId, verdict: GoalVerdict) -> Result<Goal>;
    fn rearm_goal_review(&mut self, goal: GoalId) -> Result<Value>;
}

/// What a task waits for: another task, or a goal closed as achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dependency {
    Task(TaskId),
    Goal(GoalId),
}

/// The planning commands of one actor on one store.
pub struct Planning<'a, S: ?Sized> {
    store: &'a mut S,
    actor: &'a ActorContext,
    authorizer: &'a dyn Authorizer,
}

impl<'a, S: PlanningStore + ?Sized> Planning<'a, S> {
    pub fn new(store: &'a mut S, actor: &'a ActorContext, authorizer: &'a dyn Authorizer) -> Self {
        Self {
            store,
            actor,
            authorizer,
        }
    }

    /// `add`: a draft task, in `goal` when it names one.
    pub fn add(&mut self, task: NewTask) -> Result<Task> {
        let resource = task.goal_id.map_or(Resource::Queue, Resource::Goal);
        self.authorize(Capability::TaskWrite, resource)?;
        self.store.add(task)
    }

    pub fn edit(&mut self, task: TaskId, edit: TaskEdit) -> Result<Task> {
        self.authorize_task(Capability::TaskWrite, task)?;
        self.store.edit_task(task, edit)
    }

    pub fn set_goal(&mut self, task: TaskId, goal: Option<GoalId>) -> Result<Task> {
        self.authorize_task(Capability::TaskWrite, task)?;
        self.store.set_goal(task, goal)
    }

    pub fn set_paths(&mut self, task: TaskId, paths: Vec<String>) -> Result<Task> {
        self.authorize_task(Capability::TaskWrite, task)?;
        self.store.set_paths(task, paths)
    }

    pub fn set_priority(&mut self, task: TaskId, priority: Priority) -> Result<Task> {
        self.authorize_task(Capability::TaskWrite, task)?;
        self.store.set_priority(task, priority)
    }

    pub fn draft(&mut self, task: TaskId) -> Result<Task> {
        self.authorize_task(Capability::TaskWrite, task)?;
        self.store.transition(task, TaskAction::Draft)
    }

    /// `ready`, or `ready --bypass-review` past plan review: the user's and
    /// the inbox's at a person's word, which the event's actor tells apart.
    pub fn ready(&mut self, task: TaskId, bypass_review: bool) -> Result<Task> {
        let (capability, action) = if bypass_review {
            (Capability::TaskReadyBypassReview, TaskAction::BypassReview)
        } else {
            (Capability::TaskReady, TaskAction::Ready)
        };
        self.authorize_task(capability, task)?;
        self.store.transition(task, action)
    }

    /// `cancel`, as a duplicate of another task when it names one.
    pub fn cancel(&mut self, task: TaskId, duplicate_of: Option<TaskId>) -> Result<Task> {
        self.authorize_task(Capability::TaskCancel, task)?;
        match duplicate_of {
            Some(other) => self.store.cancel_duplicate(task, other),
            None => self.store.transition(task, TaskAction::Cancel),
        }
    }

    /// `dependency add`: the task as it is afterwards.
    pub fn add_dependency(&mut self, task: TaskId, on: Dependency) -> Result<TaskDetail> {
        self.authorize_task(Capability::TaskWrite, task)?;
        self.store.add_dependency(task, on)?;
        self.store.show(task)
    }

    /// `dependency remove`: the task as it is afterwards.
    pub fn remove_dependency(&mut self, task: TaskId, on: Dependency) -> Result<TaskDetail> {
        self.authorize_task(Capability::TaskWrite, task)?;
        self.store.remove_dependency(task, on)?;
        self.store.show(task)
    }

    /// `submit`: a new proposal, or again the one it names (any planner
    /// may take up a proposal plan review sent back).
    pub fn submit(&mut self, submission: Submission, findings: &[FindingId]) -> Result<Proposal> {
        let named = submission
            .proposal
            .map_or(Resource::Queue, |id| Resource::Proposal { id, owner: None });
        self.refuse_ungranted(Capability::ProposalSubmit, &named)?;
        let resource = match submission.proposal {
            Some(id) => self.proposal(id)?,
            None => Resource::Queue,
        };
        self.authorize(Capability::ProposalSubmit, resource)?;
        self.store.submit(submission, findings)
    }

    /// `proposal withdraw`: a planner's own proposal only.
    pub fn withdraw(&mut self, proposal: ProposalId) -> Result<Proposal> {
        let named = Resource::Proposal {
            id: proposal,
            owner: None,
        };
        self.refuse_ungranted(Capability::ProposalWithdraw, &named)?;
        let resource = self.proposal(proposal)?;
        self.authorize(Capability::ProposalWithdraw, resource)?;
        self.store.withdraw_proposal(proposal)
    }

    pub fn add_goal(&mut self, goal: NewGoal) -> Result<Goal> {
        self.authorize(Capability::GoalWrite, Resource::Queue)?;
        self.store.add_goal(goal)
    }

    pub fn edit_goal(&mut self, goal: GoalId, edit: GoalEdit) -> Result<Goal> {
        self.authorize(Capability::GoalWrite, Resource::Goal(goal))?;
        self.store.edit_goal(goal, edit)
    }

    pub fn ready_goal(&mut self, goal: GoalId) -> Result<Goal> {
        self.authorize(Capability::GoalReady, Resource::Goal(goal))?;
        self.store.ready_goal(goal)
    }

    pub fn close_goal(&mut self, goal: GoalId, verdict: GoalVerdict) -> Result<Goal> {
        self.authorize(Capability::GoalClose, Resource::Goal(goal))?;
        self.store.close_goal(goal, verdict)
    }

    /// `goal review`: review the goal again.
    pub fn review_goal(&mut self, goal: GoalId) -> Result<Value> {
        self.authorize(Capability::GoalReviewRequest, Resource::Goal(goal))?;
        self.store.rearm_goal_review(goal)
    }

    /// The proposal and its owner. An owner that names a role but no one
    /// actor (`planner`, written by a session opened before
    /// `DAGQ_ACTOR_ID`) is no owner: every such planner would match it.
    fn proposal(&self, id: ProposalId) -> Result<Resource> {
        let owner = self
            .store
            .proposal_owner(id)?
            .filter(|owner| owner.contains(':'));
        Ok(Resource::Proposal { id, owner })
    }

    fn authorize_task(&mut self, capability: Capability, id: TaskId) -> Result<()> {
        self.refuse_ungranted(capability, &Resource::task(id))?;
        let status = self.store.task_status(id)?;
        self.authorize(
            capability,
            Resource::Task {
                id,
                status: Some(status),
            },
        )
    }

    /// Refuse, before the store is read, a capability the actor has on no
    /// resource: a role without it is refused whether or not the task or
    /// proposal exists. `named` is the resource as the command names it,
    /// its status and owner unknown; what they decide is left to
    /// [`Self::authorize`].
    fn refuse_ungranted(&mut self, capability: Capability, named: &Resource) -> Result<()> {
        match self.authorizer.authorize(self.actor, capability, named) {
            Err(error) if error.reason != DenyReason::Resource => {
                self.record(&error, named);
                Err(error.into())
            }
            _ => Ok(()),
        }
    }

    /// Ask the authorizer; a refusal is recorded (on the queue, as the
    /// refused actor) and returned as the [`AuthorizationError`]. A record
    /// that cannot be written does not turn the refusal into another error.
    fn authorize(&mut self, capability: Capability, resource: Resource) -> Result<()> {
        let Err(error) = self.authorizer.authorize(self.actor, capability, &resource) else {
            return Ok(());
        };
        self.record(&error, &resource);
        Err(error.into())
    }

    fn record(&self, error: &AuthorizationError, resource: &Resource) {
        if let Err(record) = self.store.record_denial(super::denial(error, resource)) {
            warn!("could not record the refusal ({error}): {record:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use anyhow::anyhow;

    use super::*;
    use crate::domain::{ActorRole, PlannerOrigin, PlannerOwner, RunId, StaticPolicy};
    use serde_json::json;

    /// A store that knows task statuses and proposal owners, records the
    /// refusals, and answers every change with the error `store: <what>`,
    /// so a test sees whether a command reached it.
    #[derive(Default)]
    struct Store {
        status: Option<TaskStatus>,
        owner: Option<String>,
        denials: RefCell<Vec<Value>>,
        record_fails: bool,
    }

    fn reached(what: &str) -> anyhow::Error {
        anyhow!("store: {what}")
    }

    impl PlanningStore for Store {
        fn task_status(&self, _: TaskId) -> Result<TaskStatus> {
            self.status.ok_or_else(|| anyhow!("task does not exist"))
        }
        fn proposal_owner(&self, _: ProposalId) -> Result<Option<String>> {
            Ok(self.owner.clone())
        }
        fn record_denial(&self, payload: Value) -> Result<()> {
            if self.record_fails {
                return Err(anyhow!("read-only"));
            }
            self.denials.borrow_mut().push(payload);
            Ok(())
        }
        fn add(&mut self, _: NewTask) -> Result<Task> {
            Err(reached("add"))
        }
        fn edit_task(&mut self, _: TaskId, _: TaskEdit) -> Result<Task> {
            Err(reached("edit"))
        }
        fn set_goal(&mut self, _: TaskId, _: Option<GoalId>) -> Result<Task> {
            Err(reached("set-goal"))
        }
        fn set_paths(&mut self, _: TaskId, _: Vec<String>) -> Result<Task> {
            Err(reached("set-paths"))
        }
        fn set_priority(&mut self, _: TaskId, _: Priority) -> Result<Task> {
            Err(reached("set-priority"))
        }
        fn transition(&mut self, _: TaskId, action: TaskAction) -> Result<Task> {
            Err(reached(&format!("{action:?}")))
        }
        fn cancel_duplicate(&mut self, _: TaskId, _: TaskId) -> Result<Task> {
            Err(reached("cancel-duplicate"))
        }
        fn add_dependency(&mut self, _: TaskId, _: Dependency) -> Result<()> {
            Err(reached("dependency add"))
        }
        fn remove_dependency(&mut self, _: TaskId, _: Dependency) -> Result<()> {
            Err(reached("dependency remove"))
        }
        fn show(&mut self, _: TaskId) -> Result<TaskDetail> {
            Err(reached("show"))
        }
        fn submit(&mut self, _: Submission, _: &[FindingId]) -> Result<Proposal> {
            Err(reached("submit"))
        }
        fn withdraw_proposal(&mut self, _: ProposalId) -> Result<Proposal> {
            Err(reached("withdraw"))
        }
        fn add_goal(&mut self, _: NewGoal) -> Result<Goal> {
            Err(reached("goal add"))
        }
        fn edit_goal(&mut self, _: GoalId, _: GoalEdit) -> Result<Goal> {
            Err(reached("goal edit"))
        }
        fn ready_goal(&mut self, _: GoalId) -> Result<Goal> {
            Err(reached("goal ready"))
        }
        fn close_goal(&mut self, _: GoalId, _: GoalVerdict) -> Result<Goal> {
            Err(reached("goal close"))
        }
        fn rearm_goal_review(&mut self, _: GoalId) -> Result<Value> {
            Err(reached("goal review"))
        }
    }

    type Command = fn(&mut Planning<'_, Store>) -> Result<()>;

    const TASK: TaskId = TaskId::new(1);
    const GOAL: GoalId = GoalId::new(1);

    fn new_task() -> NewTask {
        NewTask {
            title: "t".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Priority::default(),
            kind: None,
        }
    }

    fn submission(proposal: Option<ProposalId>) -> Submission {
        Submission {
            tasks: vec![TASK],
            goals: Vec::new(),
            proposal,
            owner: PlannerOwner {
                origin: PlannerOrigin::Person,
                workspace_id: None,
            },
        }
    }

    /// Every planning command, by name.
    fn commands() -> Vec<(&'static str, Command)> {
        vec![
            ("add", |p| p.add(new_task()).map(drop)),
            ("edit", |p| p.edit(TASK, TaskEdit::default()).map(drop)),
            ("set-goal", |p| p.set_goal(TASK, Some(GOAL)).map(drop)),
            ("set-paths", |p| {
                p.set_paths(TASK, vec!["docs/**".into()]).map(drop)
            }),
            ("set-priority", |p| {
                p.set_priority(TASK, Priority::High).map(drop)
            }),
            ("draft", |p| p.draft(TASK).map(drop)),
            ("ready", |p| p.ready(TASK, false).map(drop)),
            ("ready --bypass-review", |p| p.ready(TASK, true).map(drop)),
            ("cancel", |p| p.cancel(TASK, None).map(drop)),
            ("cancel --duplicate-of", |p| {
                p.cancel(TASK, Some(TaskId::new(2))).map(drop)
            }),
            ("dependency add", |p| {
                p.add_dependency(TASK, Dependency::Task(TaskId::new(2)))
                    .map(drop)
            }),
            ("dependency remove", |p| {
                p.remove_dependency(TASK, Dependency::Goal(GOAL)).map(drop)
            }),
            ("submit", |p| p.submit(submission(None), &[]).map(drop)),
            ("submit --proposal", |p| {
                p.submit(submission(Some(ProposalId::new(1))), &[])
                    .map(drop)
            }),
            ("proposal withdraw", |p| {
                p.withdraw(ProposalId::new(1)).map(drop)
            }),
            ("goal add", |p| p.add_goal(new_goal()).map(drop)),
            ("goal edit", |p| {
                p.edit_goal(GOAL, GoalEdit::default()).map(drop)
            }),
            ("goal ready", |p| p.ready_goal(GOAL).map(drop)),
            ("goal close", |p| {
                p.close_goal(GOAL, GoalVerdict::Achieved).map(drop)
            }),
            ("goal review", |p| p.review_goal(GOAL).map(drop)),
        ]
    }

    fn new_goal() -> NewGoal {
        NewGoal {
            title: "g".into(),
            description: String::new(),
            acceptance: String::new(),
            constraints: String::new(),
            doc: None,
            draft: false,
        }
    }

    enum Outcome {
        /// Reached the store.
        Allowed,
        Denied(AuthorizationError),
    }

    /// Run `command` as `actor` on a store whose task is `status` and whose
    /// proposal `owner` owns; the refusals it recorded come back too.
    fn run(
        actor: &ActorContext,
        status: TaskStatus,
        owner: Option<&str>,
        command: Command,
    ) -> (Outcome, Vec<Value>) {
        let mut store = Store {
            status: Some(status),
            owner: owner.map(str::to_owned),
            ..Store::default()
        };
        let error = command(&mut Planning::new(&mut store, actor, &StaticPolicy)).unwrap_err();
        let outcome = match error.downcast::<AuthorizationError>() {
            Ok(denied) => Outcome::Denied(denied),
            Err(error) => {
                assert!(error.to_string().starts_with("store: "), "{error:#}");
                Outcome::Allowed
            }
        };
        (outcome, store.denials.into_inner())
    }

    fn allowed(actor: &ActorContext, status: TaskStatus, owner: Option<&str>, name: &str) -> bool {
        let command = commands()
            .into_iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("no command {name}"))
            .1;
        matches!(run(actor, status, owner, command).0, Outcome::Allowed)
    }

    fn planner(id: i64) -> ActorContext {
        ActorContext::instance(ActorRole::Planner, id)
    }

    #[test]
    fn the_user_and_the_inbox_may_run_every_planning_command() {
        for actor in [
            ActorContext::user(),
            ActorContext::instance(ActorRole::Inbox, 1),
        ] {
            for (name, command) in commands() {
                let (outcome, denials) = run(&actor, TaskStatus::InProgress, None, command);
                assert!(matches!(outcome, Outcome::Allowed), "{actor:?} {name}");
                assert!(denials.is_empty());
            }
        }
    }

    #[test]
    fn workers_jobs_and_the_observer_may_run_no_planning_command() {
        let run_id = RunId::new("r1").unwrap();
        let mut actors = vec![
            ActorContext::worker(&run_id, TASK),
            ActorContext::instance(ActorRole::Observer, 1),
            ActorContext::instance(ActorRole::Wrapper, 1),
            ActorContext::instance(ActorRole::Integrator, 1),
        ];
        actors.extend(
            [
                ActorRole::ReviewJob,
                ActorRole::RecoveryJob,
                ActorRole::PlanReviewJob,
                ActorRole::GoalReviewJob,
            ]
            .map(|role| ActorContext::instance(role, 1)),
        );
        for actor in actors {
            for (name, command) in commands() {
                let (outcome, denials) = run(&actor, TaskStatus::Draft, None, command);
                let Outcome::Denied(error) = outcome else {
                    panic!("{actor:?} may {name}");
                };
                assert_eq!(
                    (error.role, error.reason),
                    (actor.role(), DenyReason::NotGranted)
                );
                assert_eq!(denials.len(), 1, "{name}");
                assert_eq!(denials[0]["role"], actor.role().as_str());
                assert_eq!(denials[0]["capability"], error.capability.as_str());
            }
        }
    }

    #[test]
    fn the_supervisor_has_only_its_own_transitions() {
        let supervisor = ActorContext::instance(ActorRole::Supervisor, 1);
        for (name, command) in commands() {
            let expected = matches!(
                name,
                "ready" | "cancel" | "cancel --duplicate-of" | "goal close"
            );
            let (outcome, _) = run(&supervisor, TaskStatus::Draft, None, command);
            assert_eq!(matches!(outcome, Outcome::Allowed), expected, "{name}");
        }
    }

    #[test]
    fn the_planner_keeps_its_authority_on_tasks_that_have_not_started() {
        let me = planner(7);
        for status in [TaskStatus::Draft, TaskStatus::Submitted, TaskStatus::Ready] {
            for name in [
                "add",
                "edit",
                "set-goal",
                "set-paths",
                "set-priority",
                "draft",
                "cancel",
                "cancel --duplicate-of",
                "dependency add",
                "dependency remove",
                "submit",
                "submit --proposal",
                "goal add",
                "goal edit",
                "goal close",
            ] {
                assert!(allowed(&me, status, None, name), "{status:?} {name}");
            }
        }
        // Its own proposal it may withdraw.
        assert!(allowed(
            &me,
            TaskStatus::Draft,
            Some("planner:7"),
            "proposal withdraw"
        ));
    }

    #[test]
    fn the_planner_may_not_ready_touch_a_started_task_or_withdraw_another_proposal() {
        let me = planner(7);
        for name in [
            "ready",
            "ready --bypass-review",
            "goal ready",
            "goal review",
        ] {
            assert!(!allowed(&me, TaskStatus::Draft, None, name), "{name}");
        }
        for status in [
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Canceled,
        ] {
            for name in [
                "cancel",
                "edit",
                "set-paths",
                "set-priority",
                "dependency add",
                "draft",
            ] {
                assert!(!allowed(&me, status, None, name), "{status:?} {name}");
            }
        }
        // `planner` (a session from before DAGQ_ACTOR_ID) names no one.
        let legacy = ActorContext::new(ActorRole::Planner, "planner");
        assert!(!allowed(
            &legacy,
            TaskStatus::Draft,
            Some("planner"),
            "proposal withdraw"
        ));
        for owner in [Some("planner:8"), Some("planner"), None] {
            let (outcome, denials) = run(&me, TaskStatus::Draft, owner, |p| {
                p.withdraw(ProposalId::new(3)).map(drop)
            });
            let Outcome::Denied(error) = outcome else {
                panic!("withdrew the proposal of {owner:?}");
            };
            assert_eq!(error.reason, DenyReason::Resource);
            assert_eq!(
                denials,
                [json!({
                    "event": "authorization_denied",
                    "role": "planner",
                    "capability": "proposal.withdraw",
                    "reason": "not on this resource",
                    "resource": {"kind": "proposal", "id": 3,
                                 "owner": owner.filter(|o| o.contains(':'))},
                })]
            );
        }
    }

    #[test]
    fn a_task_the_store_cannot_find_is_its_error_not_a_refusal() {
        let mut store = Store::default();
        let me = planner(1);
        let error = Planning::new(&mut store, &me, &StaticPolicy)
            .cancel(TASK, None)
            .unwrap_err();
        assert_eq!(error.to_string(), "task does not exist");
        assert!(store.denials.into_inner().is_empty());
    }

    #[test]
    fn a_role_without_the_capability_is_refused_before_the_store_is_read() {
        let mut store = Store::default();
        let observer = ActorContext::instance(ActorRole::Observer, 1);
        let error = Planning::new(&mut store, &observer, &StaticPolicy)
            .cancel(TASK, None)
            .unwrap_err();
        let denied = error.downcast::<AuthorizationError>().unwrap();
        assert_eq!(denied.reason, DenyReason::NotGranted);
        assert_eq!(
            store.denials.into_inner()[0]["resource"],
            json!({"kind": "task", "id": 1, "status": null})
        );
    }

    #[test]
    fn a_refusal_stands_when_its_record_fails() {
        let mut store = Store {
            record_fails: true,
            ..Store::default()
        };
        let observer = ActorContext::instance(ActorRole::Observer, 1);
        let error = Planning::new(&mut store, &observer, &StaticPolicy)
            .add_goal(new_goal())
            .unwrap_err();
        assert_eq!(
            error.downcast::<AuthorizationError>().unwrap().to_string(),
            "observer may not goal.write (not granted)"
        );
    }
}
