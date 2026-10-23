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
    let gone = anyhow::Error::new(ServiceUnreachable::maybe_done("no answer")).context("claim");
    assert!(unreachable(&gone));
    assert!(!undone(&gone));
    let refused = anyhow::Error::new(ServiceUnreachable::not_done("no socket")).context("claim");
    assert!(unreachable(&refused));
    assert!(undone(&refused));
    assert!(!unreachable(&service_error(&failed(None))));
    assert!(!undone(&service_error(&failed(None))));
}

/// A service whose answers are lost after it read the request.
struct AnswersLost;

impl ServiceCaller for AnswersLost {
    fn call(&self, _: UseCase, _: Value) -> Result<Value> {
        Err(ServiceUnreachable::maybe_done("the answer was lost").into())
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn sign_out(&self) {}
}

/// No connection of the queue's own: the test uses none.
struct NoQueue;

impl super::super::QueueOpener for NoQueue {
    fn open(&self) -> Result<Box<dyn Queue + Send>> {
        anyhow::bail!("not opened")
    }
}

/// A read whose answer was lost did nothing; a write whose answer was lost
/// may have been done. Each write is counted as tried on its thread, a
/// read is not, and either one unanswered is counted on its thread and
/// seen by the next look.
#[test]
fn a_lost_answer_tells_a_read_from_a_write_tried() {
    let queue = SupervisorQueues {
        service: Arc::new(AnswersLost),
        local: Arc::new(NoQueue),
        actor: ActorContext::instance(ActorRole::Supervisor, 1),
    }
    .open()
    .unwrap();
    let (run, token) = (RunId::new("r1").unwrap(), LeaseToken::new("t"));
    let (tried, unanswered) = (writes_tried(), unanswered_here());
    let read = queue.holds_lease(&run, &token).unwrap_err();
    assert!(unreachable(&read) && undone(&read), "{read:#}");
    assert_eq!(writes_tried(), tried);
    assert_eq!(unanswered_here(), unanswered + 1);
    assert!(take_unanswered());
    let mut queue = queue;
    let write = queue.release_lease(&run, &token).unwrap_err();
    assert!(unreachable(&write) && !undone(&write), "{write:#}");
    assert_eq!(writes_tried(), tried + 1);
    assert_eq!(unanswered_here(), unanswered + 2);
    assert!(take_unanswered());
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

/// `set_max_load` reads the source when the supervisor sends one, and the
/// two arguments of a binary that sends none.
#[test]
fn set_max_load_reads_its_source_and_the_older_form_without_it() {
    use crate::domain::slot_limits::SettingSource;
    let (token, max_load, source) =
        dispatch::max_load_args("set_max_load", serde_json::json!(["t", 6.0, "flag"])).unwrap();
    assert_eq!(token, LeaseToken::new("t"));
    assert_eq!(max_load, Some(6.0));
    assert_eq!(source, Some(SettingSource::Flag));
    let (_, max_load, source) =
        dispatch::max_load_args("set_max_load", serde_json::json!(["t", null])).unwrap();
    assert_eq!((max_load, source), (None, None));
    assert!(dispatch::max_load_args("set_max_load", serde_json::json!(["t"])).is_err());
}

/// A service that answers `set_max_load` with `with_source` when the
/// supervisor sends a source and with nothing when it does not, and keeps
/// the number of arguments of each call.
struct MaxLoadService {
    with_source: fn() -> anyhow::Error,
    calls: std::sync::Mutex<Vec<usize>>,
}

impl ServiceCaller for MaxLoadService {
    fn call(&self, _: UseCase, params: Value) -> Result<Value> {
        let args = params["args"].as_array().map_or(0, Vec::len);
        self.calls.lock().unwrap().push(args);
        if args == 3 {
            Err((self.with_source)())
        } else {
            Ok(Value::Null)
        }
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn sign_out(&self) {}
}

/// A source the service refuses as arguments it does not read (the
/// service of the binary before a handoff) is dropped and the value sent
/// alone; any other failure is the call's, not sent again without the
/// source.
#[test]
fn set_max_load_sends_the_value_alone_only_to_a_service_that_reads_no_source() {
    use crate::domain::slot_limits::SettingSource;
    let calls = |with_source: fn() -> anyhow::Error| {
        let service = Arc::new(MaxLoadService {
            with_source,
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let queue = SupervisorQueues {
            service: service.clone(),
            local: Arc::new(NoQueue),
            actor: ActorContext::instance(ActorRole::Supervisor, 1),
        }
        .open()
        .unwrap();
        let result =
            queue.set_max_load(&LeaseToken::new("t"), Some(6.0), Some(SettingSource::Flag));
        (result.is_ok(), service.calls.lock().unwrap().clone())
    };
    fn refusal(code: ServiceErrorCode) -> anyhow::Error {
        service_error(&ServiceError {
            code,
            message: "the arguments of set_max_load do not read".into(),
            denied: None,
            kind: None,
        })
    }
    assert_eq!(
        calls(|| refusal(ServiceErrorCode::BadRequest)),
        (true, vec![3, 2])
    );
    assert_eq!(
        calls(|| refusal(ServiceErrorCode::Failed)),
        (false, vec![3])
    );
    assert_eq!(
        calls(|| service_error(&failed(Some(FailureKind::QueueBusy)))),
        (false, vec![3])
    );
    assert_eq!(
        calls(|| ServiceUnreachable::maybe_done("the answer was lost").into()),
        (false, vec![3])
    );
}
