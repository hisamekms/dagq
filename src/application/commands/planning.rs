//! The planning commands (task 732): goals, tasks before they run, their
//! dependencies and proposals. Each is authorized as the caller before it
//! reaches the store, with the task's status, the proposal it is in and the
//! proposal's owner read from the store, so the planner's rules (tasks in
//! draft, submitted or ready only, and not of another planner's revising
//! proposal; its own proposal only) apply to what the queue holds rather
//! than to what the command line says. The policy is the
//! [`crate::domain::StaticPolicy`]'s (`docs/design/authorization.md`).

use anyhow::{Result, ensure};
use serde_json::{Value, json};
use tracing::warn;

use std::path::{Path, PathBuf};

use crate::application::container_image::KeptImageRecord;
use crate::application::inherit::{
    CarriedBranches, InheritReadAt, InheritRequest, InheritRunAt, InheritStore, InheritedByHand,
    keep_head,
};
use crate::application::queue_commands::{
    InheritWork, QueueCommand, ReadyInheritImagesParams, ReadyInheritParams, ReadyInheritToRead,
};

use crate::domain::{
    ActorContext, AuthorizationError, Authorizer, Capability, CommitSha, DomainError, DraftRevisit,
    FindingId, Goal, GoalEdit, GoalId, GoalVerdict, NewGoal, NewTask, Priority, Proposal,
    ProposalId, Resource, Submission, Task, TaskAction, TaskAuthorization, TaskDetail, TaskEdit,
    TaskId, TaskStatus, authorization::DenyReason, cancel_reason::NewCancel,
    follow_up::RevisitChange,
};

/// What the planning commands read and change of the queue.
pub trait PlanningStore {
    /// The status of `task` and the proposal it is in, with that
    /// proposal's status and owner; a missing task is an error.
    fn task_authorization(&self, task: TaskId) -> Result<TaskAuthorization>;
    /// The actor id of the planner that owns `proposal`, `None` when the
    /// queue did not record it; a missing proposal is an error.
    fn proposal_owner(&self, proposal: ProposalId) -> Result<Option<String>>;
    /// Record a refusal as the queue event `authorization_denied`.
    fn record_denial(&self, payload: Value) -> Result<()>;

    fn add(&mut self, task: NewTask) -> Result<Task>;
    /// `authorized` is what the edit was authorized with; the store
    /// refuses the edit when the task has another status, proposal,
    /// proposal status or owner in its transaction.
    fn edit_task(
        &mut self,
        task: TaskId,
        edit: TaskEdit,
        authorized: TaskAuthorization,
    ) -> Result<Task>;
    fn judge_follow_up(
        &mut self,
        task: TaskId,
        judgement: crate::domain::follow_up::MembershipJudgement,
        role: &str,
    ) -> Result<Value>;
    // Like `edit_task`, each change of a task below gets what it was
    // authorized with and refuses, changing nothing, a task with another
    // status or proposal in the store's transaction (task 1609).
    fn set_goal(
        &mut self,
        task: TaskId,
        goal: Option<GoalId>,
        authorized: TaskAuthorization,
    ) -> Result<Task>;
    fn set_paths(
        &mut self,
        task: TaskId,
        paths: Vec<String>,
        authorized: TaskAuthorization,
    ) -> Result<Task>;
    fn set_priority(
        &mut self,
        task: TaskId,
        priority: Option<Priority>,
        authorized: TaskAuthorization,
    ) -> Result<Task>;
    /// Set, change or clear the revisit time of the draft `task` as `role`
    /// with actor id `actor` (ADR-t1540-1): the revisit time afterwards.
    fn revisit_draft(
        &mut self,
        task: TaskId,
        change: RevisitChange,
        role: &str,
        actor: &str,
        authorized: TaskAuthorization,
    ) -> Result<Option<DraftRevisit>>;
    fn transition(
        &mut self,
        task: TaskId,
        action: TaskAction,
        authorized: TaskAuthorization,
    ) -> Result<Task>;
    fn cancel(
        &mut self,
        task: TaskId,
        cancel: NewCancel,
        authorized: TaskAuthorization,
    ) -> Result<Task>;
    fn add_dependency(
        &mut self,
        task: TaskId,
        on: Dependency,
        authorized: TaskAuthorization,
    ) -> Result<()>;
    fn remove_dependency(
        &mut self,
        task: TaskId,
        on: Dependency,
        authorized: TaskAuthorization,
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
        self.authorize(Capability::TaskWrite, resource.clone())?;
        crate::domain::worker::refuse_interactive(task.worker_mode)?;
        let result = self.store.add(task);
        self.recorded(result, Capability::TaskWrite, &resource)
    }

