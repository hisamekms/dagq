//! The replanning commands (ADR-t1521-1, ADR-t1521-2): `replan request`,
//! which only the user and the inbox run (at a person's word), `replan
//! propose`, which a worker runs on its own run only, and the trusted
//! runtime's detection and moves of an operation (the park, the apply),
//! which no command line reaches. Each is authorized as the caller on the
//! run before it reaches the store, and a refusal is recorded as
//! `authorization_denied`. The policy is the
//! [`crate::domain::StaticPolicy`]'s (`docs/design/authorization.md`).

use anyhow::Result;

use super::{DenialLog, Gate};
use crate::domain::task_replan::{
    NewReplan, ReplanMove, ReplanOperation, ReplanOrigin, ReplanRecorded, ReplanStatus,
};
use crate::domain::{ActorContext, Authorizer, Capability, ReplanId, Resource, RunId, TaskId};

/// What the replanning commands read and change of the queue.
pub trait ReplanStore: DenialLog {
    /// The task of run `run`; `None` for a run the queue does not know.
    fn run_task(&self, run: &RunId) -> Result<Option<TaskId>>;
    /// Record `new` from `origin` as `by_role` with actor id `by_id`: a new
    /// operation on its run, or the task's open one, so that a task keeps
    /// at most one open operation. One that guards the task stays on the
    /// run it fenced, and a request on any run of the task joins it; one
    /// that does not guard is carried to the task's latest run (by a
    /// request on that run too), and a request on another run is refused
    /// while it is open there.
    fn record_replan(
        &mut self,
        new: &NewReplan,
        origin: ReplanOrigin,
        by_role: &str,
        by_id: &str,
    ) -> Result<ReplanRecorded>;
    /// Operation `id`; a missing one is an error.
    fn replan_operation(&self, id: ReplanId) -> Result<ReplanOperation>;
    /// Move operation `id`, read under `generation`, as `step` says.
    fn move_replan(
        &mut self,
        id: ReplanId,
        generation: i64,
        step: &ReplanMove,
    ) -> Result<ReplanOperation>;
}

/// The replanning commands of one actor on one store.
pub struct Replans<'a, S> {
    store: &'a mut S,
    gate: Gate<'a>,
}

impl<'a, S: ReplanStore> Replans<'a, S> {
    pub fn new(store: &'a mut S, actor: &'a ActorContext, authorizer: &'a dyn Authorizer) -> Self {
        Self {
            store,
            gate: Gate { actor, authorizer },
        }
    }

    /// `replan request`: a person's (or the inbox's) request on a run,
    /// which also records the operation's planning request.
    pub fn request(&mut self, new: &NewReplan) -> Result<ReplanRecorded> {
        self.record(new, Capability::ReplanRequest, ReplanOrigin::Manual)
    }

    /// `replan propose`: a worker's proposal on its own run, refused on
    /// any other run.
    pub fn propose(&mut self, new: &NewReplan) -> Result<ReplanRecorded> {
        self.record(new, Capability::ReplanPropose, ReplanOrigin::Worker)
    }

    /// The runtime's detection of a run to diagnose: the trusted runtime's
    /// only.
    pub fn detect(&mut self, new: &NewReplan) -> Result<ReplanRecorded> {
        self.record(new, Capability::ReplanPark, ReplanOrigin::Runtime)
    }

    /// Move operation `id` (read under `generation`) as `step` says: into
    /// the replacement ([`Capability::ReplanApply`]) or any other step
    /// ([`Capability::ReplanPark`]), the trusted runtime's only.
    pub fn advance(
        &mut self,
        id: ReplanId,
        generation: i64,
        step: &ReplanMove,
    ) -> Result<ReplanOperation> {
        let capability = if matches!(step.to, ReplanStatus::Applying | ReplanStatus::Applied) {
            Capability::ReplanApply
        } else {
            Capability::ReplanPark
        };
        self.gate
            .refuse_ungranted(&*self.store, capability, &Resource::Queue)?;
        let op = self.store.replan_operation(id)?;
        self.gate.authorize(
            &*self.store,
            capability,
            &Resource::Run {
                id: op.run_id,
                task: Some(op.task_id),
            },
        )?;
        self.store.move_replan(id, generation, step)
    }

