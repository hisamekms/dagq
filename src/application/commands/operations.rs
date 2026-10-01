//! The runtime operations (task 734): `integrate`, `recover`, `review`,
//! the service's `up` / `down` / `install` / `auto-update` / `supervise`,
//! the queue's `init` / `migrate` / `rebind`, `plan`, `observe`, and the
//! internal `session`, `session-event` and `planner-session` the runtime's
//! wrappers and hooks run. Each is authorized as the caller before the
//! command does anything: the user and the inbox (a person's word, told
//! apart by the record) run them all, the planner the service and the
//! queue at a person's word, the supervisor what it starts for itself; the
//! workers, the four jobs and the observer none of them but a worker's
//! own session. The operations themselves stay where they are; this is
//! their entry. The policy is the [`crate::domain::StaticPolicy`]'s
//! (`docs/design/authorization.md`).

use anyhow::Result;

use super::{DenialLog, Gate};
use crate::domain::{ActorContext, Authorizer, Capability, PlannerId, Resource, RunId, TaskId};

/// The session a `session-event` hook reports on, as its environment and
/// arguments name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookSession {
    /// The inbox's, or a planner's of a workspace that names no planner.
    Queue,
    /// The planner's of `DAGQ_PLANNER_ID`.
    Planner(PlannerId),
    /// A run's session (`--run`, else the caller's `DAGQ_RUN_ID`); `None`
    /// when it names none or one that cannot be read.
    Run(Option<RunId>),
}

/// A runtime operation and what it acts on, as far as the command names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Init,
    Migrate,
    Rebind,
    Install,
    /// The supervisor's replacement of the fixed binary: `install`'s
    /// capability, which the supervisor that starts it holds.
    AutoUpdate,
    Up,
    Down,
    /// `broker start` and `broker stop`: the resource broker's container
    /// and dagq's Podman machine, with the service's capability.
    Broker,
    /// `service start`, `service stop` and `service serve`: the queue
    /// service, with the service's capability (ADR-t1233-4 decision 1).
    QueueService,
    Plan,
    /// The supervisor's loop, started by `up` (in its workspace or by
    /// launchd), by a person, or by a supervisor handing off to itself.
    Supervise,
    Observe,
    /// `integrate ID`, or `integrate --next` (`None`).
    Integrate(Option<TaskId>),
    /// `recover RUN`; `None` for a run id that cannot be read.
    Recover(Option<RunId>),
    Review(TaskId),
    /// The session wrapper of a run; `None` for a run id that cannot be read.
    Session(Option<RunId>),
    PlannerSession(PlannerId),
    SessionEvent(HookSession),
}

fn run_or_unresolved(run: Option<&RunId>) -> Resource {
    run.cloned().map_or(Resource::Unresolved, Resource::run)
}

impl Operation {
    /// The capability the operation needs and the resource it acts on.
    pub fn request(&self) -> (Capability, Resource) {
        use Capability as C;
        match self {
            Self::Init | Self::Migrate | Self::Rebind => (C::QueueAdmin, Resource::Queue),
            Self::Install | Self::AutoUpdate => (C::BinaryInstall, Resource::Queue),
            Self::Up | Self::Down | Self::Broker | Self::QueueService => {
                (C::ServiceLifecycle, Resource::Queue)
            }
            Self::Plan => (C::PlannerOpen, Resource::Queue),
            Self::Supervise => (C::Supervise, Resource::Queue),
            Self::Observe => (C::ObserveRun, Resource::Queue),
            Self::Integrate(task) => (
                C::IntegrationRequest,
                task.map_or(Resource::Queue, Resource::task),
            ),
            Self::Recover(run) => (C::RunRecover, run_or_unresolved(run.as_ref())),
            Self::Review(task) => (C::PrepareReview, Resource::task(*task)),
            Self::Session(run) => (C::SessionRun, run_or_unresolved(run.as_ref())),
            Self::PlannerSession(planner) => (C::SessionRun, Resource::Planner(*planner)),
            Self::SessionEvent(session) => (
                C::SessionRecord,
                match session {
                    HookSession::Queue => Resource::Queue,
                    HookSession::Planner(planner) => Resource::Planner(*planner),
                    HookSession::Run(run) => run_or_unresolved(run.as_ref()),
                },
            ),
        }
    }
}

