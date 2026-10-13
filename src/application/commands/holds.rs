//! The hold of a task and its release (ADR-t1879-1): `hold` and `release`,
//! which only a person at a plain terminal and the inbox run. Each is
//! authorized as the caller on the task (a run named is resolved to its
//! task first) before it reaches the store, and a refusal is recorded as
//! `authorization_denied`. The policy is the [`crate::domain::StaticPolicy`]'s
//! (`docs/design/authorization.md`).

use anyhow::Result;

use super::{DenialLog, Gate};
use crate::domain::task_hold::{HoldRecorded, HoldTarget, ReleaseRecorded};
use crate::domain::{ActorContext, Authorizer, Capability, Resource, RunId, TaskId};

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
    /// Release the open hold of the task `target` names.
    fn release_task(
        &mut self,
        target: &HoldTarget,
        by_role: &str,
        by_id: &str,
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

    /// `release`: release the open hold of the task `target` names.
    pub fn release(&mut self, target: &HoldTarget) -> Result<ReleaseRecorded> {
        self.authorize(target)?;
        let actor = self.gate.actor;
        self.store
            .release_task(target, actor.role().as_str(), actor.actor_id())
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
    use crate::domain::task_hold::{HoldState, TaskHold};
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

        fn release_task(
            &mut self,
            target: &HoldTarget,
            _: &str,
            _: &str,
        ) -> Result<ReleaseRecorded> {
            self.released.push(target.clone());
            Ok(ReleaseRecorded {
                hold: hold(TaskId::new(7)),
                continuation: "claim",
            })
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
                let released = holds.release(target);
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
