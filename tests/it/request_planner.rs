//! Planning requests (ADR-t1394-1) through the CLI and the supervisor loop:
//! the inbox records a person's words, the supervisor opens one planner of
//! the runtime's for each `open` request within the limit it shares with
//! the other planners of the runtime's, and the planner submits a proposal
//! of it (the request becomes `proposed`), declines it, or asks a
//! `planner_question` whose answer reaches it (as its next turn) or a new
//! planner. The backend (the planners' wrappers parked in the background)
//! and the stub reviewer are plan review's.

use crate::common::cli::{invoke_as, invoke_with, ok_as};
use crate::plan_review::{
    PlanWorkspace, StubReviewer, fixture, open_goal, options, planner_prompt, supervise,
    supervise_with,
};
use crate::runtime_support::planner_turns::{
    block_next_request, exit_requested, idle, turn_requests,
};
use dagq::{
    application::{PlanRequestStore, RequestPlannerStart, TaskStore},
    domain::{
        AskKind, AskReason, EventId, FindingTarget, NewAsk, NewFinding, NewTask, PlannerId,
        PlannerOrigin, PlannerOwner, Priority, RequestId, Submission, TaskId,
        plan_request::{NewPlanRequest, RequestRef, RequestStatus},
    },
    infrastructure::{location::planners_dir, sqlite::SqliteQueue},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

/// The payloads of the queue's events of `kind` about request `id`, with
/// the role that wrote each.
fn request_events(db: &Path, id: RequestId, kind: &str) -> Vec<(Value, Option<String>)> {
    let connection = Connection::open(db).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT payload, actor_role FROM run_events WHERE kind=?1
             AND json_extract(payload,'$.request_id')=?2 ORDER BY id",
        )
        .unwrap();
    statement
        .query_map(rusqlite::params![kind, id.as_i64()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .unwrap()
        .map(|row| {
            let (payload, role) = row.unwrap();
            (serde_json::from_str(&payload).unwrap(), role)
        })
        .collect()
}

/// The attention events `watch` and `events` show the inbox, by kind.
fn attention(db: &Path, kind: &str) -> Vec<Value> {
    dagq::compose::events(db, EventId::new(0), 500, false).unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == kind)
        .cloned()
        .collect()
}

fn record(queue: &mut SqliteQueue, text: &str) -> RequestId {
    queue
        .record_plan_request(
            &NewPlanRequest {
                text: text.into(),
                note: None,
                refs: Vec::new(),
                priority: None,
            },
            "inbox",
            "inbox",
        )
        .unwrap()
        .id
}

fn request_planner(queue: &SqliteQueue, request: RequestId) -> Option<PlannerId> {
    queue
        .planners(false)
        .unwrap()
        .into_iter()
        .find(|planner| planner.request_id == Some(request))
        .map(|planner| planner.id)
}

/// A draft task of `goal` that waits for the fixture's blocker.
fn draft(queue: &mut SqliteQueue, goal: dagq::domain::GoalId, title: &str) -> TaskId {
    queue
        .add(NewTask {
            change: None,
            title: title.into(),
            description: format!("{title}: do it"),
            acceptance: format!("{title} is done"),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: None,
            dependencies: vec![TaskId::new(1)],
            goal_dependencies: Vec::new(),
            goal_id: Some(goal),
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Interactive),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap()
        .id()
}

fn submit_from(queue: &mut SqliteQueue, workspace: &str, task: TaskId) -> dagq::domain::Proposal {
    queue
        .submit_linking(
            Submission {
                tasks: vec![task],
                goals: Vec::new(),
                proposal: None,
                owner: PlannerOwner {
                    origin: PlannerOrigin::Runtime,
                    workspace_id: Some(workspace.into()),
                },
            },
            &[],
        )
        .unwrap()
}

/// End `planner` as a session that exited without deciding anything.
fn exit(queue: &SqliteQueue, planner: PlannerId) {
    queue.register_planner_wrapper(planner, 1).unwrap();
    queue.register_planner_agent(planner, 1, 1).unwrap();
    queue.planner_exited(planner, 1, 0).unwrap();
}

fn planner_question(queue: &mut SqliteQueue, request: RequestId) -> dagq::domain::Ask {
    queue
        .ask(NewAsk {
            recommendation: Some("plan".into()),
            confidence: Some(dagq::domain::AskConfidence::Low),
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: None,
            run_id: None,
            question: "a new goal for this?".into(),
            options: vec!["plan".into(), "decline".into()],
            asked_by: "planner".into(),
            reason_category: AskReason::Scope,
            finding_id: None,
            request_id: Some(request),
        })
        .unwrap()
        .ask
}

#[test]
fn a_request_the_inbox_records_gets_one_planner_whose_submission_proposes_it() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let words = "The landing rate fell; plan what brings it back.\nKeep `dagq.toml` as it is.";
    let words_file = fx.db.parent().unwrap().join("words.txt");
    std::fs::write(&words_file, words).unwrap();
    let recorded = ok_as(
        "inbox",
        &fx.db,
        &[
            "request",
            "add",
            "--text-file",
            words_file.to_str().unwrap(),
            "--note",
            "the person asked in the inbox at 10:02",
            "--ref",
            "task:1",
            "--ref",
            &format!("goal:{goal}"),
            "--priority",
            "interrupt",
        ],
    );
    let id = RequestId::new(recorded["id"].as_i64().unwrap());
    assert_eq!(recorded["status"], "open");
    assert_eq!(recorded["priority"], "interrupt");
    assert_eq!(recorded["text"], words);
    assert_eq!(recorded["requested_by"], "inbox");
    assert_eq!(
        recorded["refs"],
        json!([{"kind": "task", "id": 1}, {"kind": "goal", "id": goal.as_i64()}])
    );
    let events = request_events(&fx.db, id, "request_recorded");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1.as_deref(), Some("inbox"));
    assert_eq!(events[0].0["priority"], "interrupt");
    let listed = ok_as("inbox", &fx.db, &["requests"]);
    assert_eq!(listed["requests"].as_array().unwrap().len(), 1);

    // The next pass opens one planner of the runtime's for it.
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    assert_eq!(planners[0].origin, PlannerOrigin::Runtime);
    assert_eq!(planners[0].request_id, Some(id));
    let handle = planners[0].workspace_id.clone().unwrap();
    assert_eq!(backend.launched(), std::slice::from_ref(&handle));
    // The words are handed over as a file in the planner's directory, and
    // the prompt points at it rather than carrying them.
    let handed = planners_dir(&fx.db)
        .join(planners[0].id.to_string())
        .join("requests")
        .join(format!("request-{id}.md"));
    assert_eq!(std::fs::read_to_string(&handed).unwrap(), words);
    let prompt = planner_prompt(&fx.db, planners[0].id);
    crate::runtime_support::planner_prompt_bytes::assert_planner_prompt_bytes(
        &fx.db,
        planners[0].id,
        "request",
        dagq::application::prompt::REQUEST_PLANNER_PROMPT_LIMIT,
    );
    for expected in [
        format!("planning request {id} of the queue"),
        format!(
            "dagq: a request for you is in the file {}",
            planners_dir(&fx.db)
                .canonicalize()
                .unwrap()
                .join(planners[0].id.to_string())
                .join("requests")
                .join(format!(
                    "request-{id}.md: read it and work on it as it says."
                ))
                .display()
        ),
        "the person asked in the inbox at 10:02".to_owned(),
        "Task 1 (draft): blocker".to_owned(),
        format!("Goal {goal}: "),
        "dagq search".to_owned(),
        format!("from request {id}"),
        format!("dagq request decline {id} --reason"),
        format!("dagq ask --request {id} --kind planner_question"),
        "`dagq events --full --task ID`".to_owned(),
        "AGENTS.md".to_owned(),
        "The person gave it the priority `interrupt`.".to_owned(),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    assert!(!prompt.contains("Keep `dagq.toml`"), "{prompt}");
    let opened_events = request_events(&fx.db, id, "request_planner_opened");
    assert_eq!(opened_events.len(), 1);
    assert_eq!(opened_events[0].0["attempt"], 1);

    // Never two for one request: the next pass, a higher limit and a
    // direct attempt all leave the one planner.
    supervise_with(
        &fx,
        &backend,
        &reviewer,
        &options(3, Duration::from_secs(3600)),
    );
    assert_eq!(queue.planners(false).unwrap().len(), 1);
    assert!(matches!(
        queue.open_request_planner(id, None).unwrap(),
        RequestPlannerStart::Skipped
    ));

    // The planner submits as its own: the request is proposed with the
    // proposal, and the inbox is told.
    let task = draft(&mut queue, goal, "bring it back");
    let proposal = submit_from(&mut queue, &handle, task);
    let proposed = queue.plan_request(id).unwrap();
    assert_eq!(proposed.status, RequestStatus::Proposed);
    assert_eq!(proposed.proposals, vec![proposal.id()]);
    let told = attention(&fx.db, "request_proposed");
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["next"], "report the request's proposal");
    // The goal its planner adds takes the priority the person gave the
    // request as the person's (another value is refused), and the goal and
    // its task record the request as their origin (ADR-t1975-1 decisions 2
    // and 5).
    let mut as_planner =
        SqliteQueue::open(&fx.db)
            .unwrap()
            .with_actor(dagq::domain::actor::ActorContext::instance(
                dagq::domain::actor::ActorRole::Planner,
                planners[0].id,
            ));
    let new_goal = |priority| dagq::domain::NewGoal {
        title: "bring the rate back".into(),
        priority,
        ..Default::default()
    };
    let refused = as_planner
        .add_goal(new_goal(Some(Priority::Normal)))
        .unwrap_err()
        .to_string();
    assert!(refused.contains("attaches interrupt"), "{refused}");
    let planned = as_planner.add_goal(new_goal(None)).unwrap().id();
    let more = draft(&mut as_planner, planned, "and more");
    let origin = json!({"origin": "human", "origin_kind": "request",
        "origin_request_id": id.as_i64()});
    let goal_show = ok_as("inbox", &fx.db, &["goal", "show", &planned.to_string()]);
    let task_show = ok_as("inbox", &fx.db, &["show", &more.to_string()]);
    for (shown, priority_source) in [
        (&goal_show["goal"], None),
        (&task_show["task"], Some("goal")),
    ] {
        assert_eq!(shown["priority"], "interrupt", "{shown}");
        assert_eq!(shown["priority_by"], "human", "{shown}");
        assert_eq!(
            shown.get("priority_source").and_then(Value::as_str),
            priority_source
        );
        for key in ["origin", "origin_kind", "origin_request_id"] {
            assert_eq!(shown[key], origin[key], "{key} {shown}");
        }
    }
    assert_eq!(goal_show["tasks"][0]["priority_by"], "human");
    let listed = ok_as("inbox", &fx.db, &["list", "--goal", &planned.to_string()]);
    assert_eq!(listed["tasks"][0]["priority_by"], "human", "{listed}");
    // A second proposal of it is linked too, and tells nobody again.
    let second = submit_from(&mut queue, &handle, more);
    assert_eq!(
        queue.plan_request(id).unwrap().proposals,
        vec![proposal.id(), second.id()]
    );
    assert_eq!(attention(&fx.db, "request_proposed").len(), 1);
    // Withdrawn and submitted again by a planner without the request, its
    // task stays the request's (ADR-t1971-1 decision 2).
    queue.withdraw_proposal(second.id()).unwrap();
    let again = submit_from(&mut queue, "another planner", more);
    assert_eq!(
        queue.plan_request(id).unwrap().proposals,
        vec![proposal.id(), second.id(), again.id()]
    );
    // Its origin and the person's priority stay as they were recorded.
    let resubmitted = queue.show(more).unwrap().task;
    assert_eq!(serde_json::to_value(resubmitted.origin()).unwrap(), origin);
    assert_eq!(
        (resubmitted.priority(), resubmitted.priority_by()),
        (
            Priority::Interrupt,
            dagq::domain::plan_request::PriorityBy::Human
        )
    );
    assert_eq!(attention(&fx.db, "request_proposed").len(), 1);
    // A proposed request waits for no planner, and is listed with --all.
    assert!(queue.planner_requests().unwrap().is_empty());
    assert!(
        ok_as("inbox", &fx.db, &["requests"])["requests"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let one = ok_as("planner", &fx.db, &["requests", &id.to_string()]);
    assert_eq!(one["requests"][0]["status"], "proposed");

    // Done and idle, the planner is asked to exit.
    idle(&queue, &fx.db, planners[0].id);
    supervise(&fx, &backend, &reviewer);
    assert!(exit_requested(&fx.db, planners[0].id));
}

#[test]
fn only_the_requests_own_planner_declines_it_and_the_inbox_is_told() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let id = ok_as("inbox", &fx.db, &["request", "add", "--text", "add a tab"])["id"]
        .as_i64()
        .unwrap();
    let id_text = id.to_string();
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let planner = request_planner(&queue, RequestId::new(id)).unwrap();

    // Another planner may not decline it: the request's planner is read
    // from the queue. Which roles may decline at all is
    // domain::authorization's unit test
    // only_the_user_and_the_inbox_record_a_request_and_only_its_planner_declines_it.
    let decline = ["request", "decline", &id_text, "--reason", "done already"];
    let other = invoke_with(
        &[("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", "planner:99")],
        &fx.db,
        &decline,
    );
    assert!(!other.status.success());
    let error: Value = serde_json::from_slice(&other.stderr).unwrap();
    assert_eq!(error["denied"]["capability"], "request.decline");
    assert_eq!(error["denied"]["reason"], "not on this resource");
    let denied = Connection::open(&fx.db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM run_events WHERE kind='authorization_denied'
             AND json_extract(payload,'$.capability')='request.decline'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(denied, 1);
    assert_eq!(
        queue.plan_request(RequestId::new(id)).unwrap().status,
        RequestStatus::Open
    );

    // A decline authorized for a planner that is no longer the open one is
    // refused in the decline's own transaction.
    let stale = queue
        .decline_request(
            RequestId::new(id),
            "done already",
            "planner",
            Some(PlannerId::new(planner.as_i64() + 100)),
        )
        .unwrap_err();
    assert!(stale.to_string().contains("its planner changed"), "{stale}");
    // Its own planner declines it with the reason.
    let own = format!("planner:{planner}");
    let declined = invoke_with(
        &[("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", &own)],
        &fx.db,
        &decline,
    );
    assert!(
        declined.status.success(),
        "{}",
        String::from_utf8_lossy(&declined.stderr)
    );
    let declined: Value = serde_json::from_slice(&declined.stdout).unwrap();
    assert_eq!(declined["status"], "declined");
    assert_eq!(declined["status_reason"], "done already");
    let told = attention(&fx.db, "request_declined");
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["next"], "rephrase or drop the request");
    assert_eq!(told[0]["reason"], "done already");
    let events = request_events(&fx.db, RequestId::new(id), "request_declined");
    assert_eq!(events[0].0["planner_id"], planner.as_i64());
    // No planner is opened for it again (that a declined request is not
    // declined twice is domain::plan_request's unit test
    // only_an_open_request_is_declined_and_with_a_reason).
    exit(&queue, planner);
    supervise(&fx, &backend, &reviewer);
    supervise(&fx, &backend, &reviewer);
    assert_eq!(
        request_events(&fx.db, RequestId::new(id), "request_planner_opened").len(),
        1
    );
}

/// Task 1564: `ask --request N` opens only for request N's open runtime
/// planner, read from the queue; another planner (of another request, a
/// draft or a finding) is refused and the refusal is recorded, and the
/// request keeps no ask. Which roles may ask at all is
/// domain::authorization's unit test only_a_requests_own_planner_asks_on_it.
#[test]
fn only_the_requests_own_planner_asks_a_planner_question_on_it() {
    let fx = fixture();
    let queue = SqliteQueue::open(&fx.db).unwrap();
    let id = ok_as("inbox", &fx.db, &["request", "add", "--text", "add a tab"])["id"]
        .as_i64()
        .unwrap();
    let id_text = id.to_string();
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let planner = request_planner(&queue, RequestId::new(id)).unwrap();
    let ask = [
        "ask",
        "--kind",
        "planner_question",
        "--request",
        &id_text,
        "--because",
        "scope",
        "--question",
        "a new goal for this?",
        "--option",
        "plan",
        "--option",
        "decline",
    ];
    let asks_on_request = || {
        Connection::open(&fx.db)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM asks WHERE request_id = ?1",
                [id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
    };

    let other = invoke_with(
        &[("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", "planner:99")],
        &fx.db,
        &ask,
    );
    assert!(!other.status.success());
    let error: Value = serde_json::from_slice(&other.stderr).unwrap();
    assert_eq!(error["denied"]["capability"], "ask.open");
    assert_eq!(error["denied"]["reason"], "not on this resource");
    let denied: Value = Connection::open(&fx.db)
        .unwrap()
        .query_row(
            "SELECT payload FROM run_events WHERE kind='authorization_denied'
             AND json_extract(payload,'$.capability')='ask.open'",
            [],
            |r| r.get::<_, String>(0),
        )
        .map(|payload| serde_json::from_str(&payload).unwrap())
        .unwrap();
    assert_eq!(denied["resource"]["request"], id);
    assert_eq!(denied["resource"]["planner"], planner.as_i64());
    assert_eq!(asks_on_request(), 0);

    // Its own planner asks, and the ask names the request.
    let own = format!("planner:{planner}");
    let opened = invoke_with(
        &[("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", &own)],
        &fx.db,
        &ask,
    );
    assert!(
        opened.status.success(),
        "{}",
        String::from_utf8_lossy(&opened.stderr)
    );
    assert_eq!(asks_on_request(), 1);
    // A planner's question on no request is not tied to one.
    let unbound = invoke_with(
        &[("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", "planner:99")],
        &fx.db,
        &[
            "ask",
            "--kind",
            "planner_question",
            "--task",
            "1",
            "--because",
            "scope",
            "--question",
            "which goal?",
        ],
    );
    assert!(
        unbound.status.success(),
        "{}",
        String::from_utf8_lossy(&unbound.stderr)
    );
}

#[test]
fn a_planner_question_about_a_request_reaches_its_planner_or_a_new_one_and_three_exhaust_it() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let id = record(&mut queue, "split the store");
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let first = request_planner(&queue, id).unwrap();

    // It asks about the request, on no task, and is answered while it is
    // still there (before a pass ended it for the wait, ADR-t1704-1
    // decision 2).
    let asked = planner_question(&mut queue, id);
    assert_eq!(asked.request_id, Some(id));
    idle(&queue, &fx.db, first);
    queue.answer(asked.id, "plan").unwrap();
    supervise(&fx, &backend, &reviewer);
    assert!(!exit_requested(&fx.db, first));
    let requests = turn_requests(&fx.db, first);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0]["prompt"],
        format!("answer to ask {}: plan", asked.id)
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());

    // A question about the request names nothing else.
    let refused = queue
        .ask(NewAsk {
            task_id: Some(TaskId::new(1)),
            ..planner_question_of(id)
        })
        .unwrap_err();
    assert!(
        refused.to_string().contains("planning request"),
        "{refused}"
    );

    // It asks again and its session ends before the answer: the request
    // waits for the answer, and a new planner carries it.
    let again = planner_question(&mut queue, id);
    queue.planner_exited(first, std::process::id(), 0).unwrap();
    supervise(&fx, &backend, &reviewer);
    supervise(&fx, &backend, &reviewer);
    assert_eq!(request_planner(&queue, id), None);
    assert_eq!(
        request_events(&fx.db, id, "request_planner_opened").len(),
        1
    );
    queue.answer(again.id, "decline").unwrap();
    supervise(&fx, &backend, &reviewer);
    let second = request_planner(&queue, id).expect("a planner carries the answer");
    let prompt = planner_prompt(&fx.db, second);
    assert!(
        prompt.contains(&format!("answer to ask {}: decline", again.id)),
        "{prompt}"
    );
    assert!(prompt.contains("planner 2 of at most 3"), "{prompt}");
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let opened = request_events(&fx.db, id, "request_planner_opened");
    assert_eq!(opened[1].0["ask_id"], again.id.as_i64());

    // The planners end without deciding it: a third is opened, and then
    // the request is exhausted and the inbox is told.
    exit(&queue, second);
    supervise(&fx, &backend, &reviewer);
    supervise(&fx, &backend, &reviewer);
    let third = request_planner(&queue, id).expect("a third planner");
    exit(&queue, third);
    supervise(&fx, &backend, &reviewer);
    supervise(&fx, &backend, &reviewer);
    assert_eq!(request_planner(&queue, id), None);
    assert_eq!(
        request_events(&fx.db, id, "request_planner_opened").len(),
        3
    );
    let exhausted = queue.plan_request(id).unwrap();
    assert_eq!(exhausted.status, RequestStatus::Exhausted);
    assert_eq!(exhausted.planners, 3);
    let told = attention(&fx.db, "request_planner_exhausted");
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["next"], "rephrase or drop the request");

    // An answer to a question about a request that moved on is closed.
    let late = queue
        .ask(NewAsk {
            asked_by: "inbox".into(),
            ..planner_question_of(id)
        })
        .unwrap()
        .ask;
    queue.answer(late.id, "plan").unwrap();
    supervise(&fx, &backend, &reviewer);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert_eq!(
        request_events(&fx.db, id, "request_planner_opened").len(),
        3
    );
}

