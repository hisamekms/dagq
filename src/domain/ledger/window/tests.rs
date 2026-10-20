use crate::domain::EventKind;
use serde_json::{Value, json};

use super::*;
use crate::domain::ledger::{
    LEDGER_V, build,
    settle::{SETTLE_GRACE_MS, life_input, settlement},
};
use crate::domain::marks::utc_text;
use crate::domain::sessions::TOKEN_CUT_CLOSED_WINDOW_MS;
use crate::domain::stats::executions::executions;
use crate::domain::{RunId, event_kind};

const T0: i64 = 1_900_000_000_000;
const MIN: i64 = 60_000;
const RUN: &str = "11111111-1111-4111-8111-111111111111";
const BUILT: &str = "2030-01-01T00:00:00.000Z";

/// The events of a queue as a test lays them down, ascending id.
#[derive(Default)]
struct Log {
    events: Vec<RunEvent>,
}

impl Log {
    fn push(
        &mut self,
        minute: i64,
        ids: (Option<i64>, Option<i64>, bool),
        kind: &str,
        payload: Value,
    ) -> EventId {
        let (task, goal, run) = ids;
        let id = EventId::new(self.events.len() as i64 + 1);
        self.events.push(RunEvent {
            id,
            task_id: task.map(TaskId::new),
            goal_id: goal.map(GoalId::new),
            run_id: run.then(|| RunId::new(RUN).unwrap()),
            kind: kind.into(),
            payload,
            created_at: utc_text(T0 + minute * MIN),
            actor: None,
        });
        id
    }

    fn queue(&mut self, minute: i64, kind: &str, payload: Value) -> EventId {
        self.push(minute, (None, None, false), kind, payload)
    }
}

fn tokens(input: i64) -> Value {
    json!({"input": input, "output": 1, "cache_read": 0, "cache_creation": 0})
}

/// The Execution's end of a headless turn or job, with its context: a
/// measured peak, or none with why.
fn ended(mut payload: Value, peak: Option<i64>) -> Value {
    payload["tokens_source"] = json!("model_usage");
    payload["tokens"] = tokens(100);
    payload["tokens_by_model"] = json!([]);
    payload["model"] = json!("opus");
    payload["peak_context"] = json!(peak);
    payload["context_window"] = json!(200_000);
    payload["compactions"] = json!(peak.map(|_| 1));
    if peak.is_none() {
        payload["context_reason"] = json!("rollout_missing");
    }
    payload
}

/// A queue with one of each session and job of no run, a run with its
/// session and review, and the queue's intervals and events.
fn queue_log() -> Log {
    let mut log = Log::default();
    // The attributes' baseline mark, and task 7 of goal 3 with change
    // `runtime`.
    log.push(
        0,
        (Some(7), None, false),
        event_kind::TASK_CREATED,
        json!({"goal_id": 3, "status": "ready", "title": "t", "change": "runtime",
               "execution_class": "implementation", "priority": null, "priority_by": null}),
    );
    log.queue(
        0,
        event_kind::ATTRIBUTES_BASELINE,
        json!({"tasks": 0, "goals": 0}),
    );
    // The runtime planner's session and its turn.
    let planner = log.queue(
        1,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 5, "route": "headless",
               "session_id": "p", "launch": {"provider": "codex"}}),
    );
    log.queue(
        2,
        "turn_finished",
        ended(json!({"planner_id": 5, "provider": "codex"}), None),
    );
    log.queue(
        3,
        "session_closed",
        json!({"opened_event_id": planner, "session_id": "p", "reason": "exited"}),
    );
    // A plan review, a goal review of goal 3 (no task), a throughput review
    // and the observer.
    log.queue(
        4,
        "plan_review_started",
        json!({"plan_review_id": 11, "launch": {"provider": "claude"}}),
    );
    log.queue(
        6,
        "plan_review_finished",
        ended(
            json!({"plan_review_id": 11, "verdict": "pass"}),
            Some(50_000),
        ),
    );
    log.push(
        5,
        (None, Some(3), false),
        "goal_review_started",
        json!({"goal_review_id": 21}),
    );
    log.push(
        7,
        (None, Some(3), false),
        "goal_review_finished",
        ended(json!({"goal_review_id": 21, "verdict": "open"}), Some(10)),
    );
    let review = json!({"mode": "hourly", "period": "2030-01-01T00", "dir": "d"});
    log.queue(8, "throughput_review_started", review.clone());
    let mut finished = ended(review, Some(20));
    finished["outcome"] = json!("succeeded");
    log.queue(9, "throughput_review_finished", finished);
    log.queue(10, "observe_started", json!({}));
    log.queue(
        11,
        "observe_finished",
        ended(json!({"outcome": "succeeded"}), Some(30)),
    );
    // The inbox's session and two cuts, the second at its close; the
    // first cut's record has no context (before the context was recorded).
    let inbox = log.queue(
        12,
        "session_opened",
        json!({"kind": "inbox", "session_id": "i"}),
    );
    log.queue(
        13,
        "session_tokens",
        json!({"opened_event_id": inbox, "kind": "inbox", "at": utc_text(T0 + 13 * MIN),
               "tokens_source": "transcript", "tokens": tokens(7), "tokens_by_model": []}),
    );
    log.queue(
        14,
        "session_closed",
        json!({"opened_event_id": inbox, "session_id": "i", "reason": "exited"}),
    );
    log.queue(
        15,
        "session_tokens",
        json!({"opened_event_id": inbox, "kind": "inbox", "at": utc_text(T0 + 14 * MIN),
               "final": true, "tokens_source": "transcript", "tokens": tokens(3),
               "tokens_by_model": []}),
    );
    // The run of task 7: its worker session, a turn and its review.
    let run = (Some(7), None, true);
    log.push(16, run, "run_claimed", json!({}));
    let worker = log.push(
        16,
        run,
        "session_opened",
        json!({"kind": "worker", "route": "headless", "session_id": "w"}),
    );
    log.push(
        18,
        run,
        "turn_finished",
        ended(json!({"provider": "claude"}), Some(90_000)),
    );
    log.push(
        19,
        run,
        "session_closed",
        json!({"opened_event_id": worker, "session_id": "w", "reason": "exited"}),
    );
    log.push(
        20,
        run,
        "review_started",
        json!({"launch": {"provider": "claude"}}),
    );
    log.push(
        22,
        run,
        "review_finished",
        ended(json!({"verdict": "pass"}), Some(40_000)),
    );
    // The queue's intervals and events: slots full, a hold on the claims, a
    // deferral of task 8 and an update installed.
    let full = json!({"supervisor": "s", "class": "heavy", "running": 2, "slots": 2});
    log.queue(30, "slots_full_started", full.clone());
    log.queue(32, "slots_full_ended", full);
    log.queue(
        33,
        "claim_held",
        json!({"supervisor": "s", "reason": "load"}),
    );
    log.queue(35, "claim_resumed", json!({"supervisor": "s"}));
    log.push(
        36,
        (Some(8), None, false),
        "claim_deferred",
        json!({"supervisor": "s", "reason": "hot_files"}),
    );
    log.push(
        38,
        (Some(8), None, false),
        "claim_deferral_ended",
        json!({"supervisor": "s", "why": "expired"}),
    );
    log.queue(
        40,
        "update_installed",
        json!({"supervisor": "s", "build": "b"}),
    );
    log
}

