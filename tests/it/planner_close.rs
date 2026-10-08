//! A person's planner opened before `dagq plan` was abolished whose row is
//! still open (ADR-t1433-2 decision 5, amending ADR-t1394-1 decisions 1, 7
//! and 9): the supervisor's pass closes its row without cmux and records
//! `planner_closed` once (`person_retired`, `workspace_closed: false`). Its
//! workspace is neither looked up, listed, typed into nor closed; a person
//! closes it in their own terminal.

use crate::common;
use crate::plan_review::{
    PlanWorkspace, StubReviewer, idle_person_planner, open_goal, planner_prompt,
};
use crate::runtime_support::*;

use dagq::domain::{AskKind, DraftOrigin, NewAsk, PlannerId, PlannerOrigin};
use serde_json::{Value, json};

/// Supervisor options for one pass with no sweep in it, so only the pass
/// closes a planner.
fn options() -> dagq::runtime::SuperviseOptions {
    dagq::runtime::SuperviseOptions {
        sweep_interval: std::time::Duration::from_secs(3600),
        ..supervise_options(4, true)
    }
}

#[test]
fn the_supervisor_closes_every_open_row_of_a_persons_planner_without_cmux() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let me = std::process::id();
    let record = |workspace: Option<&str>| {
        let planner = queue.open_planner(PlannerOrigin::Person, None).unwrap();
        if let Some(workspace) = workspace {
            queue
                .planner_workspace_created(planner.id, workspace)
                .unwrap();
            queue.register_planner_wrapper(planner.id, me).unwrap();
            queue.register_planner_agent(planner.id, me, me).unwrap();
        }
        planner.id
    };
    // Alive and at work, alive and idle, exited, and one that never got a
    // workspace.
    let working = record(Some("W-WORKING"));
    let idle = record(Some("W-IDLE"));
    let exited = record(Some("W-EXITED"));
    let unopened = record(None);
    queue.planner_exited(exited, me, 0).unwrap();
    let idle_dir = dagq::infrastructure::location::planners_dir(&db).join(idle.to_string());
    std::fs::create_dir_all(&idle_dir).unwrap();
    std::fs::write(
        dagq::application::planner_idle_marker(&idle_dir),
        r#"{"hook_event_name":"Stop","background_tasks":[]}"#,
    )
    .unwrap();
    let open = || -> Vec<PlannerId> {
        queue
            .planners(false)
            .unwrap()
            .into_iter()
            .map(|planner| planner.id)
            .collect()
    };
    let closes = || -> Vec<Value> {
        let mut closes: Vec<Value> = queue
            .latest_events_of("planner_closed", 10)
            .unwrap()
            .into_iter()
            .map(|event| event.payload)
            .collect();
        closes.sort_by_key(|close| close["planner_id"].as_i64());
        closes
    };
    // cmux would list every workspace as open.
    let backend = TestWorkspace::new(&db, false, "exit 0");
    for workspace in ["W-WORKING", "W-IDLE", "W-EXITED"] {
        backend.list(workspace);
    }

    supervise_with(&db, &repo, &backend, &options()).unwrap();
    assert!(open().is_empty(), "{:?}", open());
    let recorded = closes();
    assert_eq!(recorded.len(), 4, "{recorded:?}");
    for (close, (id, workspace)) in recorded.iter().zip([
        (working, Some("W-WORKING")),
        (idle, Some("W-IDLE")),
        (exited, Some("W-EXITED")),
        (unopened, None),
    ]) {
        assert_eq!(close["planner_id"], id.as_i64());
        assert_eq!(close["origin"], "person");
        assert_eq!(close["code"], "person_retired");
        assert_eq!(close["workspace_id"], serde_json::json!(workspace));
        assert_eq!(close["workspace_closed"], false);
    }
    assert_eq!(recorded[2]["exit_code"], 0);
    // No cmux for them: nothing closed, listed, looked up, read or typed.
    assert!(backend.closed().is_empty(), "{:?}", backend.closed());
    let asked = backend.asked.lock().unwrap().clone();
    assert!(
        !asked.iter().any(|session| session.starts_with("W-")),
        "{asked:?}"
    );

    // Once closed it is not closed again.
    supervise_with(&db, &repo, &backend, &options()).unwrap();
    assert_eq!(closes().len(), 4);
    assert!(backend.closed().is_empty());

    // `events --kind` reads them.
    let read = common::cli::ok(&db, &["events", "--full", "--kind", "planner_closed"]);
    let events = read["events"].as_array().unwrap();
    assert_eq!(events.len(), 4, "{read}");
    assert!(
        events
            .iter()
            .all(|event| event["payload"]["code"] == "person_retired"),
        "{read}"
    );
}

/// ADR-t1433-2 decision 5: the answer of a `planner_question` a person's
/// planner opened before `dagq plan` was abolished asked about a draft is
/// not typed into its workspace: its row is closed without cmux and a new
/// planner of the runtime's carries the answer, as for a draft whose
/// planner is gone (ADR-t1394-1 decision 7).
#[test]
fn a_planner_question_answer_of_a_persons_planner_is_carried_by_a_new_planner() {
    let fx = crate::plan_review::fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    idle_person_planner(&queue, &fx.db, "PW");
    let draft = common::queue::runtime_draft(
        &mut queue,
        "draft",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": null, "index": 0}),
    );
    let asked = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: Some(draft),
            run_id: None,
            question: "split it?".into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(asked.id, "adopt").unwrap();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    crate::plan_review::supervise(&fx, &backend, &reviewer);
    assert!(backend.closed().is_empty(), "{:?}", backend.closed());
    let closes = queue.latest_events_of("planner_closed", 10).unwrap();
    assert_eq!(closes.len(), 1, "{closes:?}");
    assert_eq!(closes[0].payload["code"], "person_retired");
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    assert_eq!(planners[0].origin, PlannerOrigin::Runtime);
    assert_eq!(planners[0].draft_task_id, Some(draft));
    assert_eq!(backend.launched().len(), 1);
    let prompt = planner_prompt(&fx.db, planners[0].id);
    assert!(
        prompt.contains(&format!("answer to ask {}: adopt", asked.id))
            && prompt.contains("split it?"),
        "{prompt}"
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());
}