/// Authorize `operation` as `actor` before it runs; a refusal is recorded
/// on `log` (as the refused actor) and returned as the
/// [`crate::domain::AuthorizationError`].
pub fn authorize(
    actor: &ActorContext,
    authorizer: &dyn Authorizer,
    log: &dyn DenialLog,
    operation: &Operation,
) -> Result<()> {
    let (capability, resource) = operation.request();
    Gate { actor, authorizer }.authorize(log, capability, &resource)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::Value;

    use super::*;
    use crate::domain::{ActorRole, AuthorizationError, StaticPolicy};

    #[derive(Default)]
    struct Log(RefCell<Vec<Value>>);

    impl DenialLog for Log {
        fn record_denial(&self, payload: Value) -> Result<()> {
            self.0.borrow_mut().push(payload);
            Ok(())
        }
    }

    fn run(id: &str) -> RunId {
        RunId::new(id).unwrap()
    }

    fn allowed(actor: &ActorContext, operation: &Operation) -> bool {
        authorize(actor, &StaticPolicy, &Log::default(), operation).is_ok()
    }

    /// Every operation but the internal ones a session runs for itself.
    fn operations() -> Vec<Operation> {
        vec![
            Operation::Init,
            Operation::Migrate,
            Operation::Rebind,
            Operation::Install,
            Operation::AutoUpdate,
            Operation::Up,
            Operation::Down,
            Operation::Broker,
            Operation::Plan,
            Operation::Supervise,
            Operation::Observe,
            Operation::Integrate(Some(TaskId::new(1))),
            Operation::Integrate(None),
            Operation::Recover(Some(run("r1"))),
            Operation::Review(TaskId::new(1)),
        ]
    }

    #[test]
    fn the_user_and_the_inbox_run_every_operation() {
        for actor in [
            ActorContext::user(),
            ActorContext::instance(ActorRole::Inbox, "inbox"),
        ] {
            for operation in operations() {
                assert!(allowed(&actor, &operation), "{actor:?} {operation:?}");
            }
        }
    }

    #[test]
    fn workers_jobs_and_the_observer_run_none_and_each_refusal_is_recorded() {
        let mut actors = vec![
            ActorContext::worker(&run("r1"), TaskId::new(1)),
            ActorContext::instance(ActorRole::Observer, 1),
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
            for operation in operations() {
                let log = Log::default();
                let error = authorize(&actor, &StaticPolicy, &log, &operation).unwrap_err();
                let error = error.downcast_ref::<AuthorizationError>().unwrap();
                assert_eq!(error.role, actor.role(), "{operation:?}");
                assert_eq!(log.0.borrow().len(), 1, "{operation:?}");
                assert_eq!(log.0.borrow()[0]["event"], "authorization_denied");
            }
        }
    }

    #[test]
    fn the_planner_runs_the_service_and_the_queue_but_no_run_or_landing() {
        let planner = ActorContext::instance(ActorRole::Planner, 7);
        for operation in [
            Operation::Up,
            Operation::Down,
            Operation::Broker,
            Operation::Install,
            Operation::AutoUpdate,
            Operation::Init,
            Operation::Migrate,
            Operation::Rebind,
            Operation::Plan,
            Operation::PlannerSession(PlannerId::new(7)),
            Operation::SessionEvent(HookSession::Queue),
            Operation::SessionEvent(HookSession::Planner(PlannerId::new(7))),
        ] {
            assert!(allowed(&planner, &operation), "{operation:?}");
        }
        for operation in [
            Operation::Integrate(Some(TaskId::new(1))),
            Operation::Integrate(None),
            Operation::Recover(Some(run("r1"))),
            Operation::Review(TaskId::new(1)),
            Operation::Supervise,
            Operation::Observe,
            Operation::Session(Some(run("r1"))),
            Operation::PlannerSession(PlannerId::new(8)),
            Operation::SessionEvent(HookSession::Planner(PlannerId::new(8))),
            Operation::SessionEvent(HookSession::Run(Some(run("r1")))),
        ] {
            assert!(!allowed(&planner, &operation), "{operation:?}");
        }
    }

    #[test]
    fn the_supervisor_starts_its_own_processes_and_updates_the_binary() {
        let supervisor = ActorContext::instance(ActorRole::Supervisor, 42);
        // Its automatic update runs `migrate --check`, `migrate`, `init` on
        // a probe queue and `up` with the new binary, as `install` does.
        for operation in operations() {
            assert!(allowed(&supervisor, &operation), "{operation:?}");
        }
        // A session's span is its session's to record.
        assert!(!allowed(
            &supervisor,
            &Operation::SessionEvent(HookSession::Queue)
        ));
    }

    #[test]
    fn a_worker_runs_and_records_its_own_session_only() {
        let worker = ActorContext::worker(&run("r1"), TaskId::new(1));
        assert!(allowed(&worker, &Operation::Session(Some(run("r1")))));
        assert!(allowed(
            &worker,
            &Operation::SessionEvent(HookSession::Run(Some(run("r1"))))
        ));
        for operation in [
            Operation::Session(Some(run("r2"))),
            Operation::Session(None),
            Operation::SessionEvent(HookSession::Run(Some(run("r2")))),
            Operation::SessionEvent(HookSession::Run(None)),
            // Not the inbox's or a planner's span either.
            Operation::SessionEvent(HookSession::Queue),
            Operation::SessionEvent(HookSession::Planner(PlannerId::new(1))),
            Operation::PlannerSession(PlannerId::new(1)),
        ] {
            assert!(!allowed(&worker, &operation), "{operation:?}");
        }
        // A worker without its run in its environment owns no session.
        let anonymous = ActorContext::instance(ActorRole::Worker, 1);
        assert!(!allowed(&anonymous, &Operation::Session(Some(run("r1")))));
    }

    #[test]
    fn the_resource_of_each_operation() {
        assert_eq!(
            Operation::Integrate(Some(TaskId::new(3))).request(),
            (
                Capability::IntegrationRequest,
                Resource::task(TaskId::new(3))
            )
        );
        assert_eq!(
            Operation::Integrate(None).request(),
            (Capability::IntegrationRequest, Resource::Queue)
        );
        assert_eq!(
            Operation::Recover(None).request(),
            (Capability::RunRecover, Resource::Unresolved)
        );
        assert_eq!(
            Operation::SessionEvent(HookSession::Run(Some(run("r1")))).request(),
            (Capability::SessionRecord, Resource::run(run("r1")))
        );
    }
}