    fn record(
        &mut self,
        new: &NewReplan,
        capability: Capability,
        origin: ReplanOrigin,
    ) -> Result<ReplanRecorded> {
        new.validate()?;
        let task = self.store.run_task(&new.run)?;
        self.gate.authorize(
            &*self.store,
            capability,
            &Resource::Run {
                id: new.run.clone(),
                task,
            },
        )?;
        let actor = self.gate.actor;
        self.store
            .record_replan(new, origin, actor.role().as_str(), actor.actor_id())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::Value;

    use super::*;
    use crate::domain::{ActorRole, StaticPolicy};

    #[derive(Default)]
    struct Store {
        denials: RefCell<Vec<Value>>,
        recorded: Vec<(ReplanOrigin, String)>,
        moved: Vec<ReplanStatus>,
    }

    impl DenialLog for Store {
        fn record_denial(&self, payload: Value) -> Result<()> {
            self.denials.borrow_mut().push(payload);
            Ok(())
        }
    }

    impl ReplanStore for Store {
        fn run_task(&self, _: &RunId) -> Result<Option<TaskId>> {
            Ok(Some(TaskId::new(7)))
        }

        fn record_replan(
            &mut self,
            _: &NewReplan,
            origin: ReplanOrigin,
            by_role: &str,
            _: &str,
        ) -> Result<ReplanRecorded> {
            self.recorded.push((origin, by_role.to_owned()));
            anyhow::bail!("recorded")
        }

        fn replan_operation(&self, _: ReplanId) -> Result<ReplanOperation> {
            anyhow::bail!("no operation in this store")
        }

        fn move_replan(
            &mut self,
            _: ReplanId,
            _: i64,
            step: &ReplanMove,
        ) -> Result<ReplanOperation> {
            self.moved.push(step.to);
            anyhow::bail!("moved")
        }
    }

    fn new(run: &str) -> NewReplan {
        NewReplan {
            run: RunId::new(run).unwrap(),
            reason: "the review keeps sending it back".into(),
            evidence: Vec::new(),
        }
    }

    #[test]
    fn each_role_records_only_the_origin_it_may_and_a_worker_only_on_its_run() {
        let worker = ActorContext::worker(&RunId::new("own").unwrap(), TaskId::new(7));
        let cases: [(ActorContext, bool, bool); 6] = [
            (ActorContext::user(), true, false),
            (
                ActorContext::instance(ActorRole::Inbox, "inbox"),
                true,
                false,
            ),
            (worker, false, true),
            (ActorContext::instance(ActorRole::Planner, 3), false, false),
            (ActorContext::instance(ActorRole::Observer, 1), false, false),
            (
                ActorContext::instance(ActorRole::ReviewJob, 1),
                false,
                false,
            ),
        ];
        for (actor, requests, proposes) in cases {
            let mut store = Store::default();
            let mut replans = Replans::new(&mut store, &actor, &StaticPolicy);
            let requested = replans.request(&new("own"));
            let proposed = replans.propose(&new("own"));
            let elsewhere = replans.propose(&new("other"));
            assert!(elsewhere.is_err());
            let error = |result: Result<ReplanRecorded>| result.unwrap_err().to_string();
            assert_eq!(error(requested) == "recorded", requests, "{actor:?}");
            assert_eq!(error(proposed) == "recorded", proposes, "{actor:?}");
            let expected: Vec<ReplanOrigin> = [
                requests.then_some(ReplanOrigin::Manual),
                proposes.then_some(ReplanOrigin::Worker),
            ]
            .into_iter()
            .flatten()
            .collect();
            assert_eq!(
                store
                    .recorded
                    .iter()
                    .map(|(origin, _)| *origin)
                    .collect::<Vec<_>>(),
                expected
            );
            // Every refusal is recorded, the worker's on another run too.
            let refused = usize::from(!requests) + usize::from(!proposes) + 1;
            assert_eq!(store.denials.borrow().len(), refused, "{actor:?}");
        }
    }

    #[test]
    fn only_the_runtime_detects_or_moves_an_operation() {
        let step = ReplanMove {
            to: ReplanStatus::PauseRequested,
            reason: "diagnosed".into(),
            snapshot_id: None,
            diagnosis: None,
        };
        for actor in [
            ActorContext::user(),
            ActorContext::instance(ActorRole::Inbox, "inbox"),
            ActorContext::instance(ActorRole::Planner, 3),
            ActorContext::worker(&RunId::new("own").unwrap(), TaskId::new(7)),
        ] {
            let mut store = Store::default();
            let mut replans = Replans::new(&mut store, &actor, &StaticPolicy);
            let detected = replans.detect(&new("own")).unwrap_err();
            assert!(detected.to_string().contains("replan.park"), "{detected}");
            let moved = replans.advance(ReplanId::new(1), 1, &step).unwrap_err();
            assert!(moved.to_string().contains("replan.park"), "{moved}");
            assert!(store.recorded.is_empty());
            assert!(store.moved.is_empty());
        }
        let supervisor = ActorContext::instance(ActorRole::Supervisor, 1);
        let mut store = Store::default();
        let mut replans = Replans::new(&mut store, &supervisor, &StaticPolicy);
        assert_eq!(
            replans.detect(&new("own")).unwrap_err().to_string(),
            "recorded"
        );
        // Read before it moves: this store has none.
        assert!(replans.advance(ReplanId::new(1), 1, &step).is_err());
        assert_eq!(
            store.recorded,
            vec![(ReplanOrigin::Runtime, "supervisor".into())]
        );
    }
}
