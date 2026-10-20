//! The hold of a task and its release (ADR-t1879-1): `hold` and `release`,
//! which only a person at a plain terminal and the inbox run. Each is
//! authorized as the caller on the task (a run named is resolved to its
//! task first) before it reaches the store, and a refusal is recorded as
//! `authorization_denied`. The policy is the [`crate::domain::StaticPolicy`]'s
//! (`docs/design/authorization.md`).

use anyhow::Result;

use super::{DenialLog, Gate};
use crate::application::inherit::{ReleasedBranches, ReleasedRead};
use crate::domain::task_hold::{
    self, HoldRecorded, HoldState, HoldTarget, ProgressVersion, ReleaseChoice, ReleaseFacts,
    ReleaseReadAt, ReleaseRecorded, TaskHold,
};
use crate::domain::{
    ActorContext, Authorizer, Capability, CommitSha, Resource, RunId, Task, TaskId, TaskRun,
};

/// What a release reads before its store's transaction: the task, its
/// open hold, its latest run with its [`ProgressVersion`], whether it has
/// a session a resume goes on in and whether a result was kept for it
/// ([`ReleaseFacts`]).
#[derive(Debug, Clone)]
pub struct ReleaseView {
    pub task: Task,
    pub hold: Option<TaskHold>,
    pub run: Option<TaskRun>,
    pub version: Option<ProgressVersion>,
    pub session: bool,
    pub kept: bool,
}

impl ReleaseView {
    /// What the release decides on ([`task_hold::release_decision`]).
    pub fn facts(&self) -> ReleaseFacts {
        ReleaseFacts {
            task: self.task.status(),
            run: self.run.as_ref().map(TaskRun::status),
            run_provider: self.run.as_ref().map(TaskRun::actual_provider),
            session: self.session,
            kept: self.kept,
        }
    }
}

/// A release as the store applies it: what it names, the provider and
/// model it chose, and, for a release that ends the run for a new one,
/// the work read of that run ([`ReleasedBranches::released_work`]) with
/// the hold and the run's version it was read at, which the store's
/// transaction checks again ([`task_hold::check_release_read`]).
#[derive(Debug, Clone)]
pub struct ReleaseRequest {
    pub target: HoldTarget,
    pub choice: ReleaseChoice,
    pub work: Option<(ReleaseReadAt, ReleasedRead)>,
}

/// What the hold commands read and change of the queue.
pub trait HoldStore: DenialLog {
    /// The task of run `run`; `None` for a run the queue does not know.
    fn run_task(&self, run: &RunId) -> Result<Option<TaskId>>;
    /// Hold the task `target` names, as `by_role` with actor id `by_id`.
    fn hold_task(
        &mut self,
        target: &HoldTarget,
        reason: &str,
        until: Option<i64>,
        by_role: &str,
        by_id: &str,
    ) -> Result<HoldRecorded>;
    /// What a release of the task `target` names reads before its
    /// transaction.
    fn release_view(&self, target: &HoldTarget) -> Result<ReleaseView>;
    /// Release the open hold of the task `request.target` names. A release
    /// that carries the run's work over calls `keep` with the run and the
    /// head to keep inside its transaction, before it commits: an error
    /// refuses the release, which changes nothing.
    fn release_task(
        &mut self,
        request: &ReleaseRequest,
        by_role: &str,
        by_id: &str,
        keep: &dyn Fn(&TaskRun, &CommitSha) -> Result<()>,
    ) -> Result<ReleaseRecorded>;
}

/// The hold commands of one actor on one store.
pub struct Holds<'a, S> {
    store: &'a mut S,
    gate: Gate<'a>,
}

impl<'a, S: HoldStore> Holds<'a, S> {
    pub fn new(store: &'a mut S, actor: &'a ActorContext, authorizer: &'a dyn Authorizer) -> Self {
        Self {
            store,
            gate: Gate { actor, authorizer },
        }
    }

    /// `hold`: hold the task `target` names for `reason`, with `until` the
    /// time to remind a person (unix seconds).
    pub fn hold(
        &mut self,
        target: &HoldTarget,
        reason: &str,
        until: Option<i64>,
    ) -> Result<HoldRecorded> {
        self.authorize(target)?;
        let actor = self.gate.actor;
        self.store.hold_task(
            target,
            reason,
            until,
            actor.role().as_str(),
            actor.actor_id(),
        )
    }

    /// `release`: release the open hold of the task `target` names, with
    /// the provider and model of `choice` for the task's later turns and
    /// runs (ADR-t1879-1). Once the run waits, a release that ends it for
    /// a new one ([`task_hold::ends_run`]) reads its work first
    /// (`branches`: its own commits, and its uncommitted work saved as a
    /// commit; a release whose work cannot be saved is refused) and keeps
    /// the head the next run carries over under `refs/dagq/runs/<run-id>`
    /// in the store's transaction, before it commits: a head that cannot be
    /// kept refuses the release, which then changes nothing, as does a
    /// task whose hold or run moved since the work was read
    /// (`release_read_moved`). While the run settles nothing is read: the
    /// store refuses it with `hold_not_settled`.
    pub fn release(
        &mut self,
        target: &HoldTarget,
        choice: ReleaseChoice,
        branches: &dyn ReleasedBranches,
    ) -> Result<ReleaseRecorded> {
        self.authorize(target)?;
        let view = self.store.release_view(target)?;
        let work = match (&view.hold, &view.run, &view.version) {
            (Some(hold), Some(run), Some(version))
                if hold.state() == HoldState::Held
                    && task_hold::ends_run(view.facts(), &choice) =>
            {
                let at = ReleaseReadAt {
                    hold: hold.id,
                    run: run.id().clone(),
                    version: version.clone(),
                };
                Some((at, branches.released_work(run)?))
            }
            _ => None,
        };
        let actor = self.gate.actor;
        self.store.release_task(
            &ReleaseRequest {
                target: target.clone(),
                choice,
                work,
            },
            actor.role().as_str(),
            actor.actor_id(),
            &|run, head| branches.keep(run, head),
        )
    }