/// An answer whose sending to the request's planner fails is recorded
/// once as `ask_delivery_failed` and handed over: the planner is asked to
/// exit and not sent it again, and once its session ended a new planner of
/// the request carries the answer; the inbox has nothing to close.
#[test]
fn an_answer_that_could_not_be_sent_to_a_requests_planner_goes_to_a_new_one() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let id = record(&mut queue, "split the store");
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let first = request_planner(&queue, id).unwrap();
    let asked = planner_question(&mut queue, id);
    idle(&queue, &fx.db, first);
    queue.answer(asked.id, "plan").unwrap();
    block_next_request(&fx.db, first, 0);
    supervise(&fx, &backend, &reviewer);
    let failed = request_events(&fx.db, id, "ask_delivery_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].0["planner_id"], json!(first));
    assert!(turn_requests(&fx.db, first).is_empty());
    assert!(exit_requested(&fx.db, first));
    supervise(&fx, &backend, &reviewer);
    assert_eq!(request_events(&fx.db, id, "ask_delivery_failed").len(), 1);
    assert!(turn_requests(&fx.db, first).is_empty());

    queue.planner_exited(first, std::process::id(), 0).unwrap();
    supervise(&fx, &backend, &reviewer);
    supervise(&fx, &backend, &reviewer);
    let second = request_planner(&queue, id).expect("a planner carries the answer");
    assert_ne!(second, first);
    let prompt = planner_prompt(&fx.db, second);
    assert!(
        prompt.contains(&format!("answer to ask {}: plan", asked.id)),
        "{prompt}"
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let opened = request_events(&fx.db, id, "request_planner_opened");
    assert_eq!(opened.len(), 2, "{opened:?}");
    assert_eq!(opened[1].0["ask_id"], asked.id.as_i64());
}

/// ADR-t2015-1: a question about a draft the request's planner added and
/// left outside a proposal goes the request's way: typed into its live
/// planner, else carried by a new planner of the request even after the
/// request was proposed; the runtime delivers it, so the inbox is not told.
#[test]
fn an_answer_about_a_draft_a_requests_planner_left_reaches_it_or_a_new_one_after_the_proposal() {
    use dagq::domain::actor::{ActorContext, ActorRole};
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let id = record(&mut queue, "split the store");
    // Plan review passes the proposal it submits.
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let first = request_planner(&queue, id).unwrap();
    let mut as_planner = SqliteQueue::open(&fx.db)
        .unwrap()
        .with_actor(ActorContext::instance(ActorRole::Planner, first));
    let left = draft(&mut as_planner, goal, "left");
    let submitted = draft(&mut as_planner, goal, "submitted");
    let about_left = |queue: &mut SqliteQueue| {
        queue
            .ask(NewAsk {
                task_id: Some(left),
                request_id: None,
                options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
                ..planner_question_of(id)
            })
            .unwrap()
            .ask
    };

    // Its planner is open: the answer is its next turn.
    let asked = about_left(&mut as_planner);
    idle(&queue, &fx.db, first);
    queue.answer(asked.id, "adopt").unwrap();
    supervise(&fx, &backend, &reviewer);
    let requests = turn_requests(&fx.db, first);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0]["prompt"],
        format!("answer to ask {}: adopt", asked.id)
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());

    // It submits another draft (the request is proposed), asks again
    // about the one it left and ends before the answer.
    let workspace = queue.planner(first).unwrap().workspace_id.unwrap();
    submit_from(&mut queue, &workspace, submitted);
    assert_eq!(
        queue.plan_request(id).unwrap().status,
        RequestStatus::Proposed
    );
    let again = about_left(&mut as_planner);
    queue.planner_exited(first, std::process::id(), 0).unwrap();
    supervise(&fx, &backend, &reviewer);
    supervise(&fx, &backend, &reviewer);
    assert_eq!(request_planner(&queue, id), None);
    queue.answer(again.id, "cancel").unwrap();
    let answered = queue
        .show(left)
        .unwrap()
        .events
        .into_iter()
        .rev()
        .find(|event| event.kind == "ask_answered")
        .unwrap();
    assert_eq!(answered.payload["runtime_delivers"], true);
    supervise(&fx, &backend, &reviewer);
    let second = request_planner(&queue, id).expect("a planner carries the answer");
    let prompt = planner_prompt(&fx.db, second);
    assert!(
        prompt.contains(&format!("answer to ask {}: cancel", again.id)),
        "{prompt}"
    );
    assert!(prompt.contains(&format!("draft task {left}")), "{prompt}");
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let opened = request_events(&fx.db, id, "request_planner_opened");
    assert_eq!(opened.len(), 2);
    assert_eq!(opened[1].0["ask_id"], again.id.as_i64());
    assert_eq!(
        queue.plan_request(id).unwrap().status,
        RequestStatus::Proposed
    );
}