fn subject(row: &LedgerRow) -> (&str, &str, Option<i64>) {
    let subject = row.subject.as_ref().unwrap();
    (&subject.kind, &subject.id, subject.round)
}

/// Every event is the records of exactly one row, so the rows give back
/// the events.
#[test]
fn every_event_is_the_records_of_one_row() {
    let log = queue_log();
    let rows = build(&log.events, T0 + 60 * MIN, BUILT);
    let mut owned: Vec<EventId> = rows
        .iter()
        .flat_map(|row| row.records.iter().map(|event| event.id))
        .collect();
    owned.sort();
    let all: Vec<EventId> = log.events.iter().map(|event| event.id).collect();
    assert_eq!(owned, all);
    let window = select(
        rows,
        &WindowQuery {
            now_ms: T0 + 60 * MIN,
            ..WindowQuery::default()
        },
    );
    assert_eq!(window.events(), log.events);
    // A node row is folded of the NodeSampleStore, not of the events.
    for kind in LedgerKind::ALL
        .into_iter()
        .filter(|kind| *kind != LedgerKind::Node)
    {
        assert!(window.of(kind).next().is_some(), "{kind:?}");
    }
}

/// The sessions and jobs of no run are session rows keyed by what they
/// are of and their round, with their time, route, provider, model,
/// result, what their kind records and their Executions' tokens and
/// context, a goal review's though it has no task.
#[test]
fn the_sessions_and_jobs_of_no_run_are_session_rows() {
    let log = queue_log();
    let rows = build(&log.events, T0 + 60 * MIN, BUILT);
    let sessions: Vec<&LedgerRow> = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Session)
        .collect();
    let keys: Vec<&str> = sessions.iter().map(|row| row.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "runtime_planner/5/session/1",
            "plan_review/11/job/1",
            "goal_review/21/job/1",
            "throughput_review/hourly/2030-01-01T00/d/job/1",
            "observer//job/1",
            "inbox//session/1",
        ]
    );
    let planner = sessions[0];
    assert_eq!(subject(planner), ("runtime_planner", "5", Some(1)));
    let of = planner.subject.as_ref().unwrap();
    assert_eq!(
        (of.route.as_deref(), of.provider.as_deref()),
        (Some("headless"), Some("codex"))
    );
    assert_eq!(
        (
            planner.started_at,
            planner.ended_at,
            planner.outcome.as_deref()
        ),
        (Some(T0 + MIN), Some(T0 + 3 * MIN), Some("closed"))
    );
    assert_eq!(planner.records.len(), 3);
    // The planner's turn: its tokens, and a context not measured with why.
    let turn = &planner.detail["executions"][0];
    assert_eq!(turn["actor"], "runtime_planner");
    assert_eq!(turn["tokens"]["input"], 100);
    assert_eq!(
        turn["context"],
        json!({"peak_context": null, "context_window": 200_000, "compactions": null,
               "context_reason": "rollout_missing"})
    );
    let plan_review = sessions[1];
    assert_eq!(plan_review.outcome.as_deref(), Some("pass"));
    assert_eq!(plan_review.detail["items"]["verdict"], "pass");
    assert_eq!(plan_review.detail["items"]["secs"], 120);
    assert_eq!(
        plan_review.subject.as_ref().unwrap().provider.as_deref(),
        Some("claude")
    );
    assert_eq!(
        plan_review.detail["executions"][0]["context"]["peak_context"],
        50_000
    );
    let goal_review = sessions[2];
    assert_eq!(goal_review.task_id, None);
    assert_eq!(goal_review.records[0].goal_id, Some(GoalId::new(3)));
    assert_eq!(goal_review.outcome.as_deref(), Some("open"));
    let throughput = sessions[3];
    assert_eq!(throughput.detail["items"]["mode"], "hourly");
    assert_eq!(throughput.outcome.as_deref(), Some("finished"));
    assert_eq!(sessions[4].detail["executions"][0]["actor"], "observer");
    // The inbox: one row, its two cuts its Executions; the first cut's
    // record has no context at all, not an unmeasured one.
    let inbox = sessions[5];
    assert_eq!(subject(inbox), ("inbox", "", Some(1)));
    assert_eq!(
        inbox.subject.as_ref().unwrap().provider.as_deref(),
        Some("claude")
    );
    let cuts = inbox.detail["executions"].as_array().unwrap();
    assert_eq!(cuts.len(), 2);
    assert_eq!(cuts[0]["route"], "interactive");
    assert_eq!(cuts[0]["context"], Value::Null);
    assert_eq!(cuts[1]["at"], T0 + 14 * MIN);
}