    pub fn edit(&mut self, task: TaskId, edit: TaskEdit) -> Result<Task> {
        self.refuse_ungranted(Capability::TaskWrite, &Resource::task(task))?;
        let authorized = self.store.task_authorization(task)?;
        let capability = if authorized.status == TaskStatus::InProgress && edit.verify_only() {
            Capability::TaskVerifyEdit
        } else {
            Capability::TaskWrite
        };
        // Authorized with the status and proposal the capability was chosen
        // from, the ones the store checks again in its transaction
        // (ADR-t883-1).
        self.refuse_ungranted(capability, &Resource::task(task))?;
        self.authorize(capability, authorized.resource(task))?;
        crate::domain::worker::refuse_interactive(edit.worker_mode)?;
        self.store.edit_task(task, edit, authorized)
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

    /// `set-goal`: the store refuses a planner's move of a `goal_gap`
    /// draft out of its goal (ADR-t1504-2 decision 10), and the refusal is
    /// recorded after the store's transaction rolled back, naming the rule
    /// and both goals.
    pub fn set_goal(&mut self, task: TaskId, goal: Option<GoalId>) -> Result<Task> {
        let authorized = self.authorize_task(Capability::TaskWrite, task)?;
        let resource = authorized.resource(task);
        let result = self.store.set_goal(task, goal, authorized);
        if let Err(error) = &result
            && let Some(DomainError::GoalGapMoved { from, to, .. }) =
                error.downcast_ref::<DomainError>()
        {
            let refusal = AuthorizationError {
                role: self.actor.role(),
                capability: Capability::TaskWrite,
                reason: DenyReason::GoalGapDraft,
            };
            let mut payload = super::denial(&refusal, &resource);
            payload["rule"] = json!("goal_gap draft stays in its goal");
            payload["from_goal_id"] = json!(from);
            payload["to_goal_id"] = json!(to);
            if let Err(record) = self.store.record_denial(payload) {
                warn!("could not record the refusal ({refusal}): {record:#}");
            }
        }
        result
    }

    pub fn set_paths(&mut self, task: TaskId, paths: Vec<String>) -> Result<Task> {
        let authorized = self.authorize_task(Capability::TaskWrite, task)?;
        self.store.set_paths(task, paths, authorized)
    }

    /// `set-priority`: the store refuses to change a person's priority
    /// for an actor other than a person (ADR-t1975-1 decision 3), and the
    /// refusal is recorded as the authorizer's are.
    pub fn set_priority(&mut self, task: TaskId, priority: Option<Priority>) -> Result<Task> {
        let authorized = self.authorize_task(Capability::TaskWrite, task)?;
        let resource = authorized.resource(task);
        let result = self.store.set_priority(task, priority, authorized);
        self.recorded(result, Capability::TaskWrite, &resource)
    }

    /// `revisit`: the draft's revisit time set, changed or cleared
    /// (ADR-t1540-1). It changes a task before it starts, as `task.write`:
    /// the user's and the inbox's, and a planner's on a draft.
    pub fn revisit(&mut self, task: TaskId, change: RevisitChange) -> Result<Option<DraftRevisit>> {
        let authorized = self.authorize_task(Capability::TaskWrite, task)?;
        let (role, actor) = (self.actor.role(), self.actor.actor_id().to_owned());
        self.store
            .revisit_draft(task, change, role.as_str(), &actor, authorized)
    }

    pub fn draft(&mut self, task: TaskId) -> Result<Task> {
        let authorized = self.authorize_task(Capability::TaskWrite, task)?;
        self.store.transition(task, TaskAction::Draft, authorized)
    }

    /// `ready`, or `ready --bypass-review` past plan review: the user's and
    /// the inbox's at a person's word, which the event's actor tells apart.
    pub fn ready(&mut self, task: TaskId, bypass_review: bool) -> Result<Task> {
        let (capability, action) = if bypass_review {
            (Capability::TaskReadyBypassReview, TaskAction::BypassReview)
        } else {
            (Capability::TaskReady, TaskAction::Ready)
        };
        let authorized = self.authorize_task(capability, task)?;
        self.store.transition(task, action, authorized)
    }

    /// `cancel` with its reason and the task that holds its content, when
    /// it names one (ADR-t947-4): the store refuses one without either.
    pub fn cancel(&mut self, task: TaskId, cancel: NewCancel) -> Result<Task> {
        let authorized = self.authorize_task(Capability::TaskCancel, task)?;
        self.store.cancel(task, cancel, authorized)
    }

    /// `dependency add`: the task as it is afterwards.
    pub fn add_dependency(&mut self, task: TaskId, on: Dependency) -> Result<TaskDetail> {
        let authorized = self.authorize_task(Capability::TaskWrite, task)?;
        self.store.add_dependency(task, on, authorized)?;
        self.store.show(task)
    }

    /// `dependency remove`: the task as it is afterwards.
    pub fn remove_dependency(&mut self, task: TaskId, on: Dependency) -> Result<TaskDetail> {
        let authorized = self.authorize_task(Capability::TaskWrite, task)?;
        self.store.remove_dependency(task, on, authorized)?;
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
        let result = self.store.add_goal(goal);
        self.recorded(result, Capability::GoalWrite, &Resource::Queue)
    }

    pub fn edit_goal(&mut self, goal: GoalId, edit: GoalEdit) -> Result<Goal> {
        self.authorize(Capability::GoalWrite, Resource::Goal(goal))?;
        let result = self.store.edit_goal(goal, edit);
        self.recorded(result, Capability::GoalWrite, &Resource::Goal(goal))
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

    /// Authorize `capability` on the task as the store holds it now; what
    /// it was authorized with (its status and the proposal it is in) is
    /// returned for the store to check again in its transaction (task 1609).
    fn authorize_task(&mut self, capability: Capability, id: TaskId) -> Result<TaskAuthorization> {
        self.refuse_ungranted(capability, &Resource::task(id))?;
        let authorized = self.store.task_authorization(id)?;
        self.authorize(capability, authorized.resource(id))?;
        Ok(authorized)
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

    /// `result` of a change the store refused because it would change a
    /// person's priority (ADR-t1975-1 decisions 2 and 3: another value
    /// than the request's, or a change of one a person set) is recorded
    /// as a refusal, `a person's priority`, and returned as it is, naming
    /// the person's value.
    fn recorded<T>(
        &self,
        result: Result<T>,
        capability: Capability,
        resource: &Resource,
    ) -> Result<T> {
        if let Err(error) = &result
            && matches!(
                error.downcast_ref::<DomainError>(),
                Some(
                    DomainError::PersonsPriority { .. }
                        | DomainError::RequestPriorityDiffers { .. }
                )
            )
        {
            let refusal = AuthorizationError {
                role: self.actor.role(),
                capability,
                reason: DenyReason::PersonsPriority,
            };
            self.record(&refusal, resource);
        }
        result
    }

    fn record(&self, error: &AuthorizationError, resource: &Resource) {
        if let Err(record) = self.store.record_denial(super::denial(error, resource)) {
            warn!("could not record the refusal ({error}): {record:#}");
        }
    }
}

impl<S: PlanningStore + InheritStore + ?Sized> Planning<'_, S> {
    /// What `ready --inherit` reads before its head is chosen (a person's,
    /// the user's or the inbox's at their word, retry of an `in_progress`
    /// task whose latest run failed or was interrupted, carrying that run's
    /// branch over to the next run without plan review, ADR-t1962-1): the
    /// task's status it is authorized with (`task.ready_bypass_review`) and
    /// its latest run, which the control side chooses the head of and
    /// writes under `refs/dagq/runs/<run-id>` with its own Git (ADR-t3102-3
    /// decision 5, ADR-t3294-1, [`crate::application::inherit::keep_head`])
    /// before it asks for [`Self::ready_inherit_by_caller`].
    pub fn ready_inherit_read(&mut self, task: TaskId, reason: &str) -> Result<InheritToRead> {
        let status = self
            .authorize_task(Capability::TaskReadyBypassReview, task)?
            .status;
        check_reason(reason)?;
        let run = self.store.show(task)?.runs.pop();
        Ok(InheritToRead {
            at: InheritReadAt {
                task: status,
                run: run.as_ref().map(InheritRunAt::of),
            },
            run,
        })
    }

    /// The records of the image the client of `ready --inherit` of `task`
    /// made ready for a run's facts-only turn as it chose the head,
    /// recorded as the caller's events once authorized as
    /// [`Self::ready_inherit_read`] is, whether the head was chosen or not;
    /// how many. Each is stamped at `now`, the time it is recorded here
    /// (Unix seconds), as a release's are
    /// ([`super::holds::Holds::record_images`]).
    pub fn ready_inherit_images(
        &mut self,
        task: TaskId,
        records: &[KeptImageRecord],
        now: i64,
    ) -> Result<usize> {
        self.authorize_task(Capability::TaskReadyBypassReview, task)?;
        if !records.is_empty() {
            let records: Vec<_> = records.iter().map(|record| record.recorded(now)).collect();
            InheritStore::record_images(&mut *self.store, &records)?;
        }
        Ok(records.len())
    }

    /// `ready --inherit` with the head its caller chose and wrote, from what
    /// [`Self::ready_inherit_read`] read (`at`): authorized again, and
    /// recorded in the store's one transaction, which checks `at` again
    /// (the task's status it was authorized with, its latest run, and that
    /// run's status and validated commit) and refuses, changing nothing,
    /// when any moved since ([`InheritStore`]). No Git runs here.
    pub fn ready_inherit_by_caller(
        &mut self,
        task: TaskId,
        reason: &str,
        at: InheritReadAt,
        head: Option<CommitSha>,
    ) -> Result<InheritedByHand> {
        let authorized = self
            .authorize_task(Capability::TaskReadyBypassReview, task)?
            .status;
        check_reason(reason)?;
        InheritStore::inherit_by_hand(
            &mut *self.store,
            InheritRequest {
                task,
                authorized,
                at,
                head,
                role: self.actor.role(),
                reason: reason.trim().to_owned(),
            },
        )
    }
}

/// [`Planning::ready_inherit_read`]'s answer: what the head is chosen from,
/// and the latest run to choose it of.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InheritToRead {
    pub at: InheritReadAt,
    pub run: Option<crate::domain::TaskRun>,
}

/// What a `ready --inherit`'s caller chooses and keeps the head with,
/// outside the queue service (ADR-t3102-3 decision 5): the ports
/// [`ready_inherit`] runs through.
pub struct InheritClient<'a> {
    /// The main checkout of the repository whose Git common directory it
    /// is given.
    pub main_checkout: &'a dyn Fn(&Path) -> Result<PathBuf>,
    /// Runs its callback with the branches of the runs' repositories,
    /// their containers bound to the main checkout given (none when none
    /// was found) and of the queue at the DB path given (the queue the read
    /// answered for), and what takes the records of the images a facts-only
    /// turn of a run in a container made ready since it was last asked, in
    /// the order made (none for a caller that runs no such turn).
    pub git: &'a WithInheritGit<'a>,
}