fn planner_question_of(request: RequestId) -> NewAsk {
    NewAsk {
        recommendation: None,
        confidence: None,
        topics: Vec::new(),
        kind: AskKind::PlannerQuestion,
        task_id: None,
        run_id: None,
        question: "still?".into(),
        options: vec!["plan".into(), "decline".into()],
        asked_by: "planner".into(),
        reason_category: AskReason::Scope,
        finding_id: None,
        request_id: Some(request),
    }
}

#[test]
fn requests_share_the_limit_of_the_runtimes_planners_and_come_first() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let finding = queue
        .record_finding(NewFinding {
            kind: "conflict_hotspot".into(),
            target: FindingTarget::Queue,
            subject: "src/main.rs".into(),
            summary: "it conflicts".into(),
            detail: None,
            impact: None,
            evidence: Vec::new(),
            propose: Some("again and again".into()),
            by: "observer".into(),
        })
        .unwrap()
        .finding
        .id;
    let first = record(&mut queue, "first");
    let second = record(&mut queue, "second");
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::default();
    // One place: the oldest request takes it.
    supervise(&fx, &backend, &reviewer);
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    assert_eq!(planners[0].request_id, Some(first));
    // Three places: the other request and then the finding.
    supervise_with(
        &fx,
        &backend,
        &reviewer,
        &options(3, Duration::from_secs(3600)),
    );
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 3, "{planners:?}");
    assert_eq!(planners[1].request_id, Some(second));
    assert_eq!(planners[2].finding_id, Some(finding));
}