/// The run's row keeps the sessions and jobs on it and its Executions, its
/// records being its run's events.
#[test]
fn the_runs_sessions_jobs_and_executions_are_in_its_row() {
    let log = queue_log();
    let rows = build(&log.events, T0 + 60 * MIN, BUILT);
    let run = rows.iter().find(|row| row.kind == LedgerKind::Run).unwrap();
    assert_eq!(run.key, RUN);
    assert_eq!(run.ledger_v, LEDGER_V);
    assert_eq!(run.records.len(), 6);
    let sessions = run.detail["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        (
            &sessions[0]["kind"],
            &sessions[0]["route"],
            &sessions[0]["result"]
        ),
        (&json!("worker"), &json!("headless"), &json!("closed"))
    );
    let jobs = run.detail["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        (&jobs[0]["kind"], &jobs[0]["verdict"], &jobs[0]["secs"]),
        (&json!("review"), &json!("pass"), &json!(120))
    );
    let executions = run.detail["executions"].as_array().unwrap();
    let actors: Vec<&Value> = executions.iter().map(|e| &e["actor"]).collect();
    assert_eq!(actors, [&json!("worker"), &json!("review")]);
    assert_eq!(executions[0]["context"]["peak_context"], 90_000);
    // The task's row keeps its own event of no run.
    let task = rows
        .iter()
        .find(|row| row.kind == LedgerKind::Task && row.key == "7")
        .unwrap();
    assert_eq!(task.records.len(), 1);
    assert_eq!(task.records[0].kind, EventKind::TaskCreated);
}

/// The queue's intervals are rows from their start to their end with
/// both as records; an event is a row of its own.
#[test]
fn the_queues_intervals_and_events_are_queue_rows() {
    let log = queue_log();
    let rows = build(&log.events, T0 + 60 * MIN, BUILT);
    let queue: Vec<&LedgerRow> = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Queue)
        .collect();
    let of_kind = |kind: &str| {
        queue
            .iter()
            .find(|row| row.subject.as_ref().unwrap().kind == kind)
            .copied()
            .unwrap()
    };
    let full = of_kind("slots_full");
    assert_eq!(subject(full), ("slots_full", "s", None));
    assert_eq!(
        (full.started_at, full.ended_at, full.outcome.as_deref()),
        (Some(T0 + 30 * MIN), Some(T0 + 32 * MIN), Some("freed"))
    );
    assert_eq!(full.key, full.records[0].id.to_string());
    assert_eq!(full.records.len(), 2);
    let held = of_kind("claim_hold");
    assert_eq!(held.outcome.as_deref(), Some("resumed"));
    assert_eq!(held.detail["interval"]["reason"], "load");
    let deferred = of_kind("claim_deferral");
    assert_eq!(deferred.task_id, Some(TaskId::new(8)));
    assert_eq!(deferred.outcome.as_deref(), Some("expired"));
    assert_eq!(deferred.records.len(), 2);
    let installed = of_kind("update_installed");
    assert_eq!(installed.started_at, installed.ended_at);
    assert_eq!(subject(installed), ("update_installed", "s", None));
    assert_eq!(of_kind("attributes_baseline").records.len(), 1);
}

/// The Executions a window's rows keep give `stats`' context per run and
/// per kind and provider as the events do, an unmeasured value and one
/// not recorded told apart.
#[test]
fn the_rows_executions_give_the_context_the_events_give() {
    let log = queue_log();
    let window = through_ledger(&log.events, None);
    let from_events = executions(&log.events);
    assert_eq!(window.executions(), from_events);
    let records = window.context_records();
    assert_eq!(records, context::records(&from_events));
    assert_eq!(
        context::per_run(&records),
        context::per_run(&context::records(&from_events))
    );
    let by_kind = context::by_kind(&records, None, T0 + 60 * MIN);
    assert_eq!(
        by_kind,
        context::by_kind(&context::records(&from_events), None, T0 + 60 * MIN)
    );
    let planner = &by_kind["runtime_planner"]["codex"];
    assert_eq!(
        (
            planner.executions,
            planner.not_recorded,
            planner.peak_context.count
        ),
        (1, 0, 0)
    );
    assert_eq!(planner.peak_context.missing, 1);
    // A cut of the inbox records no context and is not counted.
    assert!(!by_kind.contains_key("inbox"));
}

