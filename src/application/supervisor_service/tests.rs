use super::*;
use crate::domain::queue_service::ServiceErrorCode;
use crate::domain::{NewTask, Priority, RunId, Task};

fn failed(kind: Option<FailureKind>) -> ServiceError {
    ServiceError {
        code: ServiceErrorCode::Failed,
        message: "it failed".into(),
        denied: None,
        kind,
    }
}

/// The failures the supervisor decides on come back as the queue's own
/// errors, so the loop reads a run that is not there and a busy queue as
/// it read them on the DB; the service's other failures keep its code.
#[test]
fn a_services_failure_reads_as_the_queues_own_error() {
    let run = RunId::new("r1").unwrap();
    let missing = service_error(&failed(Some(FailureKind::RunNotFound(run.clone()))));
    assert!(missing.downcast_ref::<RunNotFound>().is_some());
    assert_eq!(failure_kind(&missing), Some(FailureKind::RunNotFound(run)));
    let busy = service_error(&failed(Some(FailureKind::QueueBusy)));
    assert!(QueueBusy::is(&busy));
    assert_eq!(failure_kind(&busy), Some(FailureKind::QueueBusy));
    let other = service_error(&failed(None));
    assert!(
        format!("{other:#}").contains("(failed): it failed"),
        "{other:#}"
    );
    assert_eq!(failure_kind(&other), None);
}

/// Only a service out of reach is waited for: a refusal is not.
#[test]
fn only_a_service_out_of_reach_is_unreachable() {
    let gone = anyhow::Error::new(ServiceUnreachable("no socket".into())).context("claim");
    assert!(unreachable(&gone));
    assert!(!unreachable(&service_error(&failed(None))));
}

/// A task crosses whole, the fields its CLI JSON leaves out included: the
/// task made back from its form is the same task.
#[test]
fn a_task_crosses_whole() {
    let task = Task::new(
        crate::domain::TaskId::new(3),
        NewTask {
            title: "t".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: vec!["src/**".into()],
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            priority: Some(Priority::High),
            goal_id: None,
            context: "c".into(),
            change: None,
            provider: None,
            worker_mode: None,
            wait_for_build: true,
            execution_class: Default::default(),
        },
        "2026-10-22T00:00:00.000Z".into(),
    )
    .unwrap();
    let form = serde_json::to_value(task.clone().into_form()).unwrap();
    let back = Task::from_form(serde_json::from_value(form).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(crate::domain::task::TaskWire::from(back)).unwrap(),
        serde_json::to_value(crate::domain::task::TaskWire::from(task)).unwrap()
    );
}