#[test]
fn only_the_inbox_and_a_person_record_a_request() {
    let fx = fixture();
    // A role other than the inbox and a person is refused and the refusal
    // recorded with its role; which roles are refused is
    // domain::authorization's unit test
    // only_the_user_and_the_inbox_record_a_request_and_only_its_planner_declines_it.
    let add = ["request", "add", "--text", "plan it", "--ref", "task:1"];
    let output = invoke_as(Some("planner"), &fx.db, &add);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error.to_string().contains("request.record"), "{error}");
    let denied: Vec<String> = Connection::open(&fx.db)
        .unwrap()
        .prepare(
            "SELECT actor_role FROM run_events WHERE kind='authorization_denied'
             AND json_extract(payload,'$.capability')='request.record' ORDER BY id",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(denied, ["planner"]);
    let queue = SqliteQueue::open(&fx.db).unwrap();
    assert!(queue.plan_requests(true).unwrap().is_empty());

    // The inbox and a person at a plain terminal record it, each as itself.
    let by_inbox = ok_as("inbox", &fx.db, &add);
    assert_eq!(by_inbox["requested_by"], "inbox");
    let by_person = invoke_as(None, &fx.db, &["request", "add", "--text", "mine"]);
    assert!(by_person.status.success());
    let by_person: Value = serde_json::from_slice(&by_person.stdout).unwrap();
    assert_eq!(by_person["requested_by"], "user");
    // No words are refused (blank words and an unknown reference are
    // domain::plan_request's unit tests a_request_needs_words_and_a_note_is_not_blank
    // and a_reference_reads_each_kind_and_refuses_the_rest).
    assert!(
        !invoke_as(Some("inbox"), &fx.db, &["request", "add"])
            .status
            .success()
    );
    let all = ok_as("observer", &fx.db, &["requests", "--all"]);
    assert_eq!(all["requests"].as_array().unwrap().len(), 2);
    assert_eq!(
        queue.plan_request(RequestId::new(1)).unwrap().refs,
        vec![RequestRef::Task(TaskId::new(1))]
    );
}

