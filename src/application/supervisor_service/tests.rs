use super::*;
use crate::application::EventStore;
use crate::domain::queue_service::ServiceErrorCode;
use crate::domain::queue_service::{ServiceError, SupervisorParams, UseCase};
use crate::domain::{ActorContext, ActorRole, LeaseToken, NewTask, Priority, RunId, Task};
use serde_json::Value;

fn failed(kind: Option<FailureKind>) -> ServiceError {
    ServiceError {
        code: ServiceErrorCode::Failed,
        message: "it failed".into(),
        denied: None,
        kind,
    }
}

/// The failures the supervisor decides on come back as the queue's own
/// errors, so the loop reads a run that is not there, a busy queue and a
/// landing a handoff refused as it read them on the DB; the service's
/// other failures keep its code.
#[test]
fn a_services_failure_reads_as_the_queues_own_error() {
    let run = RunId::new("r1").unwrap();
    let asked = crate::application::HandoffAsked {
        supervisor: LeaseToken::new("s"),
        binary: "/next/dagq".into(),
    };
    let kind = failure_kind(&anyhow::Error::new(asked).context("begin_landing")).unwrap();
    let handing_off = service_error(&failed(Some(kind.clone())));
    let refused = handing_off
        .downcast_ref::<crate::application::HandoffAsked>()
        .unwrap();
    assert_eq!(
        (refused.supervisor.as_str(), refused.binary.as_str()),
        ("s", "/next/dagq")
    );
    assert_eq!(failure_kind(&handing_off), Some(kind));
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
    fn with_token(&self, _: &std::path::Path) -> Result<Arc<dyn ServiceCaller>> {
        anyhow::bail!("no landing here")
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn hand_over(&self) {}
    fn sign_out(&self) {}
    fn credential(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// A read whose answer was lost did nothing; a write whose answer was lost
/// may have been done. Each write is counted as tried on its thread, a
/// read is not, and either one unanswered is counted on its thread and
/// seen by the next look.
#[test]
fn a_lost_answer_tells_a_read_from_a_write_tried() {
    let queue = SupervisorQueues::new(
        Arc::new(AnswersLost),
        ActorContext::instance(ActorRole::Supervisor, 1),
    )
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
    fn with_token(&self, _: &std::path::Path) -> Result<Arc<dyn ServiceCaller>> {
        anyhow::bail!("no landing here")
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn hand_over(&self) {}
    fn sign_out(&self) {}
    fn credential(&self) -> Option<std::path::PathBuf> {
        None
    }
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
        let queue = SupervisorQueues::new(
            service.clone(),
            ActorContext::instance(ActorRole::Supervisor, 1),
        )
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

/// Every operation [`SupervisorQueue`] sends by name is one the service
/// answers (an arm of `dispatch`, a token operation or a job's) or one it
/// refuses to every supervisor (the planning of a person or a planner),
/// and every one it answers needs a capability the supervisor has
/// ([`needs::supervisor_needs`]), but the integrator's alone
/// ([`needs::INTEGRATOR_ONLY`]), which the supervisor is refused and the
/// landing's integrator sends.
#[test]
fn every_operation_the_supervisor_sends_is_one_the_service_answers() {
    use crate::domain::authorization::grants;
    let refused = |name: &str| {
        needs::supervisor_needs(name, &[])
            .is_err_and(|_| !include_str!("dispatch.rs").contains(&format!("\"{name}\" =>")))
            || needs::supervisor_needs(name, &[])
                .is_ok_and(|needs| !grants(ActorRole::Supervisor).contains(&needs))
    };
    let sent = |source: &str| -> Vec<String> {
        source
            .split("remote(")
            .skip(1)
            .filter_map(|rest| {
                let rest = rest.trim_start().strip_prefix('"')?;
                Some(rest[..rest.find('"')?].to_owned())
            })
            .collect()
    };
    let mut names = sent(include_str!("forward.rs"));
    names.extend(sent(include_str!("../supervisor_service.rs")));
    assert!(names.len() > 300, "{}", names.len());
    let dispatch = include_str!("dispatch.rs");
    for name in names {
        let answered = dispatch.contains(&format!("\"{name}\" =>"))
            || TOKEN_OPS.contains(&name.as_str())
            || JOB_OPS.contains(&name.as_str());
        assert!(answered || refused(&name), "the service answers no {name}");
        if needs::INTEGRATOR_ONLY.contains(&name.as_str()) {
            assert!(needs::supervisor_needs(&name, &[]).is_err(), "{name}");
            assert!(INTEGRATOR_RUN_OPS.contains(&name.as_str()), "{name}");
            continue;
        }
        // Read from their arguments: a submission and an ask.
        if answered && !["submit", "ask"].contains(&name.as_str()) {
            let needs = needs::supervisor_needs(&name, &[]);
            assert!(
                needs.is_ok_and(|needs| grants(ActorRole::Supervisor).contains(&needs)),
                "the supervisor is refused its {name}"
            );
        }
    }
}

/// A plan review's verdict crosses with the codes of its reasons, which
/// the job's printed form carries inside each reason.
#[test]
fn a_plan_reviews_verdict_crosses_with_its_reason_codes() {
    use crate::application::PlanReviewApply;
    use crate::domain::{PlanReviewDecision, PlanReviewVerdict};
    let verdict = PlanReviewVerdict {
        verdict: PlanReviewDecision::Concern,
        reasons: vec!["needs a person".into()],
        reason_codes: vec![vec!["scope".into()]],
        summary: "doubtful".into(),
        actions: Vec::new(),
        reopen: Vec::new(),
        precedents: Vec::new(),
        predictions: None,
        recommendation: None,
        confidence: None,
        reason_category: None,
    };
    let apply = PlanReviewApply {
        verdict: verdict.clone(),
        decision: PlanReviewDecision::Concern,
        overridden: None,
        revise_reasons: Vec::new(),
        ask: None,
        concern: None,
        duration_secs: 3,
        session: None,
        prompt_bytes: None,
        replacement_refused: Some(crate::application::RefusedPass {
            decided: crate::domain::plan_review::PlanVerdictDecision {
                decision: PlanReviewDecision::Revise,
                overridden: Some("the replacement plan does not hold".into()),
                concern: None,
            },
            ask: None,
        }),
    };
    let back: PlanReviewApply =
        serde_json::from_value(serde_json::to_value(&apply).unwrap()).unwrap();
    assert_eq!(back.verdict, verdict);
    assert_eq!(
        back.replacement_refused.unwrap().decided,
        apply.replacement_refused.unwrap().decided
    );
}

/// An answerer's name, a fixed text, reads back as the same text.
#[test]
fn an_answerer_crosses_as_its_text() {
    let answerer = crate::domain::Answerer::PERSON;
    let back: crate::domain::Answerer =
        serde_json::from_value(serde_json::to_value(answerer).unwrap()).unwrap();
    assert_eq!(back, answerer);
}

/// A service whose events are `count` events, which answers a page of
/// more than `largest` of them as past its limit.
struct PagedEvents {
    count: i64,
    largest: usize,
    asked: std::sync::Mutex<Vec<usize>>,
}

impl ServiceCaller for PagedEvents {
    fn call(&self, use_case: UseCase, params: Value) -> Result<Value> {
        assert_eq!(use_case, UseCase::Supervisor);
        let params: SupervisorParams = serde_json::from_value(params)?;
        match params.op.as_str() {
            "latest_event_id" => Ok(serde_json::json!(self.count)),
            "events_between" => {
                let after = params.args[0].as_i64().unwrap();
                let upto = params.args[1].as_i64().unwrap();
                let limit = usize::try_from(params.args[3].as_u64().unwrap()).unwrap();
                self.asked.lock().unwrap().push(limit);
                if limit > self.largest {
                    return Err(service_error(&ServiceError {
                        code: ServiceErrorCode::ResponseTooLarge,
                        message: "too large".into(),
                        denied: None,
                        kind: None,
                    }));
                }
                let page: Vec<Value> = (after + 1..=upto)
                    .take(limit)
                    .map(|id| {
                        serde_json::json!({"id": id, "task_id": null, "goal_id": null,
                            "run_id": null, "kind": "observation", "payload": {},
                            "created_at": "2026-10-23T00:00:00.000Z"})
                    })
                    .collect();
                Ok(Value::Array(page))
            }
            op => panic!("no {op} here"),
        }
    }
    fn with_token(&self, _: &std::path::Path) -> Result<Arc<dyn ServiceCaller>> {
        unreachable!("no landing here")
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn hand_over(&self) {}
    fn sign_out(&self) {}
    fn credential(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// The whole log is read a page at a time up to the newest event when the
/// read began, a page past the answer's limit asked again at half its
/// size: the events come back once each, in order.
#[test]
fn all_events_reads_the_log_in_pages_within_the_answers_limit() {
    let service = Arc::new(PagedEvents {
        count: 2500,
        largest: 600,
        asked: std::sync::Mutex::default(),
    });
    let queue = SupervisorQueues::new(
        service.clone(),
        ActorContext::instance(ActorRole::Supervisor, 7),
    )
    .queue();
    let events = EventStore::all_events(&queue).unwrap();
    let ids: Vec<i64> = events.iter().map(|event| event.id.as_i64()).collect();
    assert_eq!(ids, (1..=2500).collect::<Vec<_>>());
    let asked = service.asked.lock().unwrap().clone();
    assert_eq!(&asked[..3], [2000, 1000, 500]);
    assert!(asked[3..].iter().all(|limit| *limit == 500), "{asked:?}");
}

/// An integrator's token writes only for the landing it was issued for:
/// one whose `integration_started` its run's events hold, which they keep
/// after the landing has left `integrating`. One that names another event,
/// an event of another kind or no event writes nothing.
#[test]
fn an_integrators_token_writes_only_for_the_landing_its_run_began() {
    use crate::domain::{EventId, EventKind, RunEvent};
    let run = RunId::new("11111111-1111-4111-8111-111111111111").unwrap();
    let event = |id: i64, kind: EventKind| RunEvent {
        id: EventId::new(id),
        task_id: None,
        goal_id: None,
        run_id: Some(run.clone()),
        kind: kind.as_str().into(),
        payload: Value::Null,
        created_at: String::new(),
        actor: None,
    };
    let events = [
        event(3, EventKind::IntegrationStarted),
        event(4, EventKind::IntegrationRebased),
        event(5, EventKind::RunIntegrated),
    ];
    let integrator = Principal::integrator(&run, &LeaseToken::new("lease"), 7);
    assert_eq!(
        landing_committed(&events, &integrator.clone().of_landing(EventId::new(3))),
        Ok(())
    );
    for orphan in [
        integrator.clone().of_landing(EventId::new(6)),
        integrator.clone().of_landing(EventId::new(4)),
        integrator,
    ] {
        let refusal = landing_committed(&events, &orphan).unwrap_err();
        assert!(refusal.contains("no landing that began"), "{refusal}");
    }
}

/// The integrator of a landing sends only the landing's operations, and
/// every run or task they name is its own: one of another run or task, a
/// task's none (every task's), a second argument naming another task, or
/// an operation outside the landing's is refused.
#[test]
fn an_integrator_sends_only_the_landings_operations_on_its_own_run_and_task() {
    use serde_json::json;
    let run = RunId::new("11111111-1111-4111-8111-111111111111").unwrap();
    let other = "22222222-2222-4222-8222-222222222222";
    let task = crate::domain::TaskId::new(5);
    let scope = |op: &str, args: Value| integrator_scope(op, args.as_array().unwrap(), &run, task);
    for (op, args) in [
        (
            "record_runtime_event",
            json!([run, "integration_rebased", {}]),
        ),
        (
            "finish_integration",
            json!([run, "owner", {}, "/repo/.git"]),
        ),
        (
            "hand_over_integration",
            json!([run, "owner", "awaiting_integration", "handoff", {}]),
        ),
        ("end_stalled_detections", json!([run, "landed"])),
        ("close_run_asks", json!([run, "landed"])),
        ("close_blocked_asks", json!([run, 5, "landed"])),
        ("close_ended_landing_asks", json!([5])),
        ("show", json!([5])),
        (
            "record_queue_event",
            json!(["container_runtime_available", {}]),
        ),
    ] {
        assert_eq!(scope(op, args.clone()), Ok(()), "{op} {args}");
    }
    for (op, args) in [
        (
            "record_runtime_event",
            json!([other, "integration_rebased", {}]),
        ),
        ("close_run_asks", json!([other, "landed"])),
        (
            "hand_over_integration",
            json!([other, "owner", "awaiting_integration", "handoff", {}]),
        ),
        ("close_approve_landing_asks", json!([run, "landed"])),
        ("close_blocked_asks", json!([run, 6, "landed"])),
        ("close_blocked_asks", json!([run, null, "landed"])),
        ("close_ended_landing_asks", json!([null])),
        ("close_ended_landing_asks", json!([])),
        ("show", json!([6])),
        ("cancel", json!([5, {"reason": "no"}])),
        ("answer_as", json!([1, "withdrawn", {}])),
        ("begin_integration", json!([run, "owner", "main"])),
        ("issue_token", json!([{}])),
    ] {
        let refused = scope(op, args.clone()).unwrap_err();
        assert!(refused.contains(op), "{refused}");
    }
}

/// The known failures its verification is told are the integrator's to
/// read: the CI watch's records, whole and naming no run or task, and the
/// CI failures its own task fixes; those another task fixes are not.
#[test]
fn an_integrator_reads_the_ci_watch_and_its_own_tasks_ci_failures() {
    use serde_json::json;
    let run = RunId::new("11111111-1111-4111-8111-111111111111").unwrap();
    let task = crate::domain::TaskId::new(5);
    let scope = |op: &str, args: Value| integrator_scope(op, args.as_array().unwrap(), &run, task);
    assert_eq!(scope("ci_watch_events", json!([])), Ok(()));
    assert_eq!(scope("ci_failure_findings_of", json!([5])), Ok(()));
    assert_eq!(scope("latest_queue_event", json!([["ci_checked"]])), Ok(()));
    for (op, args) in [
        ("ci_failure_findings_of", json!([6])),
        ("ci_failure_findings_of", json!([null])),
        ("record_ci_check", json!([{}])),
    ] {
        let refused = scope(op, args.clone()).unwrap_err();
        assert!(refused.contains(op), "{refused}");
    }
}

/// A supervisor answers an ask only as the runtime withdrawing it: as a
/// person or the inbox it answers none.
#[test]
fn a_supervisor_answers_an_ask_only_as_the_runtime() {
    use crate::domain::Answerer;
    assert_eq!(supervisor_answers_as(&Answerer::RUNTIME), Ok(()));
    for answerer in [Answerer::PERSON, Answerer::INBOX] {
        let refused = supervisor_answers_as(&answerer).unwrap_err();
        assert!(refused.contains(answerer.by), "{refused}");
    }
}

/// A clock at a fixed time.
struct At;

impl crate::application::Clock for At {
    fn system_time(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000)
    }
    fn monotonic(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

/// Identities counted from 1.
#[derive(Default)]
struct Counted(std::sync::atomic::AtomicU64);

impl crate::application::IdGenerator for Counted {
    fn uuid(&self) -> String {
        let next = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        format!("record-{next}")
    }
}

/// The fixed clock [`At`] and identities [`Counted`] from 1.
fn generators() -> crate::application::Generators {
    crate::application::Generators {
        clock: Arc::new(At),
        ids: Arc::new(Counted::default()),
    }
}

/// A service that answers or not as `down` says, and keeps what it took.
#[derive(Default)]
struct Answering {
    down: std::sync::atomic::AtomicBool,
    busy: std::sync::atomic::AtomicBool,
    refuse: std::sync::atomic::AtomicBool,
    took: std::sync::Mutex<Vec<Value>>,
}

impl ServiceCaller for Answering {
    fn call(&self, _: UseCase, params: Value) -> Result<Value> {
        use std::sync::atomic::Ordering;
        if self.down.load(Ordering::SeqCst) {
            return Err(ServiceUnreachable::not_done("down").into());
        }
        if self.busy.load(Ordering::SeqCst) {
            return Err(service_error(&failed(Some(FailureKind::QueueBusy))));
        }
        if self.refuse.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("refused"));
        }
        self.took.lock().unwrap().push(params);
        Ok(Value::Null)
    }
    fn with_token(&self, _: &std::path::Path) -> Result<Arc<dyn ServiceCaller>> {
        unreachable!("no landing here")
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn hand_over(&self) {}
    fn sign_out(&self) {}
    fn credential(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// The supervisor's loop holds a result's record the service did not
/// answer and sends it first, under the identity its first sending gave
/// it, once the service answers; one the service then refuses is let go.
/// Any other operation is not held.
#[test]
fn a_results_record_the_service_did_not_answer_is_held_until_it_answers() {
    use resend::{Resend, Resending};
    use std::sync::atomic::Ordering;
    let service = Arc::new(Answering::default());
    let caller = Resending::new(service.clone(), Resend::Hold, generators());
    let record = |dir: &str| serde_json::json!({"op": "record_queue_event", "args": ["observe_finished", {"dir": dir}]});
    service.down.store(true, Ordering::SeqCst);
    let read = caller.call(
        UseCase::Supervisor,
        serde_json::json!({"op": "active_runs"}),
    );
    assert!(unreachable(&read.unwrap_err()));
    for dir in ["/1", "/2"] {
        let held = caller.call(UseCase::Supervisor, record(dir));
        assert!(unreachable(&held.unwrap_err()));
    }
    assert!(unreachable(&caller.send_held().unwrap_err()));
    service.down.store(false, Ordering::SeqCst);
    caller.send_held().unwrap();
    let took = service.took.lock().unwrap().clone();
    let dirs: Vec<&Value> = took
        .iter()
        .map(|params| &params["args"][1]["dir"])
        .collect();
    assert_eq!(dirs, ["/1", "/2"]);
    let records: Vec<&Value> = took.iter().map(|params| &params["record"]).collect();
    assert_eq!(records, ["record-1", "record-2"]);
    caller.send_held().unwrap();
    assert_eq!(service.took.lock().unwrap().len(), 2);

    service.down.store(true, Ordering::SeqCst);
    assert!(caller.call(UseCase::Supervisor, record("/3")).is_err());
    service.down.store(false, Ordering::SeqCst);
    // A queue busy past its wait keeps it for the next pass, and says one
    // is still held.
    service.busy.store(true, Ordering::SeqCst);
    assert!(!caller.send_held().unwrap());
    service.busy.store(false, Ordering::SeqCst);
    assert!(caller.send_held().unwrap());
    assert_eq!(service.took.lock().unwrap().len(), 3);
    // One the service refuses is let go.
    service.down.store(true, Ordering::SeqCst);
    assert!(caller.call(UseCase::Supervisor, record("/4")).is_err());
    service.down.store(false, Ordering::SeqCst);
    service.refuse.store(true, Ordering::SeqCst);
    caller.send_held().unwrap();
    service.refuse.store(false, Ordering::SeqCst);
    caller.send_held().unwrap();
    assert_eq!(service.took.lock().unwrap().len(), 3);
}

/// A result's record whose first sending meets a queue busy past its wait
/// is held as one the service did not answer, and the end of the loop is
/// told one is still held while the queue stays busy.
#[test]
fn a_results_record_the_queue_was_busy_for_is_held_and_told_as_still_held() {
    use resend::{Resend, Resending};
    use std::sync::atomic::Ordering;
    let service = Arc::new(Answering::default());
    let caller = Resending::new(service.clone(), Resend::Hold, generators());
    service.busy.store(true, Ordering::SeqCst);
    let busy = caller
        .call(
            UseCase::Supervisor,
            serde_json::json!({"op": "record_queue_event", "args": ["observe_finished", {}]}),
        )
        .unwrap_err();
    assert!(crate::application::QueueBusy::is(&busy), "{busy:#}");
    assert!(!caller.send_held().unwrap());
    assert!(service.took.lock().unwrap().is_empty());
    service.busy.store(false, Ordering::SeqCst);
    assert!(caller.send_held().unwrap());
    let took = service.took.lock().unwrap().clone();
    assert_eq!(took.len(), 1);
    assert_eq!(took[0]["record"], "record-1");
    assert!(caller.send_held().unwrap());
    assert_eq!(service.took.lock().unwrap().len(), 1);
}

/// A job's end recorded while the one before it is held sends that one
/// first: it is kept behind it, unsent, while the queue stays busy for it,
/// and goes after it once the queue takes it, so the queue takes them in
/// their order and the caller meets an error only where its own sending
/// would have; behind a record the service did not answer the same.
#[test]
fn a_record_after_one_the_queue_was_busy_for_is_kept_behind_it() {
    use resend::{Resend, Resending};
    use std::sync::atomic::Ordering;
    let phase = |to: &str| serde_json::json!({"op": "record_runtime_event", "args": ["r1", "review_job_finished", {"to": to}]});
    let phases = |took: &[Value]| -> Vec<Value> {
        took.iter()
            .filter(|params| params["op"] == "record_runtime_event")
            .map(|params| params["args"][2]["to"].clone())
            .collect()
    };
    let service = Arc::new(BusyFirst::default());
    let caller = Resending::new(service.clone(), Resend::Hold, generators());
    let took = || service.took.lock().unwrap().clone();
    // B meets the queue busy, and so does C's sending of B first.
    service.busy.store(2, Ordering::SeqCst);
    let busy = caller.call(UseCase::Supervisor, phase("B")).unwrap_err();
    assert!(resend::kept(&busy), "{busy:#}");
    let behind = caller.call(UseCase::Supervisor, phase("C")).unwrap_err();
    assert!(resend::kept(&behind), "{behind:#}");
    assert!(crate::application::QueueBusy::is(&behind), "{behind:#}");
    assert!(caller.holds_for_busy_queue());
    assert!(phases(&took()).is_empty());
    // Another operation is no record and goes as it is.
    caller
        .call(
            UseCase::Supervisor,
            serde_json::json!({"op": "active_runs"}),
        )
        .unwrap();
    assert!(caller.send_held().unwrap());
    assert!(!caller.holds_for_busy_queue());
    caller.call(UseCase::Supervisor, phase("D")).unwrap();
    assert_eq!(phases(&took()), ["B", "C", "D"]);
    // E meets the queue busy; F, which the queue takes, sends E first.
    service.busy.store(1, Ordering::SeqCst);
    assert!(resend::kept(
        &caller.call(UseCase::Supervisor, phase("E")).unwrap_err()
    ));
    caller.call(UseCase::Supervisor, phase("F")).unwrap();
    assert_eq!(phases(&took()), ["B", "C", "D", "E", "F"]);
    let records: Vec<Value> = took()
        .iter()
        .filter(|params| params["op"] == "record_runtime_event")
        .map(|params| params["record"].clone())
        .collect();
    assert_eq!(
        records,
        ["record-1", "record-2", "record-3", "record-4", "record-5"]
    );
    assert!(caller.send_held().unwrap());
    assert_eq!(phases(&took()).len(), 5);

    // Behind a record the service did not answer, one is answered as the
    // service out of reach and not sent; once it answers, both go first.
    let service = Arc::new(Answering::default());
    let caller = Resending::new(service.clone(), Resend::Hold, generators());
    service.down.store(true, Ordering::SeqCst);
    assert!(unreachable(
        &caller.call(UseCase::Supervisor, phase("G")).unwrap_err()
    ));
    let behind = caller.call(UseCase::Supervisor, phase("H")).unwrap_err();
    assert!(unreachable(&behind) && resend::kept(&behind), "{behind:#}");
    assert!(crate::application::supervisor_service::undone(&behind));
    assert!(!caller.holds_for_busy_queue());
    service.down.store(false, Ordering::SeqCst);
    caller.call(UseCase::Supervisor, phase("I")).unwrap();
    assert_eq!(phases(&service.took.lock().unwrap()), ["G", "H", "I"]);
}

/// A service whose queue is busy for the first `busy` calls.
#[derive(Default)]
struct BusyFirst {
    busy: std::sync::atomic::AtomicUsize,
    took: std::sync::Mutex<Vec<Value>>,
}

impl ServiceCaller for BusyFirst {
    fn call(&self, _: UseCase, params: Value) -> Result<Value> {
        use std::sync::atomic::Ordering;
        if self
            .busy
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(service_error(&failed(Some(FailureKind::QueueBusy))));
        }
        self.took.lock().unwrap().push(params);
        Ok(Value::Null)
    }
    fn with_token(&self, _: &std::path::Path) -> Result<Arc<dyn ServiceCaller>> {
        unreachable!("no landing here")
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn hand_over(&self) {}
    fn sign_out(&self) {}
    fn credential(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// The one monotonic time the tests start from, read once for the
/// process: a clock gives an offset from it as a value.
fn origin() -> std::time::Instant {
    static ORIGIN: std::sync::LazyLock<std::time::Instant> =
        std::sync::LazyLock::new(std::time::Instant::now);
    *ORIGIN
}

/// A clock that moves only as a caller's pauses move it.
#[derive(Default)]
struct Stepped(std::sync::Mutex<std::time::Duration>);

impl crate::application::Clock for Stepped {
    fn system_time(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000)
    }
    fn monotonic(&self) -> std::time::Instant {
        origin() + *self.0.lock().unwrap()
    }
}

/// A caller that waits, on `clock`, with its pauses recorded and moving
/// the clock instead of sleeping.
fn waiting(
    service: Arc<dyn ServiceCaller>,
    waits: resend::Waits,
) -> (
    resend::Resending,
    Arc<Stepped>,
    Arc<std::sync::Mutex<Vec<std::time::Duration>>>,
) {
    let clock = Arc::new(Stepped::default());
    let pauses = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (moved, recorded) = (clock.clone(), pauses.clone());
    let generators = crate::application::Generators {
        clock: clock.clone(),
        ids: Arc::new(Counted::default()),
    };
    let caller = resend::Resending::with_waits(service, resend::Resend::Wait, generators, waits)
        .pausing_with(Arc::new(move |pause| {
            *moved.0.lock().unwrap() += pause;
            recorded.lock().unwrap().push(pause);
        }));
    (caller, clock, pauses)
}

/// A caller that waits sends a record its queue was busy for again until
/// it is taken, kept meanwhile and let go once taken; past its limit it
/// stops with the record kept, which a later `send_held` sends.
#[test]
fn a_waiting_caller_sends_a_busy_record_again_and_keeps_it_past_its_limit() {
    use std::time::Duration;
    let ms = Duration::from_millis;
    let record =
        serde_json::json!({"op": "record_queue_event", "args": ["throughput_review_finished", {}]});
    let waits = resend::Waits {
        first: ms(1),
        longest: ms(2),
        limit: ms(4),
    };
    let service = Arc::new(BusyFirst::default());
    service.busy.store(2, std::sync::atomic::Ordering::SeqCst);
    let (caller, _, pauses) = waiting(service.clone(), waits);
    caller.call(UseCase::Supervisor, record.clone()).unwrap();
    assert_eq!(*pauses.lock().unwrap(), [ms(1), ms(2)]);
    assert_eq!(service.took.lock().unwrap().len(), 1);
    // Taken, it is kept no longer.
    assert!(caller.send_held().unwrap());
    assert_eq!(service.took.lock().unwrap().len(), 1);

    service.busy.store(100, std::sync::atomic::Ordering::SeqCst);
    let (giving_up, clock, pauses) = waiting(service.clone(), waits);
    let kept = giving_up.call(UseCase::Supervisor, record).unwrap_err();
    assert!(
        format!("{kept:#}").contains("kept for the supervisor"),
        "{kept:#}"
    );
    // 1 ms, 2 ms, and 2 ms more as 3 ms is still under the limit of 4.
    assert_eq!(*pauses.lock().unwrap(), [ms(1), ms(2), ms(2)]);
    assert_eq!(*clock.0.lock().unwrap(), ms(5));
    assert!(service.took.lock().unwrap().len() == 1);
    service.busy.store(0, std::sync::atomic::Ordering::SeqCst);
    assert!(giving_up.send_held().unwrap());
    assert_eq!(service.took.lock().unwrap().len(), 2);
}

/// A service that does not answer the first `down` calls, and keeps
/// every call sent to it.
#[derive(Default)]
struct DownFirst {
    down: std::sync::atomic::AtomicUsize,
    sent: std::sync::Mutex<Vec<Value>>,
}

impl ServiceCaller for DownFirst {
    fn call(&self, _: UseCase, params: Value) -> Result<Value> {
        use std::sync::atomic::Ordering;
        self.sent.lock().unwrap().push(params);
        if self
            .down
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(ServiceUnreachable::not_done("down").into());
        }
        Ok(Value::Null)
    }
    fn with_token(&self, _: &std::path::Path) -> Result<Arc<dyn ServiceCaller>> {
        unreachable!("no landing here")
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn hand_over(&self) {}
    fn sign_out(&self) {}
    fn credential(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// A person's or the inbox's control-side command sends the record of a
/// result it made (its observation's end) again past the limit a job
/// command keeps one at, under the identity of its first sending, until
/// the service takes it: a service down that long loses nothing of it.
#[test]
fn a_control_commands_record_is_sent_until_taken_past_a_job_commands_limit() {
    let service = Arc::new(DownFirst::default());
    service
        .down
        .store(1000, std::sync::atomic::Ordering::SeqCst);
    let clock = Arc::new(Stepped::default());
    let moved = clock.clone();
    let generators = crate::application::Generators {
        clock: clock.clone(),
        ids: Arc::new(Counted::default()),
    };
    let caller =
        control::resending(service.clone(), generators).pausing_with(Arc::new(move |pause| {
            *moved.0.lock().unwrap() += pause;
        }));
    let finished =
        serde_json::json!({"op": "record_queue_event", "args": ["observe_finished", {}]});
    caller.call(UseCase::Supervisor, finished).unwrap();
    assert!(
        *clock.0.lock().unwrap() > 2 * resend::Waits::DEFAULT.limit,
        "{:?}",
        clock.0.lock().unwrap()
    );
    let sent = service.sent.lock().unwrap();
    assert_eq!(sent.len(), 1001);
    assert!(sent[0]["record"].is_string(), "{:?}", sent[0]);
    assert!(
        sent.iter()
            .all(|params| params["record"] == sent[0]["record"])
    );
}

/// Two results alike are two records, each with its own identity, held
/// and sent as two; a record whose answer was lost is sent again under the
/// identity of its first sending, so the queue takes it once; and a
/// result sent after a held one was taken is a record of its own, sent
/// with nothing answered for it beforehand.
#[test]
fn two_results_alike_are_two_records_and_one_sent_again_keeps_its_identity() {
    use resend::{Resend, Resending};
    use std::sync::atomic::Ordering;
    let service = Arc::new(Answering::default());
    let caller = Resending::new(service.clone(), Resend::Hold, generators());
    let draining =
        || serde_json::json!({"op": "record_queue_event", "args": ["supervisor_draining", {}]});
    service.busy.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        assert!(caller.call(UseCase::Supervisor, draining()).is_err());
    }
    service.busy.store(false, Ordering::SeqCst);
    assert!(caller.send_held().unwrap());
    let records = || -> Vec<Value> {
        service
            .took
            .lock()
            .unwrap()
            .iter()
            .map(|params| params["record"].clone())
            .collect()
    };
    assert_eq!(records(), ["record-1", "record-2"]);
    caller.call(UseCase::Supervisor, draining()).unwrap();
    assert_eq!(records(), ["record-1", "record-2", "record-3"]);
    assert!(caller.send_held().unwrap());
    assert_eq!(records().len(), 3);

    // Done and its answer lost: held, and sent again as the same record.
    let lost = Arc::new(LosesFirstAnswer::default());
    let caller = Resending::new(lost.clone(), Resend::Hold, generators());
    let stopped =
        serde_json::json!({"op": "record_queue_event", "args": ["supervisor_stopped", {}]});
    assert!(unreachable(
        &caller.call(UseCase::Supervisor, stopped).unwrap_err()
    ));
    assert!(caller.send_held().unwrap());
    let took: Vec<Value> = lost
        .took
        .lock()
        .unwrap()
        .iter()
        .map(|params| params["record"].clone())
        .collect();
    assert_eq!(took, ["record-1", "record-1"]);
}

/// A service that does its first request and loses its answer, and
/// answers the later ones, keeping all it took.
#[derive(Default)]
struct LosesFirstAnswer {
    took: std::sync::Mutex<Vec<Value>>,
}

impl ServiceCaller for LosesFirstAnswer {
    fn call(&self, _: UseCase, params: Value) -> Result<Value> {
        let mut took = self.took.lock().unwrap();
        took.push(params);
        if took.len() == 1 {
            Err(ServiceUnreachable::maybe_done("the answer was lost").into())
        } else {
            Ok(Value::Null)
        }
    }
    fn with_token(&self, _: &std::path::Path) -> Result<Arc<dyn ServiceCaller>> {
        anyhow::bail!("no landing here")
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn hand_over(&self) {}
    fn sign_out(&self) {}
    fn credential(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// A job's end kept at the supervisor's end is sent with the held
/// records; a kept record that is no result's record or job's end is let
/// go unsent.
#[test]
fn a_kept_job_end_is_sent_and_a_kept_record_of_another_operation_is_not() {
    use resend::{JOB_END_OP, Resend, Resending};
    let service = Arc::new(Answering::default());
    let caller = Resending::new(service.clone(), Resend::Hold, generators());
    caller
        .keep(
            UseCase::Supervisor,
            serde_json::json!({"op": JOB_END_OP, "args": [7, "ended"]}),
        )
        .unwrap();
    caller
        .keep(
            UseCase::Supervisor,
            serde_json::json!({"op": "transition", "args": [1, "BypassReview"]}),
        )
        .unwrap();
    // A result's record kept unsent (a job's stop past an owner's grace)
    // has its identity, so the queue takes it once.
    let stopped = serde_json::json!({"op": "record_runtime_event",
        "args": ["r1", "headless_job_stopped", {}]});
    caller.keep(UseCase::Supervisor, stopped.clone()).unwrap();
    assert!(caller.send_held().unwrap());
    let took = service.took.lock().unwrap().clone();
    assert_eq!(took.len(), 2, "{took:?}");
    assert_eq!(
        took[0],
        serde_json::json!({"op": JOB_END_OP, "args": [7, "ended"]})
    );
    assert_eq!(
        (&took[1]["op"], &took[1]["args"]),
        (&stopped["op"], &stopped["args"])
    );
    assert!(took[1]["record"].is_string(), "{took:?}");
    assert!(caller.send_held().unwrap());
    assert_eq!(service.took.lock().unwrap().len(), 2);
}

/// The wait before a record is sent again doubles from its first to its
/// longest, and sending again stops at its limit: at the limit exactly
/// the record is kept, 1 ms before it is sent again.
#[test]
fn the_wait_before_a_record_is_sent_again_doubles_to_its_longest_until_its_limit() {
    use resend::{Again, Waits};
    use std::time::Duration;
    let ms = Duration::from_millis;
    let waits = Waits {
        first: ms(1000),
        longest: ms(4000),
        limit: ms(10_000),
    };
    assert_eq!(waits.next(ms(9_999), 0), Again::SendAfter(ms(1000)));
    assert_eq!(waits.next(ms(10_000), 0), Again::Keep);
    assert_eq!(waits.next(ms(10_001), 5), Again::Keep);
    let after: Vec<Again> = (0..4).map(|tries| waits.next(ms(0), tries)).collect();
    assert_eq!(
        after,
        [ms(1000), ms(2000), ms(4000), ms(4000)].map(Again::SendAfter)
    );
    // The longest 1 ms under the doubled wait cuts it; 1 ms over does not.
    let under = Waits {
        longest: ms(3999),
        ..waits
    };
    assert_eq!(under.next(ms(0), 2), Again::SendAfter(ms(3999)));
    let over = Waits {
        longest: ms(4001),
        ..waits
    };
    assert_eq!(over.next(ms(0), 2), Again::SendAfter(ms(4000)));
    assert_eq!(waits.next(ms(0), u32::MAX), Again::SendAfter(ms(4000)));
    // A landing's integrator sends again for as long as it takes.
    assert_eq!(
        Waits::LANDING.next(Duration::from_secs(365 * 24 * 3600), 40),
        Again::SendAfter(Waits::LANDING.longest)
    );
}

/// Only a result's record is sent again, and a record keeps the identity
/// its first sending gave it.
#[test]
fn only_a_results_record_is_sent_again_under_its_first_identity() {
    use resend::{resent, with_record};
    let record = serde_json::json!({"op": "finish_integration", "args": []});
    assert!(resent(UseCase::Supervisor, &record));
    assert!(!resent(
        UseCase::Supervisor,
        &serde_json::json!({"op": "claim", "args": []})
    ));
    // The outage's own record is recorded again from its file, not here.
    assert!(!resent(
        UseCase::Supervisor,
        &serde_json::json!({"op": "record_queue_event", "args": ["queue_service_down", {}]})
    ));
    let again = with_record(with_record(record, "first".into()), "second".into());
    assert_eq!(again["record"], "first");
    // A job command's finding (a weekly review's next move) too.
    assert!(resent(
        UseCase::Supervisor,
        &serde_json::json!({"op": "record_finding", "args": [{}]})
    ));
}

/// A change a pass finds against the latest of its kinds on the queue is
/// found again by the next pass, so it is not sent again and held, where
/// that read would miss it and find it once more; one its caller keeps in
/// memory (the broker answering again) or a side effect's result is.
#[test]
fn a_change_found_against_the_queue_is_found_again_and_not_held() {
    let event = |kind: &str| serde_json::json!({"op": "record_queue_event", "args": [kind, {}]});
    for kind in [
        "claim_held",
        "landing_resumed",
        "run_env_program_missing",
        "run_env_changed",
        "container_runtime_unavailable",
        "landing_recheck_main_failed",
        "landing_recheck_target_writable",
        "queue_broker_down",
        "queue_relay_down",
        "queue_relay_running",
    ] {
        assert!(!resend::resent(UseCase::Supervisor, &event(kind)), "{kind}");
    }
    for kind in [
        "queue_broker_running",
        "queue_broker_started",
        "observe_finished",
    ] {
        assert!(resend::resent(UseCase::Supervisor, &event(kind)), "{kind}");
    }
}

/// A service holding the tokens of `principals`, which does not revoke the
/// one kept under `refused` (refusing it, or out of reach when
/// `unreachable`) and keeps the keys it was asked to revoke.
struct Tokens {
    principals: Vec<Principal>,
    refused: String,
    unreachable: bool,
    asked: std::sync::Mutex<Vec<String>>,
}

impl ServiceCaller for Tokens {
    fn call(&self, _: UseCase, params: Value) -> Result<Value> {
        let params: SupervisorParams = serde_json::from_value(params)?;
        match params.op.as_str() {
            "token_principals" => Ok(serde_json::to_value(&self.principals)?),
            "revoke_principal" => {
                let principal: Principal = serde_json::from_value(params.args[0].clone())?;
                let key = principal.credential_key();
                self.asked.lock().unwrap().push(key.clone());
                if key != self.refused {
                    Ok(Value::Bool(true))
                } else if self.unreachable {
                    Err(ServiceUnreachable::not_done("no socket").into())
                } else {
                    Err(service_error(&ServiceError {
                        code: ServiceErrorCode::BadRequest,
                        message: format!("a supervisor revokes no token of {key}"),
                        denied: None,
                        kind: None,
                    }))
                }
            }
            op => panic!("no {op} here"),
        }
    }
    fn with_token(&self, _: &std::path::Path) -> Result<Arc<dyn ServiceCaller>> {
        anyhow::bail!("no landing here")
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn hand_over(&self) {}
    fn sign_out(&self) {}
    fn credential(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// A token the service does not revoke leaves the ended ones after it to
/// be revoked, and a service out of reach ends the sweep there.
#[test]
fn a_sweep_goes_on_past_a_token_the_service_does_not_revoke() {
    let first = Principal::of(&ActorContext::new(ActorRole::Worker, "worker:r1"));
    let second = Principal::of(&ActorContext::new(ActorRole::Worker, "worker:r2"));
    let sweep = |unreachable: bool| {
        let service = Arc::new(Tokens {
            principals: vec![first.clone(), second.clone()],
            refused: first.credential_key(),
            unreachable,
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let queue = SupervisorQueues::new(
            service.clone(),
            ActorContext::instance(ActorRole::Supervisor, 1),
        )
        .queue();
        let revoked = sweep_tokens(&queue, &mut |_| true);
        (revoked, service.asked.lock().unwrap().clone())
    };
    let (revoked, asked) = sweep(false);
    assert_eq!(revoked.unwrap(), vec!["worker:r2".to_owned()]);
    assert_eq!(asked, ["worker:r1", "worker:r2"]);
    let (revoked, asked) = sweep(true);
    assert!(unreachable(&revoked.unwrap_err()));
    assert_eq!(asked, ["worker:r1"]);
}

/// Records kept nowhere: each keeping fails.
struct NoKeeping;

impl resend::HeldRecords for NoKeeping {
    fn keep(&self, _: UseCase, _: &Value) -> Result<String> {
        anyhow::bail!("the disk is full")
    }
    fn kept(&self) -> Result<Vec<resend::KeptRecord>> {
        Ok(Vec::new())
    }
    fn forget(&self, _: &str) -> Result<()> {
        Ok(())
    }
}

/// A run's event that starts something (`revise_requested`, before its
/// request goes to `turns/`) or that a pass finds against the run's
/// history (`provider_waiting`), sent as such
/// (`EventStore::record_runtime_event_now`), is never kept (ADR-t3102-3
/// decision 5): one the queue is busy for, or the service does not answer,
/// is its caller's error, so the caller goes no further, and nothing is
/// left to be written later. Every other run's event is a result, of
/// whatever kind, and is kept. While a job's end is held, such an event is
/// not sent ahead of it, and goes after it once the queue takes it. A
/// caller that waits keeps none past its wait.
#[test]
fn a_runs_event_that_starts_something_or_is_found_again_is_never_kept() {
    use crate::domain::EventKind;
    use resend::{Resend, Resending, Waits, kept, not_taken};
    use std::sync::atomic::Ordering;
    let event =
        |kind: &str| serde_json::json!({"op": "record_runtime_event", "args": ["r1", kind, {}]});

    let service = Arc::new(Answering::default());
    let caller: Arc<dyn ServiceCaller> =
        Arc::new(Resending::new(service.clone(), Resend::Hold, generators()));
    let queue = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, 1),
    )
    .queue();
    let run = RunId::new("r1").unwrap();
    let record = |kind: EventKind| {
        EventStore::record_runtime_event_now(&queue, &run, kind, serde_json::json!({}))
    };
    let result = |kind: EventKind| {
        EventStore::record_runtime_event(&queue, &run, kind, serde_json::json!({}))
    };
    let took = || -> Vec<Value> {
        service
            .took
            .lock()
            .unwrap()
            .iter()
            .map(|params| params["args"][1].clone())
            .collect()
    };
    service.busy.store(true, Ordering::SeqCst);
    for kind in [EventKind::ReviseRequested, EventKind::ProviderWaiting] {
        let error = record(kind).unwrap_err();
        assert!(
            crate::application::QueueBusy::is(&error) && !kept(&error),
            "{kind:?}: {error:#}"
        );
    }
    service.busy.store(false, Ordering::SeqCst);
    service.down.store(true, Ordering::SeqCst);
    let error = record(EventKind::ReviseRequested).unwrap_err();
    assert!(unreachable(&error) && !kept(&error), "{error:#}");
    service.down.store(false, Ordering::SeqCst);
    assert!(caller.send_held().unwrap());
    assert!(took().is_empty());

    // Results recorded once after what they did (a turn's request written,
    // a resolution sent, a push, a review's failure asked) are kept while
    // the queue is busy, and written in their order once it takes them.
    service.busy.store(true, Ordering::SeqCst);
    for kind in [
        EventKind::TurnRequested,
        EventKind::ResumeRequestSent,
        EventKind::PushFinished,
        EventKind::ReviewFailed,
    ] {
        result(kind).unwrap();
    }
    service.busy.store(false, Ordering::SeqCst);
    assert!(caller.send_held().unwrap());
    assert_eq!(
        took(),
        [
            "turn_requested",
            "resume_request_sent",
            "push_finished",
            "review_failed"
        ]
    );
    service.took.lock().unwrap().clear();

    // A job's end the queue is busy for is kept, and the start after it
    // waits behind it, unsent.
    service.busy.store(true, Ordering::SeqCst);
    result(EventKind::ReviewJobFinished).unwrap();
    let error = record(EventKind::ReviseRequested).unwrap_err();
    assert!(
        crate::application::QueueBusy::is(&error) && !kept(&error) && not_taken(&error),
        "{error:#}"
    );
    assert!(took().is_empty());
    service.busy.store(false, Ordering::SeqCst);
    record(EventKind::ReviseRequested).unwrap();
    assert_eq!(took(), ["review_job_finished", "revise_requested"]);
    let sent = service.took.lock().unwrap().clone();
    assert!(sent[0]["record"].is_string(), "{sent:?}");
    assert!(sent[1]["record"].is_null(), "{sent:?}");

    // A caller that waits keeps no such event past its wait, and keeps a
    // result.
    let waits = Waits {
        first: std::time::Duration::ZERO,
        longest: std::time::Duration::ZERO,
        limit: std::time::Duration::ZERO,
    };
    let waiting = Resending::with_waits(service.clone(), Resend::Wait, generators(), waits);
    service.busy.store(true, Ordering::SeqCst);
    let start = waiting
        .call_now(UseCase::Supervisor, event("revise_requested"))
        .unwrap_err();
    assert!(not_taken(&start) && !kept(&start), "{start:#}");
    let end = waiting
        .call(UseCase::Supervisor, event("review_job_finished"))
        .unwrap_err();
    assert!(kept(&end), "{end:#}");
    service.busy.store(false, Ordering::SeqCst);
    assert!(waiting.send_held().unwrap());
    assert_eq!(
        took(),
        [
            "review_job_finished",
            "revise_requested",
            "review_job_finished"
        ]
    );
}

/// A result's record the service did not take is answered as kept, still
/// not taken, so a caller that records it again on a later pass treats it
/// as recorded; an event, whose answer is nothing, is recorded as far as
/// its caller goes and sent once. A record that could not be kept, and
/// any other operation, is no kept record.
#[test]
fn a_kept_record_is_told_from_one_lost_and_an_event_kept_is_recorded() {
    use resend::{Resend, Resending, kept, not_taken};
    use std::sync::atomic::Ordering;
    let service = Arc::new(Answering::default());
    let caller: Arc<dyn ServiceCaller> =
        Arc::new(Resending::new(service.clone(), Resend::Hold, generators()));
    service.busy.store(true, Ordering::SeqCst);
    let draining =
        || serde_json::json!({"op": "record_queue_event", "args": ["supervisor_draining", {}]});
    let held = caller.call(UseCase::Supervisor, draining()).unwrap_err();
    assert!(kept(&held) && not_taken(&held), "{held:#}");
    let read = caller
        .call(
            UseCase::Supervisor,
            serde_json::json!({"op": "active_runs"}),
        )
        .unwrap_err();
    assert!(!kept(&read));
    let queue = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, 1),
    )
    .queue();
    crate::application::EventStore::record_runtime_event(
        &queue,
        &RunId::new("r1").unwrap(),
        crate::domain::EventKind::ReviewJobFinished,
        serde_json::json!({"job": "acceptance"}),
    )
    .unwrap();
    service.busy.store(false, Ordering::SeqCst);
    assert!(caller.send_held().unwrap());
    let ops: Vec<Value> = service
        .took
        .lock()
        .unwrap()
        .iter()
        .map(|params| params["op"].clone())
        .collect();
    assert_eq!(ops, ["record_queue_event", "record_runtime_event"]);

    service.down.store(true, Ordering::SeqCst);
    let lost =
        Resending::new(service.clone(), Resend::Hold, generators()).kept_in(Arc::new(NoKeeping));
    let error = lost.call(UseCase::Supervisor, draining()).unwrap_err();
    assert!(unreachable(&error) && !kept(&error), "{error:#}");
}

/// The issue's facts with every registration kept, no job's records, and
/// `start` as the start of every target.
struct Started(Option<crate::domain::queue_service::TargetStart>);

impl super::super::queue_service::IssueFacts for Started {
    fn registered(&self, _: &LeaseToken) -> bool {
        true
    }
    fn job(
        &self,
        _: &crate::domain::queue_service::HeldToken,
        _: Option<&crate::domain::queue_service::TokenIssuer>,
    ) -> crate::domain::queue_service::JobState {
        crate::domain::queue_service::JobState::MayRun
    }
    fn target_start(
        &self,
        _: &StepTarget,
    ) -> Result<Option<crate::domain::queue_service::TargetStart>> {
        Ok(self.0.clone())
    }
}

/// The supervisor's `issue_telemetry` under no lease goes only to the
/// command of this supervisor that started the target, asking from the
/// process its start names (ADR-t3102-1 decision 2): another supervisor's
/// start, another process's, a supervised job's row and a target with no
/// start are refused, and under a lease the registration rules alone.
#[test]
fn a_supervisors_leaseless_telemetry_goes_only_to_the_command_that_started_it() {
    use crate::domain::queue_service::TargetStart;
    use crate::domain::session_step::TargetKind;
    let lease = LeaseToken::new("a");
    let a = Principal::supervisor(&lease, 7);
    let b = Principal::supervisor(&LeaseToken::new("b"), 8);
    let target = StepTarget::of(TargetKind::Observer, 3);
    let by = |principal: &Principal, pid| {
        Started(Some(TargetStart::Command {
            actor: Some((principal.role, principal.actor_id.clone())),
            pid: Some(pid),
        }))
    };
    let refusal = |parent: &Principal, process, under: Option<&LeaseToken>, facts: &Started| {
        leaseless_starter_refusal(parent, process, under, &target, facts).unwrap()
    };
    assert_eq!(refusal(&a, Some(40), None, &by(&a, 40)), None);
    for (parent, process) in [(&b, Some(40)), (&a, Some(41)), (&a, None)] {
        assert_eq!(
            refusal(parent, process, None, &by(&a, 40)),
            Some("the target was started by another caller"),
            "{parent:?} {process:?}"
        );
    }
    let row = Started(Some(TargetStart::Supervised {
        supervisor: Some(LeaseToken::new("b")),
    }));
    assert!(refusal(&a, Some(40), None, &row).is_some());
    assert_eq!(
        refusal(&a, Some(40), None, &Started(None)),
        Some("the queue has no record of the target's start")
    );
    // Under a lease, the registration it names rules alone.
    assert_eq!(refusal(&b, Some(40), Some(&lease), &Started(None)), None);
}

/// A caller signed in with a token, or none, that records the operations
/// it is asked for and fails them all when `down`.
struct SignedInCaller {
    held: std::sync::Mutex<bool>,
    down: bool,
    ops: std::sync::Mutex<Vec<String>>,
}

impl SignedInCaller {
    fn new(held: bool, down: bool) -> Self {
        Self {
            held: std::sync::Mutex::new(held),
            down,
            ops: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl ServiceCaller for SignedInCaller {
    fn call(&self, use_case: UseCase, params: Value) -> Result<Value> {
        assert_eq!(use_case, UseCase::Supervisor);
        self.ops
            .lock()
            .unwrap()
            .push(params["op"].as_str().unwrap().to_owned());
        if self.down {
            return Err(ServiceUnreachable::not_done("the service is down").into());
        }
        Ok(Value::Bool(true))
    }
    fn with_token(&self, _: &std::path::Path) -> Result<Arc<dyn ServiceCaller>> {
        anyhow::bail!("no landing here")
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn hand_over(&self) {
        *self.held.lock().unwrap() = false;
    }
    fn sign_out(&self) {}
    fn credential(&self) -> Option<std::path::PathBuf> {
        self.held
            .lock()
            .unwrap()
            .then(|| std::path::PathBuf::from("token"))
    }
}

/// A supervisor that ends asks the service to revoke its token and then
/// forgets it, whether the service took the request or not (one it did
/// not is the sweep's); one that holds none (handed over for an exec, or
/// never signed in) asks for nothing.
#[test]
fn a_supervisor_signs_out_through_the_service_only_with_a_token() {
    for down in [false, true] {
        let caller = SignedInCaller::new(true, down);
        sign_out(&caller);
        assert_eq!(*caller.ops.lock().unwrap(), [SIGN_OUT], "down: {down}");
        assert_eq!(caller.credential(), None);
    }
    let handed = SignedInCaller::new(true, false);
    handed.hand_over();
    sign_out(&handed);
    assert!(handed.ops.lock().unwrap().is_empty());
}

/// The supervisor that stops the service asks it first to revoke its token
/// as it stops, and keeps the token for the record of the stop; one that
/// holds none asks for nothing.
#[test]
fn a_supervisor_asks_the_service_to_revoke_its_token_as_it_stops() {
    let caller = SignedInCaller::new(true, false);
    sign_out_at_stop(&caller);
    assert_eq!(*caller.ops.lock().unwrap(), [SIGN_OUT_AT_STOP]);
    assert!(caller.credential().is_some());
    let none = SignedInCaller::new(false, false);
    sign_out_at_stop(&none);
    assert!(none.ops.lock().unwrap().is_empty());
}
