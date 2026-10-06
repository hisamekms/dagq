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
    ActorContext, AuthorizationError, Authorizer, Capability, DraftRevisit, FindingId, Goal,
    GoalEdit, GoalId, GoalVerdict, NewGoal, NewTask, Priority, Proposal, ProposalId, Resource,
    Submission, Task, TaskAction, TaskDetail, TaskEdit, TaskId, TaskStatus,
    authorization::DenyReason, follow_up::RevisitChange,
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
    /// `authorized` is the status the edit was authorized with; the store
    /// refuses the edit when the task has another one in its transaction.
    fn edit_task(&mut self, task: TaskId, edit: TaskEdit, authorized: TaskStatus) -> Result<Task>;
    fn judge_follow_up(
        &mut self,
        task: TaskId,
        judgement: crate::domain::follow_up::MembershipJudgement,
        role: &str,
    ) -> Result<Value>;
    // Like `edit_task`, each change of a task below gets the status it was
    // authorized with and refuses, changing nothing, a task with another
    // one in the store's transaction (task 1609).
    fn set_goal(
        &mut self,
        task: TaskId,
        goal: Option<GoalId>,
        authorized: TaskStatus,
    ) -> Result<Task>;
    fn set_paths(
        &mut self,
        task: TaskId,
        paths: Vec<String>,
        authorized: TaskStatus,
    ) -> Result<Task>;
    fn set_priority(
        &mut self,
        task: TaskId,
        priority: Option<Priority>,
        authorized: TaskStatus,
    ) -> Result<Task>;
    /// Set, change or clear the revisit time of the draft `task` as `role`
    /// with actor id `actor` (ADR-t1540-1): the revisit time afterwards.
    fn revisit_draft(
        &mut self,
        task: TaskId,
        change: RevisitChange,
        role: &str,
        actor: &str,
    ) -> Result<Option<DraftRevisit>>;
    fn transition(
        &mut self,
        task: TaskId,
        action: TaskAction,
        authorized: TaskStatus,
    ) -> Result<Task>;
    fn cancel_duplicate(
        &mut self,
        task: TaskId,
        duplicate_of: TaskId,
        authorized: TaskStatus,
    ) -> Result<Task>;
    fn add_dependency(
        &mut self,
        task: TaskId,
        on: Dependency,
        authorized: TaskStatus,
    ) -> Result<()>;
    fn remove_dependency(
        &mut self,
        task: TaskId,
        on: Dependency,
        authorized: TaskStatus,
    ) -> Result<()>;
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

    /// `add`: a draft task, in `goal` when it names one. The retired
    /// interactive worker mode is refused (ADR-t1433-2), as by `edit`.
    pub fn add(&mut self, task: NewTask) -> Result<Task> {
        let resource = task.goal_id.map_or(Resource::Queue, Resource::Goal);
        self.authorize(Capability::TaskWrite, resource)?;
        crate::domain::worker::refuse_interactive(task.worker_mode)?;
        self.store.add(task)
    }

    pub fn edit(&mut self, task: TaskId, edit: TaskEdit) -> Result<Task> {
        self.refuse_ungranted(Capability::TaskWrite, &Resource::task(task))?;
        let status = self.store.task_status(task)?;
        let capability = if status == TaskStatus::InProgress && edit.verify_only() {
            Capability::TaskVerifyEdit
        } else {
            Capability::TaskWrite
        };
        // Authorized with the status the capability was chosen from, the
        // one the store checks again in its transaction (ADR-t883-1).
        self.refuse_ungranted(capability, &Resource::task(task))?;
        self.authorize(
            capability,
            Resource::Task {
                id: task,
                status: Some(status),
            },
        )?;
        crate::domain::worker::refuse_interactive(edit.worker_mode)?;
        self.store.edit_task(task, edit, status)
    }

    pub fn judge_follow_up(
        &mut self,
        task: TaskId,
        judgement: crate::domain::follow_up::MembershipJudgement,
    ) -> Result<Value> {
        self.authorize(Capability::FollowUpJudge, Resource::task(task))?;
        self.store
            .judge_follow_up(task, judgement, self.actor.role().as_str())
    }

    pub fn set_goal(&mut self, task: TaskId, goal: Option<GoalId>) -> Result<Task> {
        let status = self.authorize_task(Capability::TaskWrite, task)?;
        self.store.set_goal(task, goal, status)
    }

    pub fn set_paths(&mut self, task: TaskId, paths: Vec<String>) -> Result<Task> {
        let status = self.authorize_task(Capability::TaskWrite, task)?;
        self.store.set_paths(task, paths, status)
    }

    pub fn set_priority(&mut self, task: TaskId, priority: Option<Priority>) -> Result<Task> {
        let status = self.authorize_task(Capability::TaskWrite, task)?;
        self.store.set_priority(task, priority, status)
    }

    /// `revisit`: the draft's revisit time set, changed or cleared
    /// (ADR-t1540-1). It changes a task before it starts, as `task.write`:
    /// the user's and the inbox's, and a planner's on a draft.
    pub fn revisit(&mut self, task: TaskId, change: RevisitChange) -> Result<Option<DraftRevisit>> {
        self.authorize_task(Capability::TaskWrite, task)?;
        let (role, actor) = (self.actor.role(), self.actor.actor_id().to_owned());
        self.store
            .revisit_draft(task, change, role.as_str(), &actor)
    }

    pub fn draft(&mut self, task: TaskId) -> Result<Task> {
        let status = self.authorize_task(Capability::TaskWrite, task)?;
        self.store.transition(task, TaskAction::Draft, status)
    }

    /// `ready`, or `ready --bypass-review` past plan review: the user's and
    /// the inbox's at a person's word, which the event's actor tells apart.
    pub fn ready(&mut self, task: TaskId, bypass_review: bool) -> Result<Task> {
        let (capability, action) = if bypass_review {
            (Capability::TaskReadyBypassReview, TaskAction::BypassReview)
        } else {
            (Capability::TaskReady, TaskAction::Ready)
        };
        let status = self.authorize_task(capability, task)?;
        self.store.transition(task, action, status)
    }

    /// `cancel`, as a duplicate of another task when it names one.
    pub fn cancel(&mut self, task: TaskId, duplicate_of: Option<TaskId>) -> Result<Task> {
        let status = self.authorize_task(Capability::TaskCancel, task)?;
        match duplicate_of {
            Some(other) => self.store.cancel_duplicate(task, other, status),
            None => self.store.transition(task, TaskAction::Cancel, status),
        }
    }

    /// `dependency add`: the task as it is afterwards.
    pub fn add_dependency(&mut self, task: TaskId, on: Dependency) -> Result<TaskDetail> {
        let status = self.authorize_task(Capability::TaskWrite, task)?;
        self.store.add_dependency(task, on, status)?;
        self.store.show(task)
    }

    /// `dependency remove`: the task as it is afterwards.
    pub fn remove_dependency(&mut self, task: TaskId, on: Dependency) -> Result<TaskDetail> {
        let status = self.authorize_task(Capability::TaskWrite, task)?;
        self.store.remove_dependency(task, on, status)?;
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

    /// Authorize `capability` on the task as the store holds it now; the
    /// status it was authorized with is returned for the store to check
    /// again in its transaction (task 1609).
    fn authorize_task(&mut self, capability: Capability, id: TaskId) -> Result<TaskStatus> {
        self.refuse_ungranted(capability, &Resource::task(id))?;
        let status = self.store.task_status(id)?;
        self.authorize(
            capability,
            Resource::Task {
                id,
                status: Some(status),
            },
        )?;
        Ok(status)
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
    use crate::domain::worker::WorkerMode;
    use crate::domain::{ActorRole, DomainError, PlannerOrigin, PlannerOwner, RunId, StaticPolicy};
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
        /// The status the last change of a task was handed as the
        /// authorized one.
        authorized_as: Option<TaskStatus>,
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
        fn edit_task(&mut self, _: TaskId, _: TaskEdit, authorized: TaskStatus) -> Result<Task> {
            self.authorized_as = Some(authorized);
            Err(reached("edit"))
        }
        fn judge_follow_up(
            &mut self,
            _: TaskId,
            _: crate::domain::follow_up::MembershipJudgement,
            _: &str,
        ) -> Result<Value> {
            unreachable!()
        }
        fn set_goal(
            &mut self,
            _: TaskId,
            _: Option<GoalId>,
            authorized: TaskStatus,
        ) -> Result<Task> {
            self.authorized_as = Some(authorized);
            Err(reached("set-goal"))
        }
        fn set_paths(&mut self, _: TaskId, _: Vec<String>, authorized: TaskStatus) -> Result<Task> {
            self.authorized_as = Some(authorized);
            Err(reached("set-paths"))
        }
        fn set_priority(
            &mut self,
            _: TaskId,
            _: Option<Priority>,
            authorized: TaskStatus,
        ) -> Result<Task> {
            self.authorized_as = Some(authorized);
            Err(reached("set-priority"))
        }
        fn revisit_draft(
            &mut self,
            _: TaskId,
            _: RevisitChange,
            _: &str,
            _: &str,
        ) -> Result<Option<DraftRevisit>> {
            Err(reached("revisit"))
        }
        fn transition(
            &mut self,
            _: TaskId,
            action: TaskAction,
            authorized: TaskStatus,
        ) -> Result<Task> {
            self.authorized_as = Some(authorized);
            Err(reached(&format!("{action:?}")))
        }
        fn cancel_duplicate(
            &mut self,
            _: TaskId,
            _: TaskId,
            authorized: TaskStatus,
        ) -> Result<Task> {
            self.authorized_as = Some(authorized);
            Err(reached("cancel-duplicate"))
        }
        fn add_dependency(
            &mut self,
            _: TaskId,
            _: Dependency,
            authorized: TaskStatus,
        ) -> Result<()> {
            self.authorized_as = Some(authorized);
            Err(reached("dependency add"))
        }
        fn remove_dependency(
            &mut self,
            _: TaskId,
            _: Dependency,
            authorized: TaskStatus,
        ) -> Result<()> {
            self.authorized_as = Some(authorized);
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
            priority: None,
            change: None,
            provider: None,
            worker_mode: None,
            wait_for_build: false,
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
                p.set_priority(TASK, Some(Priority::High)).map(drop)
            }),
            ("revisit", |p| {
                p.revisit(TASK, RevisitChange::Set { at: 1, note: None })
                    .map(drop)
            }),
            ("revisit --clear", |p| {
                p.revisit(TASK, RevisitChange::Clear).map(drop)
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
            priority: Default::default(),
            title: "g".into(),
            description: String::new(),
            acceptance: String::new(),
            constraints: String::new(),
            doc: None,
            draft: false,
            tags: Vec::new(),
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

    /// ADR-t1433-2: `add` and `edit` refuse the interactive worker mode
    /// with its reason and do not reach the store; `--headless` does.
    #[test]
    fn add_and_edit_refuse_the_interactive_worker_before_the_store() {
        let user = ActorContext::user();
        let interactive: [Command; 2] = [
            |p| {
                p.add(NewTask {
                    worker_mode: Some(WorkerMode::Interactive),
                    ..new_task()
                })
                .map(drop)
            },
            |p| {
                p.edit(
                    TASK,
                    TaskEdit {
                        worker_mode: Some(WorkerMode::Interactive),
                        ..TaskEdit::default()
                    },
                )
                .map(drop)
            },
        ];
        for command in interactive {
            let mut store = Store {
                status: Some(TaskStatus::Draft),
                ..Store::default()
            };
            let error = command(&mut Planning::new(&mut store, &user, &StaticPolicy)).unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<DomainError>(),
                    Some(DomainError::InteractiveWorkerRetired)
                ),
                "{error:#}"
            );
        }
        let headless: Command = |p| {
            p.edit(
                TASK,
                TaskEdit {
                    worker_mode: Some(WorkerMode::Headless),
                    ..TaskEdit::default()
                },
            )
            .map(drop)
        };
        assert!(matches!(
            run(&user, TaskStatus::Draft, None, headless).0,
            Outcome::Allowed
        ));
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
                ActorRole::ThroughputReviewJob,
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
                "revisit",
                "revisit --clear",
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
                "revisit",
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

    /// ADR-t883-1: the store gets the status the edit was authorized
    /// with, so it can refuse a task that moved on before its transaction.
    #[test]
    fn an_edit_hands_the_store_the_status_it_was_authorized_with() {
        let verify = || TaskEdit {
            verification_commands: Some(vec!["true".into()]),
            ..TaskEdit::default()
        };
        for (actor, status) in [
            (planner(7), TaskStatus::Ready),
            (planner(7), TaskStatus::Draft),
            (ActorContext::user(), TaskStatus::InProgress),
            (
                ActorContext::instance(ActorRole::Inbox, 1),
                TaskStatus::InProgress,
            ),
        ] {
            let mut store = Store {
                status: Some(status),
                ..Store::default()
            };
            let error = Planning::new(&mut store, &actor, &StaticPolicy)
                .edit(TASK, verify())
                .unwrap_err();
            assert_eq!(error.to_string(), "store: edit", "{actor:?} {status:?}");
            assert_eq!(store.authorized_as, Some(status), "{actor:?}");
        }
    }

    /// Task 1609: every other change of a task hands the store the status
    /// it was authorized with too, `ready` and `cancel` included.
    #[test]
    fn each_change_of_a_task_hands_the_store_the_status_it_was_authorized_with() {
        type Command = fn(&mut Planning<'_, Store>) -> Result<()>;
        let commands: [(&str, Command); 10] = [
            ("store: set-goal", |p| {
                p.set_goal(TASK, Some(GOAL)).map(drop)
            }),
            ("store: set-paths", |p| p.set_paths(TASK, vec![]).map(drop)),
            ("store: set-priority", |p| {
                p.set_priority(TASK, None).map(drop)
            }),
            ("store: Draft", |p| p.draft(TASK).map(drop)),
            ("store: Ready", |p| p.ready(TASK, false).map(drop)),
            ("store: BypassReview", |p| p.ready(TASK, true).map(drop)),
            ("store: Cancel", |p| p.cancel(TASK, None).map(drop)),
            ("store: cancel-duplicate", |p| {
                p.cancel(TASK, Some(TaskId::new(2))).map(drop)
            }),
            ("store: dependency add", |p| {
                p.add_dependency(TASK, Dependency::Goal(GOAL)).map(drop)
            }),
            ("store: dependency remove", |p| {
                p.remove_dependency(TASK, Dependency::Task(TaskId::new(2)))
                    .map(drop)
            }),
        ];
        for status in [TaskStatus::Draft, TaskStatus::Submitted] {
            for (reached, command) in commands {
                let mut store = Store {
                    status: Some(status),
                    ..Store::default()
                };
                let me = ActorContext::user();
                let error =
                    command(&mut Planning::new(&mut store, &me, &StaticPolicy)).unwrap_err();
                assert_eq!(error.to_string(), reached, "{status:?}");
                assert_eq!(store.authorized_as, Some(status), "{reached}");
            }
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