#[test]
fn questions_about_two_requests_stay_apart_and_a_closed_answer_opens_no_planner() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let first = record(&mut queue, "first");
    let second = record(&mut queue, "second");
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::default();
    supervise_with(
        &fx,
        &backend,
        &reviewer,
        &options(2, Duration::from_secs(3600)),
    );
    let planner = request_planner(&queue, first).unwrap();
    assert!(request_planner(&queue, second).is_some());

    // Each planner asks about its request at once: both questions are open,
    // neither taken for the other's.
    let asked = planner_question(&mut queue, first);
    let other = queue.ask(planner_question_of(second)).unwrap();
    assert!(other.created);
    assert_ne!(other.ask.id, asked.id);
    assert_eq!(other.ask.request_id, Some(second));
    let open: Vec<_> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .map(|ask| ask.request_id)
        .collect();
    assert_eq!(open, [Some(first), Some(second)]);
    // Asking again about the same request gives the open one back.
    let again = queue.ask(planner_question_of(first)).unwrap();
    assert!(!again.created);
    assert_eq!(again.ask.id, asked.id);

    // The first planner ends while its question waits: an unanswered
    // question opens no planner.
    exit(&queue, planner);
    supervise(&fx, &backend, &reviewer);
    assert_eq!(request_planner(&queue, first), None);
    assert!(matches!(
        queue.open_request_planner(first, Some(asked.id)).unwrap(),
        RequestPlannerStart::Skipped
    ));
    // Answered, then closed by the inbox before the supervisor carries it:
    // the closed answer opens no planner either.
    queue.answer(asked.id, "plan").unwrap();
    queue.close_ask(asked.id).unwrap();
    assert!(matches!(
        queue.open_request_planner(first, Some(asked.id)).unwrap(),
        RequestPlannerStart::Skipped
    ));
    assert_eq!(
        request_events(&fx.db, first, "request_planner_opened").len(),
        1
    );
    // An answer about the other request is no answer for this one.
    queue.answer(other.ask.id, "plan").unwrap();
    assert!(matches!(
        queue
            .open_request_planner(first, Some(other.ask.id))
            .unwrap(),
        RequestPlannerStart::Skipped
    ));
}

