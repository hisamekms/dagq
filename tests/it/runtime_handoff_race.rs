//! Runtime tests: a supervisor asked to hand off begins no landing, nor the
//! e2e before it (ADR-t3077-1 decision 1), decided in the write that would
//! start it, even when the request is recorded after the pass read none.
use crate::runtime_support;
use dagq::domain::LeaseToken;

use runtime_support::*;

/// The only registered supervisor.
fn only_registration(db: &Path) -> dagq::domain::SupervisorRegistration {
    let registrations = SqliteQueue::open(db).unwrap().supervisors().unwrap();
    assert_eq!(registrations.len(), 1, "{registrations:?}");
    registrations.into_iter().next().unwrap()
}

/// ADR-t3077-1 decision 1, in the store: a supervisor asked to hand off
/// begins no landing, decided in the write that would record
/// `integration_started`; once the request is withdrawn, the landing
/// begins.
#[test]
fn a_supervisor_asked_to_hand_off_begins_no_landing_in_the_same_write() {
    let (_dir, repo, db, run) = awaiting_run_owned();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let token = LeaseToken::new("handing-off");
    queue
        .register_supervisor(&token, std::process::id(), 1, "old")
        .unwrap();
    queue.accept_handoff(&token).unwrap();
    assert_eq!(queue.handoff_request(&token).unwrap(), None);
    // Asked after the supervisor read none, before it begins the landing.
    assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
    let main = sha(&git_out(&repo, &["rev-parse", "main"]));
    let refused = queue
        .begin_integration(run.id(), &token, &main)
        .unwrap_err();
    assert!(
        format!("{refused:#}").contains("asked to hand off"),
        "{refused:#}"
    );
    let detail = queue.show(run.task_id()).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(!event_kinds(&detail).contains(&"integration_started"));
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(
        refused
            .downcast_ref::<dagq::application::HandoffAsked>()
            .is_some()
    );

    assert!(queue.cancel_handoff(&token, "/next/dagq").unwrap());
    let landing = queue.begin_integration(run.id(), &token, &main).unwrap();
    assert_eq!(landing.status(), RunStatus::Integrating);
    assert!(event_kinds(&queue.show(run.task_id()).unwrap()).contains(&"integration_started"));
}

/// What [`ask_for_a_handoff_once_queued`] asks of the supervisor a test
/// runs on its own thread: the queue, the stop it sets with the request,
/// if any, whether it asked already, and what task 1 has come to when it
/// asks.
struct RacedHandoff {
    db: PathBuf,
    stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    asked: bool,
    when: fn(&dagq::domain::TaskDetail) -> bool,
}

/// Task 1's run was queued to land in its slot.
fn landing_queued(detail: &dagq::domain::TaskDetail) -> bool {
    event_kinds(detail).contains(&"landing_queued")
}

thread_local! {
    /// Per test thread, so the tests that race a handoff, in one process
    /// under `cargo test` or one each under nextest, share nothing.
    static RACED_HANDOFF: std::cell::RefCell<Option<RacedHandoff>> =
        const { std::cell::RefCell::new(None) };
}

/// A load average that asks the supervisor of this thread's
/// [`RACED_HANDOFF`] to hand off (and to stop, with a stop), once, at the
/// first look after task 1's run came to its `when` (queued to land, or
/// leased for its e2e): the supervisor looks at the load after its pass
/// read the handoff request (its claims and its slots' pass), so the
/// request comes between that read and the landing, or the e2e before it,
/// the pass would begin. On another thread (a landing's) it asks nothing.
fn ask_for_a_handoff_once_queued() -> Option<f64> {
    RACED_HANDOFF.with_borrow_mut(|raced| {
        let Some(raced) = raced.as_mut().filter(|raced| !raced.asked) else {
            return;
        };
        let mut queue = SqliteQueue::open(&raced.db).unwrap();
        if (raced.when)(&queue.show(TaskId::new(1)).unwrap()) {
            let token = only_registration(&raced.db).token;
            assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
            if let Some(stop) = &raced.stop {
                stop.store(true, Ordering::SeqCst);
            }
            raced.asked = true;
        }
    });
    Some(0.0)
}