/// A window by event ids or times takes the rows with an input in it, open
/// across it or ended in it; `--goal`, `--change` and `--area` choose among
/// them.
#[test]
fn a_window_takes_the_rows_of_its_cursors_and_filters() {
    let log = queue_log();
    let rows = build(&log.events, T0 + 60 * MIN, BUILT);
    let keys = |query: &WindowQuery| -> Vec<(LedgerKind, String)> {
        select(rows.clone(), query)
            .rows
            .iter()
            .map(|row| (row.kind, row.key.clone()))
            .collect()
    };
    let now_ms = T0 + 60 * MIN;
    // Events 4 to 8 (from the planner's turn to the goal review's end):
    // the planner, the plan review and the goal review, and task 7, open
    // across the window.
    let by_ids = keys(&WindowQuery {
        since: Some(Cursor::Event(EventId::new(3))),
        until: Some(Cursor::Event(EventId::new(8))),
        now_ms,
        ..WindowQuery::default()
    });
    assert_eq!(
        by_ids,
        [
            (LedgerKind::Task, "7".into()),
            (LedgerKind::Session, "runtime_planner/5/session/1".into()),
            (LedgerKind::Session, "plan_review/11/job/1".into()),
            (LedgerKind::Session, "goal_review/21/job/1".into()),
        ]
    );
    // From minute 31 to minute 34 by time: slots full ends in it, the hold
    // starts in it.
    let by_time = select(
        rows.clone(),
        &WindowQuery {
            since: Some(Cursor::Time(T0 + 31 * MIN)),
            until: Some(Cursor::Time(T0 + 34 * MIN)),
            now_ms,
            ..WindowQuery::default()
        },
    );
    assert_eq!(by_time.start_ms, Some(T0 + 31 * MIN));
    assert_eq!(by_time.end_ms, T0 + 34 * MIN);
    let kinds: Vec<&str> = by_time
        .of(LedgerKind::Queue)
        .map(|row| row.subject.as_ref().map_or("", |s| s.kind.as_str()))
        .collect();
    assert_eq!(kinds, ["slots_full", "claim_hold"]);
    // The run and its task, not ended, reach across it.
    let open: Vec<&str> = by_time
        .rows
        .iter()
        .filter(|row| row.kind != LedgerKind::Queue)
        .map(|row| row.key.as_str())
        .collect();
    assert_eq!(open, ["7", RUN]);
    // `--goal 3`: the goal review (no task), the run and the task of goal 3.
    let goal = keys(&WindowQuery {
        goal_id: Some(GoalId::new(3)),
        now_ms,
        ..WindowQuery::default()
    });
    assert_eq!(
        goal,
        [
            (LedgerKind::Task, "7".into()),
            (LedgerKind::Session, "goal_review/21/job/1".into()),
            (LedgerKind::Run, RUN.into()),
        ]
    );
    // `--change runtime`: the run and the task.
    let change = keys(&WindowQuery {
        change: Some(TaskChange::try_from("runtime".to_owned()).unwrap()),
        now_ms,
        ..WindowQuery::default()
    });
    assert_eq!(
        change,
        [
            (LedgerKind::Task, "7".into()),
            (LedgerKind::Run, RUN.into())
        ]
    );
    assert!(
        keys(&WindowQuery {
            change: Some(TaskChange::try_from("docs".to_owned()).unwrap()),
            now_ms,
            ..WindowQuery::default()
        })
        .is_empty()
    );
}

/// `--area` takes the runs whose landed commit changed a file of the area,
/// read from main's history in the queue rows.
#[test]
fn an_area_is_read_from_mains_history_in_the_rows() {
    let mut log = Log::default();
    log.push(0, (Some(7), None, true), "run_claimed", json!({}));
    log.push(
        1,
        (Some(7), None, true),
        "run_integrated",
        json!({"commit": "c1"}),
    );
    log.queue(
        2,
        "main_commits_recorded",
        json!({"after": null, "commits": [{"sha": "c1", "at": 1, "changes": [{"path": "docs/a.md"}]}]}),
    );
    let rows = build(&log.events, T0 + 10 * MIN, BUILT);
    let map = AreaMap::new(vec![("docs".into(), vec!["docs/**".into()])]).unwrap();
    let query = |area: &str| WindowQuery {
        area: Some(area.into()),
        areas: Some(map.clone()),
        now_ms: T0 + 10 * MIN,
        ..WindowQuery::default()
    };
    let docs = select(rows.clone(), &query("docs"));
    let keys: Vec<&str> = docs.rows.iter().map(|row| row.key.as_str()).collect();
    assert_eq!(keys, [RUN, "7"]);
    assert!(select(rows, &query("runtime")).rows.is_empty());
}