#[test]
fn an_ask_or_an_event_alone_leads_the_prompt_to_its_goal() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let task = draft(&mut queue, goal, "in the goal");
    let new_ask = |task: Option<TaskId>, finding: Option<dagq::domain::FindingId>| NewAsk {
        recommendation: None,
        confidence: None,
        topics: Vec::new(),
        kind: if finding.is_some() {
            AskKind::Blocked
        } else {
            AskKind::Decide
        },
        task_id: task,
        run_id: None,
        question: "what now?".into(),
        options: vec!["go".into()],
        asked_by: "inbox".into(),
        reason_category: AskReason::Scope,
        finding_id: finding,
        request_id: None,
    };
    // An ask on a task of the goal, and one raising a finding on the goal.
    let on_task = queue.ask(new_ask(Some(task), None)).unwrap().ask.id;
    let finding = queue
        .record_finding(NewFinding {
            kind: "wait".into(),
            target: FindingTarget::Goal(goal),
            subject: String::new(),
            summary: "the goal waits".into(),
            detail: None,
            impact: None,
            evidence: Vec::new(),
            propose: None,
            by: "observer".into(),
        })
        .unwrap()
        .finding
        .id;
    let on_finding = queue.ask(new_ask(None, Some(finding))).unwrap().ask.id;
    // An event of the goal itself, and one of its task.
    let connection = Connection::open(&fx.db).unwrap();
    let event_of = |sql: &str, id: i64| {
        EventId::new(
            connection
                .query_row(sql, [id], |r| r.get::<_, i64>(0))
                .unwrap(),
        )
    };
    let goal_event = event_of(
        "SELECT id FROM run_events WHERE goal_id=?1 AND task_id IS NULL ORDER BY id LIMIT 1",
        goal.as_i64(),
    );
    let task_event = event_of(
        "SELECT id FROM run_events WHERE task_id=?1 ORDER BY id LIMIT 1",
        task.as_i64(),
    );
    let mut requests = Vec::new();
    for reference in [
        RequestRef::Ask(on_task),
        RequestRef::Ask(on_finding),
        RequestRef::Event(goal_event),
        RequestRef::Event(task_event),
    ] {
        let id = queue
            .record_plan_request(
                &NewPlanRequest {
                    text: "plan it".into(),
                    note: None,
                    refs: vec![reference.clone()],
                    priority: None,
                },
                "inbox",
                "inbox",
            )
            .unwrap()
            .id;
        requests.push((id, reference));
    }
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::default();
    supervise_with(
        &fx,
        &backend,
        &reviewer,
        &options(4, Duration::from_secs(3600)),
    );
    for (id, reference) in requests {
        let planner = request_planner(&queue, id).unwrap_or_else(|| panic!("{reference}"));
        let prompt = planner_prompt(&fx.db, planner);
        for expected in [
            format!("## Goal {goal}: tidy the queue"),
            "every draft is decided".to_owned(),
            "no new tables".to_owned(),
            format!("task {task} (draft): in the goal"),
        ] {
            assert!(
                prompt.contains(&expected),
                "{reference}: {expected}\n{prompt}"
            );
        }
        assert!(
            !prompt.contains("Nothing it refers to belongs to a goal"),
            "{reference}\n{prompt}"
        );
    }
}