/// ADR-t3077-1 decision 1 when the request and the landing race: a
/// handoff asked in the pass that would begin the landing of a run waiting
/// for the slot, after that pass read none, begins no landing. The run is
/// handed over with its lease (the next process lands it as in
/// `runtime_handoff::a_handoff_starts_no_landing_and_hands_over_a_run_awaiting_the_slot`).
/// With `stop`, a stop asked with it wins from the next pass, and its
/// drain lands the run whatever request it holds.
fn a_handoff_asked_after_the_pass_read_none(stop: bool) {
    let (_dir, repo, db) = fixture();
    let options = supervise_options(1, false);
    RACED_HANDOFF.set(Some(RacedHandoff {
        db: db.clone(),
        stop: stop.then(|| options.stop.clone()),
        asked: false,
        when: landing_queued,
    }));
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let reviewer = Arc::new(TestReviewer::new(&[verdict("pass", &[], "ready to land")]));
    // Not `once`: it returns on the handoff or the stop.
    let waiting = crate::common::within(crate::common::STEP_LIMIT, "the supervisor to return");
    let outcome = supervise_reviewed_owned_with(
        &db,
        &repo,
        &backend,
        &reviewer,
        &SuperviseOptions {
            load_average: ask_for_a_handoff_once_queued,
            ..options
        },
    );
    drop(waiting);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let started = event_kinds(&detail)
        .iter()
        .filter(|kind| **kind == "integration_started")
        .count();
    if stop {
        assert_ne!(outcome["outcome"], "handoff", "{outcome}");
        assert_eq!(run.status(), RunStatus::Integrated);
        assert_eq!(started, 1);
        return;
    }
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    assert_eq!(outcome["handed_over"], 1, "{outcome}");
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert_eq!(started, 0);
    let token = only_registration(&db).token;
    assert_eq!(
        queue
            .runs_leased_by(&token)
            .unwrap()
            .iter()
            .map(|run| run.id().clone())
            .collect::<Vec<_>>(),
        vec![run.id().clone()]
    );
}

#[test]
fn a_handoff_asked_after_the_pass_read_none_starts_no_landing_of_that_pass() {
    a_handoff_asked_after_the_pass_read_none(false);
}

#[test]
fn a_stop_with_a_handoff_asked_mid_pass_lands_the_run_awaiting_the_slot() {
    a_handoff_asked_after_the_pass_read_none(true);
}