/// The same rows read at different ends: main's history and its reach are
/// those recorded by each end. Past the grace after a failure of Git, a
/// window is missing with its reason though a recovery and a new reach are
/// recorded later; one that ends after them is covered; a deletion recorded
/// later is not in an earlier window's paths.
#[test]
fn mains_history_in_the_rows_is_what_was_recorded_by_the_windows_end() {
    let mut log = Log::default();
    let since = (T0 - MIN) / 1000;
    log.queue(0, "supervisor_started", json!({"supervisor": "s"}));
    log.queue(
        0,
        "main_paths_recorded",
        json!({"head": "h", "part": 1, "parts": 1, "since": since, "paths": ["a", "b"]}),
    );
    // The head does not move: the reach goes on an hour after the base.
    log.queue(
        60,
        "main_observed",
        json!({"head": "h", "at": (T0 + 60 * MIN) / 1000, "since": since}),
    );
    log.queue(61, "supervisor_alive", json!({"supervisor": "s"}));
    log.queue(
        62,
        "main_read_failed",
        json!({"reason": "git failed", "at": (T0 + 62 * MIN) / 1000}),
    );
    for minute in [90, 120, 150, 180] {
        log.queue(minute, "supervisor_alive", json!({"supervisor": "s"}));
    }
    // Recovered and caught up, with `a` deleted.
    log.queue(
        200,
        "main_read_recovered",
        json!({"reason": "git failed", "since": (T0 + 62 * MIN) / 1000, "at": (T0 + 200 * MIN) / 1000}),
    );
    log.queue(
        201,
        "main_commits_recorded",
        json!({"after": "h", "commits": [{"sha": "c", "at": (T0 + 199 * MIN) / 1000,
               "changes": [{"path": "a", "deleted": true}]}]}),
    );
    log.queue(
        202,
        "main_observed",
        json!({"head": "c", "at": (T0 + 202 * MIN) / 1000, "since": since}),
    );
    let rows = build(&log.events, T0 + 210 * MIN, BUILT);
    let window = |minute: i64| {
        select(
            rows.clone(),
            &WindowQuery {
                since: Some(Cursor::Time(T0)),
                until: Some(Cursor::Time(T0 + minute * MIN)),
                now_ms: T0 + 210 * MIN,
                ..WindowQuery::default()
            },
        )
    };
    // Within the grace after the reach: covered, `a` still on main.
    let early = window(70);
    assert_eq!(early.main_covers(), Ok(()));
    assert!(early.main_log().history.paths.contains("a"));
    assert_eq!(
        early.main_log().recorded_through,
        Some((T0 + 60 * MIN) / 1000)
    );
    // Past the grace while Git fails: missing, the later recovery aside.
    assert!(matches!(
        window(150).main_covers(),
        Err(MainMissing::GitUnreadable { .. })
    ));
    assert!(matches!(
        window(150).main_history(),
        History::Unavailable(_)
    ));
    // After the catch-up: covered, `a` gone.
    let late = window(205);
    assert_eq!(late.main_covers(), Ok(()));
    assert!(!late.main_log().history.paths.contains("a"));
    assert!(matches!(late.main_history(), History::Read(_)));
}

/// A window that ends at an event takes that event, whatever its
/// milliseconds, and not one of the same time with a later id: the
/// attributes, the findings' baseline and the judgments of now alike.
#[test]
fn an_event_cursor_ends_the_folds_at_its_event() {
    let mut log = Log::default();
    let half = 500;
    let at = |log: &mut Log,
              ms: i64,
              ids: (Option<i64>, Option<i64>, bool),
              kind: &str,
              payload: Value| {
        log.push(0, ids, kind, payload);
        let last = log.events.last_mut().unwrap();
        last.created_at = utc_text(T0 + ms);
        last.id
    };
    let created = json!({"goal_id": null, "status": "ready", "title": "first", "change": null,
                         "execution_class": "implementation", "priority": null, "priority_by": null});
    at(
        &mut log,
        0,
        (Some(7), None, false),
        event_kind::TASK_CREATED,
        created,
    );
    // The baseline's mark half a second into a second.
    let mark = at(
        &mut log,
        half,
        (None, None, false),
        event_kind::ATTRIBUTES_BASELINE,
        json!({"tasks": 0, "goals": 0}),
    );
    // A rename of the same time, recorded after the mark.
    at(
        &mut log,
        half,
        (Some(7), None, false),
        event_kind::TASK_EDITED,
        json!({"from": {}, "to": {"title": "renamed"}}),
    );
    let finding_mark = at(
        &mut log,
        half,
        (None, None, false),
        event_kind::FINDING_BASELINE,
        json!({}),
    );
    let rows = build(&log.events, T0 + MIN, BUILT);
    let until = |id: EventId| {
        select(
            rows.clone(),
            &WindowQuery {
                until: Some(Cursor::Event(id)),
                now_ms: T0 + MIN,
                ..WindowQuery::default()
            },
        )
    };
    // The mark is in its own window; the event folded by seconds is not.
    assert!(matches!(
        attributes::fold(&log.events, Some(T0 / 1000)),
        attributes::Folded::BeforeBaseline
    ));
    let attributes::Folded::Attributes(at_mark) = until(mark).attributes() else {
        panic!("the mark is in the window")
    };
    assert_eq!(at_mark.tasks[&TaskId::new(7)].title, "first");
    let attributes::Folded::Attributes(after) = until(EventId::new(mark.as_i64() + 1)).attributes()
    else {
        panic!("after the mark")
    };
    assert_eq!(after.tasks[&TaskId::new(7)].title, "renamed");
    assert_eq!(until(mark).findings(), findings::Folded::BeforeBaseline);
    assert!(matches!(
        until(finding_mark).findings(),
        findings::Folded::Findings(_)
    ));
    assert_eq!(until(mark).events().last().unwrap().id, mark);
}