    fn authorize(&self, target: &HoldTarget) -> Result<()> {
        let resource = match target {
            HoldTarget::Task(task) => Resource::task(*task),
            HoldTarget::Run(run) => Resource::Run {
                id: run.clone(),
                task: self.store.run_task(run)?,
            },
        };
        self.gate
            .authorize(&*self.store, Capability::TaskHold, &resource)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::Value;

    use super::*;
    use crate::domain::{ActorRole, HoldId, StaticPolicy};

    #[derive(Default)]
    struct Store {
        denials: RefCell<Vec<Value>>,
        held: Vec<(HoldTarget, String)>,
        released: Vec<HoldTarget>,
    }

    impl DenialLog for Store {
        fn record_denial(&self, payload: Value) -> Result<()> {
            self.denials.borrow_mut().push(payload);
            Ok(())
        }
    }

    fn hold(task: TaskId) -> TaskHold {
        TaskHold {
            id: HoldId::new(1),
            task_id: task,
            run_id: None,
            reason: "why".into(),
            until: None,
            by_role: "user".into(),
            by_id: "user".into(),
            created_at: 1,
            settled_at: Some(1),
        }
    }

    impl HoldStore for Store {
        fn run_task(&self, _: &RunId) -> Result<Option<TaskId>> {
            Ok(Some(TaskId::new(7)))
        }

        fn hold_task(
            &mut self,
            target: &HoldTarget,
            _: &str,
            _: Option<i64>,
            by_role: &str,
            _: &str,
        ) -> Result<HoldRecorded> {
            self.held.push((target.clone(), by_role.to_owned()));
            Ok(HoldRecorded {
                hold: hold(TaskId::new(7)),
                state: HoldState::Held,
            })
        }

        fn release_view(&self, _: &HoldTarget) -> Result<ReleaseView> {
            let task = Task::new(
                TaskId::new(7),
                crate::domain::NewTask {
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
                    priority: Default::default(),
                    change: None,
                    provider: None,
                    worker_mode: None,
                    wait_for_build: false,
                    execution_class: Default::default(),
                },
                "now".into(),
            )?;
            Ok(ReleaseView {
                task,
                hold: Some(hold(TaskId::new(7))),
                run: None,
                version: None,
                session: false,
                kept: false,
            })
        }

        fn release_task(
            &mut self,
            request: &ReleaseRequest,
            _: &str,
            _: &str,
            _: &dyn Fn(&TaskRun, &CommitSha) -> Result<()>,
        ) -> Result<ReleaseRecorded> {
            self.released.push(request.target.clone());
            Ok(ReleaseRecorded {
                hold: hold(TaskId::new(7)),
                continuation: "claim",
                decision: task_hold::ReleaseDecision::Claim,
                provider: None,
                model: None,
                inherit: None,
            })
        }
    }

    /// Branches a release with no run never reads.
    struct NoBranches;

    impl ReleasedBranches for NoBranches {
        fn released_work(&self, _: &TaskRun) -> Result<ReleasedRead> {
            anyhow::bail!("no run's work is read")
        }
        fn keep(&self, _: &TaskRun, _: &CommitSha) -> Result<()> {
            anyhow::bail!("no head is kept")
        }
    }

    #[test]
    fn only_a_person_and_the_inbox_hold_or_release_a_task_or_a_run() {
        let run = RunId::new("r1").unwrap();
        let targets = [
            HoldTarget::Task(TaskId::new(7)),
            HoldTarget::Run(run.clone()),
        ];
        let cases: [(ActorContext, bool); 6] = [
            (ActorContext::user(), true),
            (ActorContext::instance(ActorRole::Inbox, "inbox"), true),
            (ActorContext::worker(&run, TaskId::new(7)), false),
            (ActorContext::instance(ActorRole::Planner, 3), false),
            (ActorContext::instance(ActorRole::Supervisor, 1), false),
            (ActorContext::instance(ActorRole::RecoveryJob, 1), false),
        ];
        for (actor, may) in cases {
            let mut store = Store::default();
            let mut holds = Holds::new(&mut store, &actor, &StaticPolicy);
            for target in &targets {
                let held = holds.hold(target, "the login expired", None);
                let released = holds.release(target, ReleaseChoice::default(), &NoBranches);
                assert_eq!(held.is_ok(), may, "{actor:?}");
                assert_eq!(released.is_ok(), may, "{actor:?}");
                if let Err(error) = held {
                    assert!(error.to_string().contains("task.hold"), "{error}");
                }
            }
            assert_eq!(store.held.len(), if may { 2 } else { 0 });
            assert_eq!(store.released.len(), if may { 2 } else { 0 });
            assert_eq!(store.denials.borrow().len(), if may { 0 } else { 4 });
        }
    }
}