/// [`InheritClient::git`]: given the main checkout and the queue's DB path,
/// it runs the callback with the runs' branches and what takes the image's
/// records.
pub type WithInheritGit<'a> = dyn Fn(
        Option<PathBuf>,
        Option<PathBuf>,
        &mut dyn FnMut(&dyn CarriedBranches, &dyn Fn() -> Vec<KeptImageRecord>),
    ) + 'a;

/// `params` as the queue service's client runs `ready --inherit`
/// (ADR-t3102-3 decision 5, ADR-t3294-1), asking `call` (the service, or
/// the command line on the DB) and answering what the record answered:
/// what the head is chosen from ([`QueueCommand::ReadyInheritRead`]); the
/// head chosen and written under `refs/dagq/runs/<run-id>` with the
/// client's Git ([`keep_head`]), in the main checkout of the repository the
/// queue is bound to and with the containers of the queue the read
/// answered for; the records of the images a facts-only turn made ready,
/// sent whether the head was chosen or not
/// ([`QueueCommand::ReadyInheritImages`]: when both fail, the head's error
/// is the command's and the records' is logged; when only the records fail,
/// nothing is recorded and the ref is put back); and the record
/// ([`QueueCommand::ReadyInherit`]), whose answer settles the ref
/// ([`crate::application::inherit::KeptHead::settle`]: `refused` tells a
/// refusal, which puts it back, from a failure whose outcome is not
/// known).
pub fn ready_inherit(
    params: &ReadyInheritParams,
    client: &InheritClient<'_>,
    mut call: impl FnMut(&QueueCommand) -> Result<Value>,
    refused: impl Fn(&anyhow::Error) -> bool,
) -> Result<Value> {
    use anyhow::Context;
    let to_read: ReadyInheritToRead =
        serde_json::from_value(call(&QueueCommand::ReadyInheritRead(ReadyInheritParams {
            work: None,
            ..params.clone()
        }))?)
        .context("the read of ready --inherit does not read")?;
    let main = to_read
        .repository
        .as_deref()
        .and_then(|common_dir| (client.main_checkout)(Path::new(common_dir)).ok());
    let queue = to_read.queue.map(PathBuf::from);
    let mut read = Some(to_read.read);
    let mut done = None;
    (client.git)(main, queue, &mut |branches, images| {
        if let Some(read) = read.take() {
            done = Some(carry_over(
                params, read, branches, images, &mut call, &refused,
            ));
        }
    });
    done.context("the head of ready --inherit was not chosen")?
}