/// ADR-t3077-1 decision 1, in the store: a supervisor asked to hand off
/// starts no e2e before a landing, decided in the write that would record
/// `run_e2e_started`; one whose stop wins over the handoff starts it, and
/// once the request is withdrawn it starts.
#[test]
fn a_supervisor_asked_to_hand_off_starts_no_e2e_in_the_same_write() {
    let (_dir, _repo, db, run) = awaiting_run_owned();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let token = LeaseToken::new("handing-off");
    queue
        .register_supervisor(&token, std::process::id(), 1, "old")
        .unwrap();
    queue.accept_handoff(&token).unwrap();
    assert_eq!(queue.handoff_request(&token).unwrap(), None);
    // Asked after the supervisor read none, before it starts the e2e.
    assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
    let refused = queue
        .begin_e2e(run.id(), &token, json!({"attempt": 1}))
        .unwrap_err();
    assert!(
        refused
            .downcast_ref::<dagq::application::HandoffAsked>()
            .is_some(),
        "{refused:#}"
    );
    let started = |queue: &mut SqliteQueue| {
        payloads(&queue.show(run.task_id()).unwrap(), "run_e2e_started")
            .into_iter()
            .map(|payload| payload["attempt"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(started(&mut queue), Vec::<Value>::new());

    queue
        .begin_e2e_over_handoff(run.id(), &token, json!({"attempt": 1}))
        .unwrap();
    assert_eq!(started(&mut queue), [json!(1)]);
    assert!(queue.cancel_handoff(&token, "/next/dagq").unwrap());
    queue
        .begin_e2e(run.id(), &token, json!({"attempt": 2}))
        .unwrap();
    assert_eq!(started(&mut queue), [json!(1), json!(2)]);
}

/// The options of a supervisor whose runs need the e2e: `ran` records
/// each e2e it runs ([`stub_e2e`]).
fn racing_e2e_options(ran: &Path, parallel: usize, once: bool) -> SuperviseOptions {
    SuperviseOptions {
        run_e2e: runtime::RunE2eOptions {
            command: Some(stub_e2e(ran)),
            ..Default::default()
        },
        ..supervise_options(parallel, once)
    }
}

/// Task 1's run was leased for the e2e it runs before it lands as
/// queued.
fn leased_for_e2e(detail: &dagq::domain::TaskDetail) -> bool {
    payloads(detail, "lease_acquired")
        .iter()
        .any(|payload| payload["reason"] == "e2e")
}

/// Supervise on this thread, with `options`, until the handoff that
/// [`ask_for_a_handoff_once_queued`] asks once task 1's run came to
/// `when`, and check that the pass started no e2e: the run, awaiting
/// integration, is handed over with its lease and no `run_e2e_started`,
/// and the e2e never ran.
fn hands_over_with_no_e2e(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
    options: SuperviseOptions,
    when: fn(&dagq::domain::TaskDetail) -> bool,
    ran: &Path,
) {
    RACED_HANDOFF.set(Some(RacedHandoff {
        db: db.to_owned(),
        stop: None,
        asked: false,
        when,
    }));
    let outcome = supervise_reviewed_owned_with(
        db,
        repo,
        backend,
        reviewer,
        &SuperviseOptions {
            load_average: ask_for_a_handoff_once_queued,
            ..options
        },
    );
    RACED_HANDOFF.set(None);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    assert_eq!(outcome["handed_over"], 1, "{outcome}");
    let mut queue = SqliteQueue::open(db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"run_e2e_started"), "{kinds:?}");
    assert!(!kinds.contains(&"integration_started"), "{kinds:?}");
    assert!(!ran.exists(), "the e2e ran");
    let token = only_registration(db).token;
    assert_eq!(
        queue
            .runs_leased_by(&token)
            .unwrap()
            .iter()
            .map(|run| run.id().clone())
            .collect::<Vec<_>>(),
        vec![run.id().clone()]
    );
}

/// ADR-t3077-1 decision 1 when the request and the e2e race: a handoff
/// asked in the pass that would start the e2e of a run that passed its
/// review and waits in its slot to land, after that pass read none,
/// starts no e2e. The run is handed over with its lease (the next process
/// lands it as in
/// `runtime_handoff::a_handoff_starts_no_landing_and_hands_over_a_run_awaiting_the_slot`).
#[test]
fn a_handoff_asked_after_the_pass_read_none_starts_no_e2e_of_that_pass() {
    let (dir, repo, db) = fixture();
    with_e2e_paths(&repo, "[\"change.txt\"]");
    let ran = dir.path().join("ran");
    let backend = TestWorkspace::new(
        &db,
        false,
        "printf 'fixed\\n' > fixed.txt; git add fixed.txt; commit work; receipt \"$(git rev-parse HEAD)\"",
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "ready to land")]);
    // Not `once`: it returns on the handoff.
    hands_over_with_no_e2e(
        &db,
        &repo,
        &backend,
        &reviewer,
        racing_e2e_options(&ran, 1, false),
        landing_queued,
        &ran,
    );
}

/// ADR-t3077-1 decision 1 for a run a person's `land` queued: a handoff
/// asked after the pass read none and after the run was leased for its
/// e2e, before the e2e starts, starts none. The run is handed over with
/// that lease.
#[test]
fn a_handoff_asked_after_a_queued_run_was_leased_for_its_e2e_starts_no_e2e() {
    let (dir, repo, db) = fixture();
    with_e2e_paths(&repo, "[\"change.txt\"]");
    let ran = dir.path().join("ran");
    let backend = TestWorkspace::new(
        &db,
        false,
        "printf 'fixed\\n' > fixed.txt; git add fixed.txt; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let concern = json!({"verdict": "concern", "reasons": ["a finding"], "summary": "judged",
                         "recommendation": "land", "confidence": "low", "reason_category": null});
    let reviewer = TestReviewer::new(&[format!("printf '%s\\n' '{concern}'")]);
    let outcome = supervise_reviewed_owned_with(
        &db,
        &repo,
        &backend,
        &reviewer,
        &racing_e2e_options(&ran, 1, true),
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, dagq::domain::AskKind::ApproveLanding);
    assert!(queue.run_leases().unwrap().is_empty());
    queue.answer(asks[0].id, "land").unwrap();

    hands_over_with_no_e2e(
        &db,
        &repo,
        &backend,
        &reviewer,
        racing_e2e_options(&ran, 1, false),
        leased_for_e2e,
        &ran,
    );
}