/// The ledger's Executions give what the events give across a day's end:
/// a session open over midnight counts each turn on the day it ended, and
/// a turn whose record has no context (before it was recorded) and one
/// whose window is not known are counted apart, not as 0.
#[test]
fn the_rows_count_tokens_by_their_day_and_missing_context_as_the_events_do() {
    const DAY: i64 = 24 * 60 * MIN;
    let midnight = (T0 / DAY + 1) * DAY;
    let mut log = Log::default();
    let at = |log: &mut Log, ms: i64, kind: &str, payload: Value| {
        log.push(0, (None, None, false), kind, payload);
        let last = log.events.last_mut().unwrap();
        last.created_at = utc_text(ms);
        last.id
    };
    let opened = at(
        &mut log,
        midnight - 30 * MIN,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 5, "route": "headless"}),
    );
    // Before midnight: its record has no context at all.
    at(
        &mut log,
        midnight - 10 * MIN,
        "turn_finished",
        json!({"planner_id": 5, "provider": "claude", "tokens_source": "model_usage",
               "tokens": tokens(100), "tokens_by_model": [], "model": "opus"}),
    );
    // After: a measured peak whose window is not known, and a measured one.
    let mut unknown = ended(json!({"planner_id": 5, "provider": "claude"}), Some(70_000));
    unknown["context_window"] = Value::Null;
    at(&mut log, midnight + 10 * MIN, "turn_finished", unknown);
    at(
        &mut log,
        midnight + 15 * MIN,
        "turn_finished",
        ended(json!({"planner_id": 5, "provider": "claude"}), Some(80_000)),
    );
    at(
        &mut log,
        midnight + 20 * MIN,
        "session_closed",
        json!({"opened_event_id": opened, "reason": "exited"}),
    );
    let window = through_ledger(&log.events, None);
    let rows = window.executions();
    let events = executions(&log.events);
    assert_eq!(rows, events);
    for (from, until) in [(midnight - DAY, midnight), (midnight, midnight + DAY)] {
        let from_rows =
            crate::domain::stats::executions::window(&rows, Some(from), until, |_| true);
        let from_events =
            crate::domain::stats::executions::window(&events, Some(from), until, |_| true);
        assert_eq!(from_rows, from_events);
    }
    let before =
        crate::domain::stats::executions::window(&rows, Some(midnight - DAY), midnight, |_| true);
    let after =
        crate::domain::stats::executions::window(&rows, Some(midnight), midnight + DAY, |_| true);
    assert_eq!((before.totals.executions, after.totals.executions), (1, 2));
    assert_eq!(before.totals.input, 100);
    let by_kind = context::by_kind(&window.context_records(), None, midnight + DAY);
    assert_eq!(
        by_kind,
        context::by_kind(&context::records(&events), None, midnight + DAY)
    );
    let planner = &by_kind["runtime_planner"]["claude"];
    assert_eq!((planner.executions, planner.not_recorded), (3, 1));
    assert_eq!(planner.peak_context.count, 2);
    assert_eq!(planner.window_ratio.count, 1);
    assert_eq!(
        planner
            .window_ratio
            .missing_by_reason
            .get(context::WINDOW_UNKNOWN),
        Some(&1)
    );
}

/// A session row ends when it closes and settles past its grace with no
/// later input; an interactive session waits a week for its close's cut.
/// A queue interval ends at its end, an event as it happens.
#[test]
fn session_and_queue_rows_end_get_dirty_and_settle() {
    let log = queue_log();
    let rows = build(&log.events, T0 + 60 * MIN, BUILT);
    let row = |key: &str| rows.iter().find(|row| row.key == key).unwrap().clone();
    let planner = row("runtime_planner/5/session/1");
    let through = planner.inputs_through.events;
    let closed = planner.ended_at.unwrap();
    let at = settlement(&planner, through, closed + SETTLE_GRACE_MS - 1);
    assert!(at.ended && !at.dirty && !at.settles);
    assert!(settlement(&planner, through, closed + SETTLE_GRACE_MS).settles);
    let later = settlement(
        &planner,
        EventId::new(through.as_i64() + 1),
        closed + SETTLE_GRACE_MS,
    );
    assert!(later.dirty && !later.settles);
    let mut old = planner.clone();
    old.ledger_v = LEDGER_V - 1;
    assert!(settlement(&old, through, closed + SETTLE_GRACE_MS).dirty);
    // Open: never settled.
    let mut open = planner;
    open.ended_at = None;
    let open = settlement(&open, through, closed + 10 * TOKEN_CUT_CLOSED_WINDOW_MS);
    assert!(!open.ended && !open.settles);
    // The inbox: a week's grace.
    let inbox = row("inbox//session/1");
    let end = inbox.ended_at.unwrap();
    let through = inbox.inputs_through.events;
    assert!(!settlement(&inbox, through, end + SETTLE_GRACE_MS).settles);
    assert!(settlement(&inbox, through, end + TOKEN_CUT_CLOSED_WINDOW_MS).settles);
    // A queue interval and event.
    let queue: Vec<LedgerRow> = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Queue)
        .cloned()
        .collect();
    let full = queue
        .iter()
        .find(|row| row.subject.as_ref().unwrap().kind == "slots_full")
        .unwrap();
    let end = full.ended_at.unwrap();
    assert!(settlement(full, full.inputs_through.events, end + SETTLE_GRACE_MS).settles);
    let mut running = full.clone();
    running.ended_at = None;
    assert!(!settlement(&running, full.inputs_through.events, end + SETTLE_GRACE_MS).ended);
    let installed = queue
        .iter()
        .find(|row| row.subject.as_ref().unwrap().kind == "update_installed")
        .unwrap();
    let state = settlement(
        installed,
        installed.inputs_through.events,
        installed.started_at.unwrap(),
    );
    assert!(state.ended && !state.settles);
}

/// A plan review its supervisor stopped, with no run, is a session row
/// whose Execution the window counts as the events do.
#[test]
fn a_stopped_job_of_no_run_keeps_its_execution() {
    let mut log = queue_log();
    log.queue(
        41,
        event_kind::HEADLESS_JOB_STOPPED,
        ended(json!({"kind": "plan_review", "proposal_id": 4}), Some(5)),
    );
    let window = through_ledger(&log.events, None);
    let stopped = window
        .of(LedgerKind::Session)
        .find(|row| row.key == "plan_review/4/events/0")
        .unwrap();
    assert_eq!(stopped.records.len(), 1);
    assert_eq!(window.executions(), executions(&log.events));
}