/// [`ready_inherit`] once the head is read from `read` with `branches`,
/// `images` taking the image's records made meanwhile.
fn carry_over(
    params: &ReadyInheritParams,
    read: InheritToRead,
    branches: &dyn CarriedBranches,
    images: &dyn Fn() -> Vec<KeptImageRecord>,
    call: &mut dyn FnMut(&QueueCommand) -> Result<Value>,
    refused: &dyn Fn(&anyhow::Error) -> bool,
) -> Result<Value> {
    let kept = keep_head(read.run.as_ref(), branches);
    let records = images();
    let sent = if records.is_empty() {
        Ok(())
    } else {
        call(&QueueCommand::ReadyInheritImages(
            ReadyInheritImagesParams {
                id: params.id,
                records,
            },
        ))
        .map(drop)
    };
    let kept = match (kept, sent) {
        (kept, Ok(())) => kept?,
        // Nothing is recorded: the ref goes back as it was.
        (Ok(kept), Err(error)) => return kept.settle(branches, Err(error), |_| true),
        (Err(error), Err(images)) => {
            warn!(
                error = %format_args!("{images:#}"),
                "the image's records of a ready --inherit's facts-only turn were not recorded: {images:#}"
            );
            return Err(error);
        }
    };
    let recorded = call(&QueueCommand::ReadyInherit(ReadyInheritParams {
        work: Some(InheritWork {
            at: read.at,
            head: kept.head.clone(),
        }),
        ..params.clone()
    }));
    kept.settle(branches, recorded, refused)
}

fn check_reason(reason: &str) -> Result<()> {
    ensure!(
        !reason.trim().is_empty(),
        "--reason must say why the run is carried over"
    );
    Ok(())
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
        /// The proposal the task is in.
        proposal: Option<crate::domain::TaskProposal>,
        owner: Option<String>,
        denials: RefCell<Vec<Value>>,
        record_fails: bool,
        /// What the last change of a task was handed as authorized.
        authorized_as: Option<TaskAuthorization>,
        /// The store refuses a change of priority as a person's.
        persons_priority: bool,
    }

    /// An actor's cancel of a task whose premise is gone.
    fn stale() -> NewCancel {
        NewCancel::by_hand(Some("stale".into()), None, None)
    }

    /// An actor's cancel of a task `of` holds the content of.
    fn duplicate_of(of: i64) -> NewCancel {
        NewCancel::by_hand(None, Some(TaskId::new(of)), None)
    }

    impl Store {
        /// A request's planner giving another priority than the request's.
        fn request_refusal(&self, what: &str) -> anyhow::Error {
            if self.persons_priority {
                DomainError::RequestPriorityDiffers {
                    request_id: crate::domain::RequestId::new(44),
                    priority: Priority::Interrupt,
                    given: Priority::Low,
                }
                .into()
            } else {
                reached(what)
            }
        }

        fn refusal(
            &self,
            what: &str,
            holder: crate::domain::authorization::PriorityHolder,
        ) -> anyhow::Error {
            if self.persons_priority {
                DomainError::PersonsPriority {
                    holder,
                    priority: Priority::Interrupt,
                    role: ActorRole::Planner,
                }
                .into()
            } else {
                reached(what)
            }
        }
    }

    fn reached(what: &str) -> anyhow::Error {
        anyhow!("store: {what}")
    }

    impl PlanningStore for Store {
        fn task_authorization(&self, _: TaskId) -> Result<TaskAuthorization> {
            let status = self.status.ok_or_else(|| anyhow!("task does not exist"))?;
            Ok(TaskAuthorization {
                status,
                proposal: self.proposal.clone(),
            })
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
            Err(self.request_refusal("add"))
        }
        fn edit_task(
            &mut self,
            _: TaskId,
            _: TaskEdit,
            authorized: TaskAuthorization,
        ) -> Result<Task> {
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
            authorized: TaskAuthorization,
        ) -> Result<Task> {
            self.authorized_as = Some(authorized);
            Err(reached("set-goal"))
        }
        fn set_paths(
            &mut self,
            _: TaskId,
            _: Vec<String>,
            authorized: TaskAuthorization,
        ) -> Result<Task> {
            self.authorized_as = Some(authorized);
            Err(reached("set-paths"))
        }
        fn set_priority(
            &mut self,
            _: TaskId,
            _: Option<Priority>,
            authorized: TaskAuthorization,
        ) -> Result<Task> {
            self.authorized_as = Some(authorized);
            Err(self.refusal(
                "set-priority",
                crate::domain::authorization::PriorityHolder::Task(TASK),
            ))
        }
        fn revisit_draft(
            &mut self,
            _: TaskId,
            _: RevisitChange,
            _: &str,
            _: &str,
            authorized: TaskAuthorization,
        ) -> Result<Option<DraftRevisit>> {
            self.authorized_as = Some(authorized);
            Err(reached("revisit"))
        }
        fn transition(
            &mut self,
            _: TaskId,
            action: TaskAction,
            authorized: TaskAuthorization,
        ) -> Result<Task> {
            self.authorized_as = Some(authorized);
            Err(reached(&format!("{action:?}")))
        }
        fn cancel(
            &mut self,
            _: TaskId,
            cancel: NewCancel,
            authorized: TaskAuthorization,
        ) -> Result<Task> {
            self.authorized_as = Some(authorized);
            Err(reached(match cancel.duplicate_of {
                Some(_) => "cancel-duplicate",
                None => "Cancel",
            }))
        }
        fn add_dependency(
            &mut self,
            _: TaskId,
            _: Dependency,
            authorized: TaskAuthorization,
        ) -> Result<()> {
            self.authorized_as = Some(authorized);
            Err(reached("dependency add"))
        }
        fn remove_dependency(
            &mut self,
            _: TaskId,
            _: Dependency,
            authorized: TaskAuthorization,
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
            Err(self.request_refusal("goal add"))
        }
        fn edit_goal(&mut self, _: GoalId, _: GoalEdit) -> Result<Goal> {
            Err(self.refusal(
                "goal edit",
                crate::domain::authorization::PriorityHolder::Goal(GOAL),
            ))
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

    impl InheritStore for Store {
        fn inherit_by_hand(&mut self, _: InheritRequest) -> Result<InheritedByHand> {
            Err(reached("inherit"))
        }
        fn record_images(&mut self, _: &[(crate::domain::EventKind, Value)]) -> Result<()> {
            Err(reached("images"))
        }
    }

    type Command = fn(&mut Planning<'_, Store>) -> Result<()>;

    /// What `ready --inherit` read of an `in_progress` task with no run.
    fn read_at() -> InheritReadAt {
        InheritReadAt {
            task: TaskStatus::InProgress,
            run: None,
        }
    }

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
            execution_class: Default::default(),
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
            ("ready --inherit", |p| {
                p.ready_inherit_by_caller(TASK, "why", read_at(), None)
                    .map(drop)
            }),
            ("ready --inherit (read)", |p| {
                p.ready_inherit_read(TASK, "why").map(drop)
            }),
            ("ready --inherit (images)", |p| {
                p.ready_inherit_images(
                    TASK,
                    &[KeptImageRecord::ContainerImageFailed {
                        image: None,
                        reason: "no image".into(),
                    }],
                    1,
                )
                .map(drop)
            }),
            ("cancel", |p| p.cancel(TASK, stale()).map(drop)),
            ("cancel --duplicate-of", |p| {
                p.cancel(TASK, duplicate_of(2)).map(drop)
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

    /// ADR-t1975-1 decisions 2 and 3: the store's refusal to change a
    /// person's priority (`set-priority`, `--inherit` too, and `goal
    /// edit`) or to give another priority than the request's (`add`, `goal
    /// add`) is recorded as a refusal of the command's capability on its
    /// resource, and the error names the person's value; any other failure
    /// of the store records nothing.
    #[test]
    fn a_refusal_to_change_a_persons_priority_is_recorded() {
        let me = planner(7);
        let commands: [(Command, &str, Value); 6] = [
            (
                |p| p.set_priority(TASK, Some(Priority::Low)).map(drop),
                "task.write",
                json!({"kind": "task", "id": 1, "status": "ready"}),
            ),
            (
                |p| p.set_priority(TASK, None).map(drop),
                "task.write",
                json!({"kind": "task", "id": 1, "status": "ready"}),
            ),
            (
                |p| {
                    p.add(NewTask {
                        priority: Some(Priority::Low),
                        ..new_task()
                    })
                    .map(drop)
                },
                "task.write",
                json!({"kind": "queue"}),
            ),
            (
                |p| {
                    p.add(NewTask {
                        priority: Some(Priority::Low),
                        goal_id: Some(GOAL),
                        ..new_task()
                    })
                    .map(drop)
                },
                "task.write",
                json!({"kind": "goal", "id": 1}),
            ),
            (
                |p| {
                    p.add_goal(NewGoal {
                        priority: Some(Priority::Low),
                        ..new_goal()
                    })
                    .map(drop)
                },
                "goal.write",
                json!({"kind": "queue"}),
            ),
            (
                |p| {
                    p.edit_goal(
                        GOAL,
                        GoalEdit {
                            priority: Some(Priority::Low),
                            ..GoalEdit::default()
                        },
                    )
                    .map(drop)
                },
                "goal.write",
                json!({"kind": "goal", "id": 1}),
            ),
        ];
        for (command, capability, resource) in commands {
            let mut store = Store {
                status: Some(TaskStatus::Ready),
                persons_priority: true,
                ..Store::default()
            };
            let error = command(&mut Planning::new(&mut store, &me, &StaticPolicy)).unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("(interrupt): planner may not change it")
                    || message
                        .contains("request 44 carries the priority a person gave it, interrupt"),
                "{error:#}"
            );
            assert_eq!(
                store.denials.into_inner(),
                [json!({
                    "event": "authorization_denied",
                    "role": "planner",
                    "capability": capability,
                    "reason": "a person's priority",
                    "resource": resource,
                })]
            );
            let (outcome, denials) = run(&me, TaskStatus::Ready, None, command);
            assert!(matches!(outcome, Outcome::Allowed));
            assert!(denials.is_empty(), "{denials:?}");
        }
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
            "ready --inherit",
            "ready --inherit (read)",
            "ready --inherit (images)",
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

    /// The task's proposal is read from the store with its status: a
    /// planner's change of a task of another planner's revising proposal,
    /// or of one whose owner names no actor, is refused before the store
    /// and recorded with the proposal; its owner, the user, the inbox and
    /// the supervisor's cancel reach the store with what they were
    /// authorized with.
    #[test]
    fn a_planner_changes_no_task_of_another_planners_revising_proposal() {
        use crate::domain::{ProposalStatus, TaskProposal};
        let changes = [
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
        ];
        let command = |name: &str| commands().into_iter().find(|(n, _)| *n == name).unwrap().1;
        let in_proposal = |owner: Option<&str>| TaskProposal {
            id: ProposalId::new(3),
            status: ProposalStatus::Revising,
            owner: owner.map(str::to_owned),
        };
        let reach = |actor: &ActorContext, owner: Option<&str>, name: &str| {
            let mut store = Store {
                status: Some(TaskStatus::Draft),
                proposal: Some(in_proposal(owner)),
                ..Store::default()
            };
            let result = command(name)(&mut Planning::new(&mut store, actor, &StaticPolicy));
            (result.unwrap_err(), store)
        };
        for owner in [Some("planner:8"), Some("planner"), None] {
            for name in changes {
                let (error, store) = reach(&planner(7), owner, name);
                let denied = error.downcast::<AuthorizationError>().unwrap();
                assert_eq!(denied.reason, DenyReason::Resource, "{owner:?} {name}");
                assert_eq!(store.authorized_as, None, "{name}");
                assert_eq!(
                    store.denials.into_inner()[0]["resource"],
                    json!({"kind": "task", "id": 1, "status": "draft",
                           "proposal": {"id": 3, "status": "revising", "owner": owner}}),
                    "{name}"
                );
            }
        }
        let supervisor = ActorContext::instance(ActorRole::Supervisor, 1);
        let mut reaching: Vec<(ActorContext, &str)> = changes
            .iter()
            .flat_map(|name| {
                [
                    planner(8),
                    ActorContext::user(),
                    ActorContext::instance(ActorRole::Inbox, 1),
                ]
                .map(|actor| (actor, *name))
            })
            .collect();
        reaching.push((supervisor.clone(), "cancel"));
        for (actor, name) in reaching {
            let (error, store) = reach(&actor, Some("planner:8"), name);
            assert!(error.to_string().starts_with("store: "), "{actor:?} {name}");
            assert!(store.denials.into_inner().is_empty());
            assert_eq!(
                store.authorized_as,
                Some(TaskAuthorization {
                    status: TaskStatus::Draft,
                    proposal: Some(in_proposal(Some("planner:8"))),
                }),
                "{name}"
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
            assert_eq!(
                store.authorized_as,
                Some(TaskAuthorization::outside_proposals(status)),
                "{actor:?}"
            );
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
            ("store: Cancel", |p| p.cancel(TASK, stale()).map(drop)),
            ("store: cancel-duplicate", |p| {
                p.cancel(TASK, duplicate_of(2)).map(drop)
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
                assert_eq!(
                    store.authorized_as,
                    Some(TaskAuthorization::outside_proposals(status)),
                    "{reached}"
                );
            }
        }
    }

    #[test]
    fn a_task_the_store_cannot_find_is_its_error_not_a_refusal() {
        let mut store = Store::default();
        let me = planner(1);
        let error = Planning::new(&mut store, &me, &StaticPolicy)
            .cancel(TASK, stale())
            .unwrap_err();
        assert_eq!(error.to_string(), "task does not exist");
        assert!(store.denials.into_inner().is_empty());
    }

    #[test]
    fn a_role_without_the_capability_is_refused_before_the_store_is_read() {
        let mut store = Store::default();
        let observer = ActorContext::instance(ActorRole::Observer, 1);
        let error = Planning::new(&mut store, &observer, &StaticPolicy)
            .cancel(TASK, stale())
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

    /// The ref of the run a `ready --inherit` client keeps the head `b`
    /// under (its writes fail when `refuse_keep`), whether a write is
    /// prepared, and the image records a facts-only turn made.
    struct ClientGit {
        value: std::rc::Rc<RefCell<Option<crate::domain::CommitSha>>>,
        prepared: std::rc::Rc<std::cell::Cell<bool>>,
        refuse_keep: bool,
        /// Its writes are checked and locked, but fail as they commit.
        commit_fails: bool,
        images: RefCell<Vec<KeptImageRecord>>,
    }

    /// A write [`ClientGit`] prepared.
    struct ClientWrite {
        value: std::rc::Rc<RefCell<Option<crate::domain::CommitSha>>>,
        prepared: std::rc::Rc<std::cell::Cell<bool>>,
        to: Option<CommitSha>,
        fails: bool,
    }

    impl crate::application::PreparedRef for ClientWrite {
        fn commit(self: Box<Self>) -> Result<()> {
            anyhow::ensure!(!self.fails, "the run's ref could not be written");
            *self.value.borrow_mut() = self.to.clone();
            Ok(())
        }
        fn abort(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }

    impl Drop for ClientWrite {
        fn drop(&mut self) {
            self.prepared.set(false);
        }
    }

    impl CarriedBranches for ClientGit {
        fn carried_head(&self, _: &crate::domain::TaskRun) -> Result<Option<CommitSha>> {
            Ok(Some(head()))
        }
        fn keep(&self, _: &crate::domain::TaskRun, _: &CommitSha) -> Result<()> {
            unreachable!("ready --inherit writes the ref against what it read")
        }
        fn kept(&self, _: &crate::domain::TaskRun) -> Result<Option<CommitSha>> {
            Ok(self.value.borrow().clone())
        }
        fn prepare_write(
            &self,
            _: &crate::domain::TaskRun,
            value: Option<&CommitSha>,
            old: Option<&CommitSha>,
        ) -> Result<Box<dyn crate::application::PreparedRef>> {
            anyhow::ensure!(!self.refuse_keep, "a ref is under the run's");
            anyhow::ensure!(!self.prepared.get(), "the run's ref is locked");
            anyhow::ensure!(self.value.borrow().as_ref() == old, "the run's ref moved");
            self.prepared.set(true);
            Ok(Box::new(ClientWrite {
                value: self.value.clone(),
                prepared: self.prepared.clone(),
                to: value.cloned(),
                fails: self.commit_fails,
            }))
        }
    }

    fn head() -> CommitSha {
        CommitSha::try_from("b".repeat(40)).unwrap()
    }

    /// `ready --inherit` as its client runs it on `git`, the service
    /// failing the images when `images_fail` and refusing the record when
    /// `refuse`: the use cases it asked for, in order, and its answer.
    fn client_run(
        git: &ClientGit,
        images_fail: bool,
        refuse: bool,
    ) -> (Vec<String>, Result<Value>) {
        use crate::domain::{Provider, RunRecord, RunStatus};
        let run = crate::domain::TaskRun::restore(RunRecord {
            id: RunId::new("r1").unwrap(),
            task_id: TASK,
            status: RunStatus::Failed,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::WorkerMode::Headless,
            base_commit: CommitSha::try_from("a".repeat(40)).unwrap(),
            branch: Some("dagq/r1".into()),
            worktree_path: None,
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: None,
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap();
        let read = ReadyInheritToRead {
            read: InheritToRead {
                at: InheritReadAt {
                    task: TaskStatus::InProgress,
                    run: Some(InheritRunAt::of(&run)),
                },
                run: Some(run),
            },
            repository: None,
            queue: Some("/queues/q/queue.db".into()),
        };
        let asked = RefCell::new(Vec::new());
        let answer = ready_inherit(
            &ReadyInheritParams {
                id: TASK.as_i64(),
                reason: "why".into(),
                work: None,
            },
            &InheritClient {
                main_checkout: &|_| unreachable!("no repository is bound"),
                git: &|main, queue, carry| {
                    assert_eq!(main, None);
                    assert_eq!(queue, Some(PathBuf::from("/queues/q/queue.db")));
                    carry(git, &|| git.images.take());
                },
            },
            |command| {
                asked
                    .borrow_mut()
                    .push(command.use_case().as_str().to_owned());
                match command {
                    QueueCommand::ReadyInheritRead(_) => Ok(serde_json::to_value(&read)?),
                    QueueCommand::ReadyInheritImages(_) if images_fail => {
                        Err(anyhow!("the images were refused"))
                    }
                    QueueCommand::ReadyInheritImages(_) => Ok(serde_json::json!({"recorded": 1})),
                    QueueCommand::ReadyInherit(params) => {
                        // The head is written, and no lock is held across
                        // the record.
                        assert!(!git.prepared.get());
                        assert_eq!(*git.value.borrow(), Some(head()));
                        assert_eq!(params.work.as_ref().unwrap().head, Some(head()));
                        if refuse {
                            Err(anyhow!("the run moved"))
                        } else {
                            Ok(serde_json::json!({"recorded": true}))
                        }
                    }
                    other => panic!("{other:?}"),
                }
            },
            |_| true,
        );
        (asked.into_inner(), answer)
    }

    /// The client's order (ADR-t3294-1): a head that cannot be kept, or
    /// whose write fails as it commits, asks for no record; the head is
    /// written before the image records and the record are sent; the image
    /// records' failure puts the ref back and records nothing; a refused
    /// record puts it back; a recorded one leaves the head.
    #[test]
    fn the_client_writes_the_head_before_the_record_and_puts_it_back_on_a_refusal() {
        let git = |refuse_keep: bool, commit_fails: bool, images: usize| ClientGit {
            value: Default::default(),
            prepared: Default::default(),
            refuse_keep,
            commit_fails,
            images: RefCell::new(
                (0..images)
                    .map(|_| KeptImageRecord::ContainerImageFailed {
                        image: None,
                        reason: "no image".into(),
                    })
                    .collect(),
            ),
        };
        for unkept in [git(true, false, 0), git(false, true, 0)] {
            let (asked, answer) = client_run(&unkept, false, false);
            assert_eq!(asked, ["ready_inherit_read"]);
            assert!(format!("{:#}", answer.unwrap_err()).contains("nothing is carried over"));
            assert_eq!(*unkept.value.borrow(), None);
        }

        let imaged = git(false, false, 1);
        let (asked, answer) = client_run(&imaged, true, false);
        assert_eq!(asked, ["ready_inherit_read", "ready_inherit_images"]);
        assert!(answer.is_err());
        assert_eq!(*imaged.value.borrow(), None);
        assert!(!imaged.prepared.get());

        let refused = git(false, false, 0);
        let (asked, answer) = client_run(&refused, false, true);
        assert_eq!(asked, ["ready_inherit_read", "ready_inherit"]);
        assert_eq!(answer.unwrap_err().to_string(), "the run moved");
        assert_eq!(*refused.value.borrow(), None);
        assert!(!refused.prepared.get());

        let recorded = git(false, false, 1);
        let (asked, answer) = client_run(&recorded, false, false);
        assert_eq!(
            asked,
            [
                "ready_inherit_read",
                "ready_inherit_images",
                "ready_inherit"
            ]
        );
        assert_eq!(answer.unwrap(), serde_json::json!({"recorded": true}));
        assert_eq!(*recorded.value.borrow(), Some(head()));
        assert!(!recorded.prepared.get());
    }
}