/// The window is timed as `stats` times it: a time `--until` not past
/// now, an event `--until` at its event's time, a `--since` not past the
/// end.
#[test]
fn a_windows_times_are_those_of_stats() {
    let log = queue_log();
    let rows = build(&log.events, T0 + 60 * MIN, BUILT);
    let now_ms = T0 + 50 * MIN;
    let at = |since: Option<Cursor>, until: Option<Cursor>| {
        let window = select(
            rows.clone(),
            &WindowQuery {
                since,
                until,
                now_ms,
                ..WindowQuery::default()
            },
        );
        (window.start_ms, window.end_ms)
    };
    assert_eq!(at(None, None), (None, now_ms));
    assert_eq!(at(None, Some(Cursor::Time(T0 + 90 * MIN))), (None, now_ms));
    assert_eq!(
        at(None, Some(Cursor::Event(EventId::new(3)))),
        (None, T0 + MIN)
    );
    // Past the latest event: the latest event's time.
    assert_eq!(
        at(None, Some(Cursor::Event(EventId::new(999)))),
        (None, T0 + 40 * MIN)
    );
    assert_eq!(
        at(
            Some(Cursor::Time(T0 + 45 * MIN)),
            Some(Cursor::Time(T0 + 20 * MIN))
        ),
        (Some(T0 + 20 * MIN), T0 + 20 * MIN)
    );
}

/// An event cursor bounds the rows by id, not by time: of two events of
/// the same time, `--until` the first leaves out the second's row and a
/// session opened at that time after it, and `--since` the first leaves
/// out its own row; a session open across the window stays in it.
#[test]
fn an_event_cursor_bounds_the_rows_of_the_same_time() {
    let mut log = Log::default();
    let across = log.queue(
        0,
        "session_opened",
        json!({"kind": "inbox", "session_id": "i"}),
    );
    let first = log.queue(5, "update_installed", json!({"supervisor": "s"}));
    let second = log.queue(5, "update_installed", json!({"supervisor": "t"}));
    log.queue(
        5,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 5, "route": "headless"}),
    );
    let rows = build(&log.events, T0 + 10 * MIN, BUILT);
    let keys = |since: Option<EventId>, until: Option<EventId>| -> Vec<String> {
        select(
            rows.clone(),
            &WindowQuery {
                since: since.map(Cursor::Event),
                until: until.map(Cursor::Event),
                now_ms: T0 + 10 * MIN,
                ..WindowQuery::default()
            },
        )
        .rows
        .iter()
        .map(|row| row.key.clone())
        .collect()
    };
    assert_eq!(
        keys(None, Some(first)),
        ["inbox//session/1".to_owned(), first.to_string()]
    );
    assert_eq!(
        keys(Some(first), None),
        [
            "inbox//session/1".to_owned(),
            second.to_string(),
            "runtime_planner/5/session/1".to_owned(),
        ]
    );
    assert_eq!(
        keys(Some(first), Some(second)),
        ["inbox//session/1".to_owned(), second.to_string()]
    );
    assert_eq!(across, EventId::new(1));
}

/// A plan review's and a goal review's job rows take the model of their
/// Claude session from its close, which is the span's row, as `stats`'
/// `jobs` does: start, the span's open, finish, the span's close.
#[test]
fn a_jobs_model_is_its_sessions_close_as_stats_reads_it() {
    let mut log = Log::default();
    for (n, kind, id, finished) in [
        (0, "plan_review", "plan_review_id", "plan_review_finished"),
        (10, "goal_review", "goal_review_id", "goal_review_finished"),
    ] {
        let session = format!("{kind}-session");
        log.queue(
            n,
            &format!("{kind}_started"),
            json!({id: 11, "session_id": session, "launch": {"provider": "claude"}}),
        );
        let opened = log.queue(
            n,
            "session_opened",
            json!({"kind": kind, "session_id": session, "route": "headless"}),
        );
        log.queue(n + 2, finished, json!({id: 11, "verdict": "pass"}));
        log.queue(
            n + 3,
            "session_closed",
            json!({"opened_event_id": opened, "session_id": session, "reason": "job_finished",
                   "model": "claude-opus-x", "effort": "high"}),
        );
    }
    let counted =
        crate::domain::stats::jobs::jobs(&log.events, EventId::new(0), EventId::new(100), |_| true);
    let rows = build(&log.events, T0 + 20 * MIN, BUILT);
    for (kind, key, close) in [
        ("plan_review", "plan_review/11/job/1", EventId::new(4)),
        ("goal_review", "goal_review/11/job/1", EventId::new(8)),
    ] {
        assert_eq!(
            counted[kind].by_model.keys().collect::<Vec<_>>(),
            ["claude-opus-x"]
        );
        let row = rows.iter().find(|row| row.key == key).unwrap();
        assert_eq!(
            row.subject.as_ref().unwrap().model.as_deref(),
            Some("claude-opus-x")
        );
        assert_eq!(row.detail["items"]["model"], "claude-opus-x");
        assert_eq!(row.detail["items"]["model_from"], close.as_i64());
        // The close is the span's record, and the job row reaches over it.
        assert!(row.records.iter().all(|event| event.id != close));
        assert_eq!(row.inputs_through.events, close);
    }
}

/// A slots-full interval and a live-alert stream that their supervisor's
/// silence ended keep the last event of that supervisor the fold read:
/// built again, they are not dirty and settle past their grace. A stop
/// recorded later (with an earlier last heartbeat) makes them dirty; built
/// again they take the end the stop gives, are clean and settle in turn.
#[test]
fn a_row_its_supervisors_life_ended_settles_and_moves_with_a_later_stop() {
    let mut log = Log::default();
    log.queue(0, "supervisor_started", json!({"supervisor": "s"}));
    log.queue(
        1,
        "slots_full_started",
        json!({"supervisor": "s", "class": "heavy", "running": 2, "slots": 2}),
    );
    log.queue(
        1,
        "live_alert_baseline",
        json!({"supervisor": "s", "version": 1}),
    );
    let alive = log.queue(2, "supervisor_alive", json!({"supervisor": "s"}));
    let of = |rows: &[LedgerRow], kind: &str| -> LedgerRow {
        rows.iter()
            .find(|row| row.subject.as_ref().is_some_and(|s| s.kind == kind))
            .unwrap()
            .clone()
    };
    let input = |row: &LedgerRow, events: &[RunEvent]| {
        row.inputs_through
            .events
            .max(life_input(row, events).unwrap())
    };
    let silent = T0 + 60 * MIN;
    let first = build(&log.events, silent, BUILT);
    for kind in ["slots_full", "live_alert_stream"] {
        let row = of(&first, kind);
        assert_eq!(
            (row.ended_at, row.outcome.as_deref()),
            (Some(T0 + 2 * MIN), Some("supervisor_gone"))
        );
        assert_eq!(row.inputs_through.supervisor, Some(alive));
        let state = settlement(&row, input(&row, &log.events), silent);
        assert!(!state.dirty && state.settles, "{kind}: {state:?}");
    }
    // The prune's stop, with the last heartbeat at minute 4.
    let stopped = log.queue(
        61,
        "supervisor_stopped",
        json!({"supervisor": "s", "last_heartbeat_at": (T0 + 4 * MIN) / 1000}),
    );
    for kind in ["slots_full", "live_alert_stream"] {
        let row = of(&first, kind);
        assert_eq!(life_input(&row, &log.events), Some(stopped));
        let state = settlement(&row, input(&row, &log.events), T0 + 62 * MIN);
        assert!(state.dirty && !state.settles, "{kind}: {state:?}");
    }
    let later = T0 + 80 * MIN;
    let second = build(&log.events, later, BUILT);
    for kind in ["slots_full", "live_alert_stream"] {
        let row = of(&second, kind);
        assert_eq!(row.ended_at, Some(T0 + 4 * MIN));
        assert_eq!(row.inputs_through.supervisor, Some(stopped));
        let state = settlement(&row, input(&row, &log.events), later);
        assert!(!state.dirty && state.settles, "{kind}: {state:?}");
        // The stop is not the row's own record: only the supervisor's input
        // reaches it.
        assert!(row.records.iter().all(|event| event.id != stopped));
    }
}

/// A window chosen by `--since`, `--until` and `--goal` counts the tokens
/// as `stats` does, `recorded_from` and `coverage` too: the records' start
/// is the queue's, though the session that started them is before the
/// window and is not one of the rows it chose.
#[test]
fn a_chosen_windows_tokens_keep_when_the_records_start() {
    use crate::domain::stats::{
        LiveSnapshot, SlotSnapshot, StatsQuery,
        executions::{Coverage, window as count},
    };
    let mut log = Log::default();
    for (start, planner) in [(0, 5), (60, 6)] {
        let opened = log.queue(
            start,
            "session_opened",
            json!({"kind": "runtime_planner", "planner_id": planner, "route": "headless"}),
        );
        log.queue(
            start + 2,
            "turn_finished",
            ended(
                json!({"planner_id": planner, "provider": "claude"}),
                Some(10),
            ),
        );
        log.queue(
            start + 5,
            "session_closed",
            json!({"opened_event_id": opened, "reason": "exited"}),
        );
    }
    log.push(
        70,
        (None, Some(3), false),
        "goal_review_started",
        json!({"goal_review_id": 21}),
    );
    log.push(
        72,
        (None, Some(3), false),
        "goal_review_finished",
        ended(json!({"goal_review_id": 21, "verdict": "open"}), Some(10)),
    );
    let now_ms = T0 + 120 * MIN;
    let rows = build(&log.events, now_ms, BUILT);
    for (since, until, goal) in [
        (30, 90, None),
        (80, 90, None),
        (30, 90, Some(GoalId::new(3))),
    ] {
        let (since, until) = (
            Cursor::Time(T0 + since * MIN),
            Cursor::Time(T0 + until * MIN),
        );
        let window = select(
            rows.clone(),
            &WindowQuery {
                since: Some(since),
                until: Some(until),
                goal_id: goal,
                now_ms,
                ..WindowQuery::default()
            },
        );
        let listed = crate::domain::stats::stats(
            &log.events,
            &HashMap::new(),
            now_ms / 1000,
            SlotSnapshot::default(),
            &StatsQuery {
                since: Some(since),
                until: Some(until),
                goal_id: goal,
                full: true,
            },
            &LiveSnapshot::default(),
        );
        let tokens = window.execution_tokens();
        assert_eq!(tokens, listed.execution_tokens, "{since:?} {goal:?}");
        assert_eq!(
            tokens.recorded_from.as_deref(),
            Some(utc_text(T0 + 2 * MIN).as_str())
        );
        assert_eq!(tokens.coverage, Coverage::Full);
        // The chosen rows alone would start the records in the window.
        let chosen = count(&window.executions(), window.start_ms, window.end_ms, |_| {
            true
        });
        assert_ne!(chosen.coverage, Coverage::Full);
    }
}
