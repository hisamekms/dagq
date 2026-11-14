//! A read of the ledger from a time against the read of every row, driven
//! identically by SQLite's memdb and the Postgres CI example: the earlier
//! records sought by what the range names
//! ([`crate::application::LedgerStore::ledger_earlier_named`]) give the
//! answers of every row, and each part goes back to the whole read when
//! its table cannot be trusted (docs/plans/ledger-window-earlier-visibility-spike.md,
//! "読みの約束" 3 to 5).
use std::collections::{BTreeSet, HashMap};

use super::{ContractStore, ledger_open::taken_over};
use crate::application::kpi::{Host, of_window as kpi_of_window};
use crate::application::stats::now_ms;
use crate::domain::kpi::{KpiConfig, KpiQuery, Period, read_from_ms};
use crate::domain::ledger::session::written::{
    Cursors, Marks, RowPosition, RowsWritten, SessionsWritten,
};
use crate::domain::ledger::testkit::{
    DAY, Log, READ_AT_DAY, T0, finding_record, log_of_a_time, numbered, run_id,
};
use crate::domain::ledger::window::{LedgerWindow, WindowQuery};
use crate::domain::ledger::{LedgerKind, LedgerRow, build, findings};
use crate::domain::stats::Cursor;
use crate::domain::{EventActor, EventId, EventKind, GoalId};
use crate::infrastructure::runtime_store::ledger_open_in as view;
use crate::infrastructure::sql_exec::{SqlRead, SqlWrite, sql_params};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

/// The time the windows are read at (unix seconds).
pub(crate) const NOW: i64 = T0 + READ_AT_DAY * DAY;
const BUILT: &str = "2030-01-01T00:00:00.000Z";
const BEFORE: i64 = 150 * DAY;
const NEAR: i64 = (READ_AT_DAY - 1) * DAY;
/// The run an older binary writes the row of, its events in the history.
const OLD_WRITER_RUN: u8 = 90;

/// The run id of the settled unit `k`.
fn unit_run(k: usize) -> String {
    format!("{k:08x}-2222-4222-8222-222222222222")
}

pub(crate) fn run(n: u8) -> String {
    run_id(n)
}

pub(crate) fn turn(log: &mut Log, task: i64, run: &str, provider: &str, secs: i64) {
    let tokens = json!({"input": 5, "output": 7, "cache_read": 0, "cache_creation": 0});
    log.add(
        Some(task),
        Some(run),
        "turn_finished",
        json!({"turn": 1, "provider": provider, "tokens": tokens,
               "tokens_source": "model_usage", "tokens_by_model": []}),
        secs,
    );
}

/// A finding the planner proposed into `proposal`.
fn proposed(log: &mut Log, finding: i64, proposal: i64, secs: i64) {
    let mut proposed = finding_record("proposed");
    proposed["finding_id"] = json!(finding);
    proposed[findings::VALUES]["id"] = json!(finding);
    let mut recorded = proposed.clone();
    recorded[findings::VALUES]["status"] = json!("open");
    proposed["from"] = json!("open");
    proposed["to"] = json!("proposed");
    proposed["proposal_id"] = json!(proposal);
    proposed["by"] = json!("planner");
    log.add(None, None, "finding_recorded", recorded, secs).add(
        None,
        None,
        "finding_status_changed",
        proposed,
        secs + 1,
    );
}

fn goal_submitted(log: &mut Log, goal: i64, proposal: i64, secs: i64) {
    log.add(
        None,
        None,
        "goal_submitted",
        json!({"proposal_id": proposal}),
        secs,
    );
    log.events.last_mut().unwrap().goal_id = Some(GoalId::new(goal));
}

/// The cases of Spike 4678 and 4755 before the range and in it.
fn cases(log: &mut Log, member_changes: usize) {
    // An ask opened before the range, answered in it; a session likewise.
    log.landed(60, &run(60), BEFORE).add(
        Some(60),
        Some(&run(60)),
        "ask_opened",
        json!({"ask_id": 7060, "kind": "worker_question"}),
        BEFORE + 1000,
    );
    log.add(
        Some(60),
        Some(&run(60)),
        "ask_answered",
        json!({"ask_id": 7060}),
        NEAR + 700,
    );
    log.landed(61, &run(61), BEFORE + 5000)
        .add(
            Some(61),
            Some(&run(61)),
            "session_opened",
            json!({"kind": "review", "provider": "claude"}),
            BEFORE + 5001,
        )
        .add(
            Some(61),
            Some(&run(61)),
            "session_closed",
            json!({"kind": "review", "provider": "claude"}),
            NEAR + 800,
        );
    // A landing's wait from before the range, landed in it.
    log.worked(63, &run(63), BEFORE + 20_000)
        .integrated(63, &run(63), NEAR + 1000)
        .pushed(63, &run(63), NEAR + 1300);
    // Proposal 8 of two tasks linked by a finding: task 66 completed before
    // the range (with `member_changes` changes of its plan before it), 65
    // completes in it.
    proposed(log, 50, 8, BEFORE + 40_000);
    for task in [66, 65] {
        log.add(
            Some(task),
            None,
            "task_submitted",
            json!({"proposal_id": 8, "owner_origin": "planner"}),
            BEFORE + 40_002,
        );
    }
    log.landed(66, &run(66), BEFORE + 40_010);
    for _ in 0..member_changes {
        log.add(
            Some(66),
            None,
            "task_priority_changed",
            json!({"priority": "high"}),
            BEFORE + 40_005,
        );
    }
    log.landed(65, &run(65), NEAR + 1600);
    // The first Executions: of `p-kept` the least on a row read (run 63)
    // and a later one left out (run 66); of `p-out` the least left out; of
    // `p-only` on a row read alone.
    turn(log, 63, &run(63), "p-kept", BEFORE + 20_001);
    turn(log, 66, &run(66), "p-kept", BEFORE + 40_011);
    turn(log, 66, &run(66), "p-out", BEFORE + 40_012);
    turn(log, 65, &run(65), "p-out", NEAR + 1601);
    turn(log, 65, &run(65), "p-only", NEAR + 1602);
    // (i-c): run 70's session closed by a close of the old form, run 71's
    // first never closed and its second closed twice.
    let session = |tag: &str| json!({"kind": "review", "provider": "claude", "tag": tag});
    let close = |closes: &str| json!({"kind": "review", "provider": "claude", "closes": closes});
    log.landed(70, &run(70), BEFORE - 4 * DAY)
        .add(
            Some(70),
            Some(&run(70)),
            "session_opened",
            session("70"),
            BEFORE - 4 * DAY + 1,
        )
        .add(
            Some(70),
            Some(&run(70)),
            "session_closed",
            close(""),
            BEFORE - 4 * DAY + 2,
        );
    log.landed(71, &run(71), BEFORE - 3 * DAY);
    for (tag, offset) in [("71a", 1), ("71b", 2)] {
        log.add(
            Some(71),
            Some(&run(71)),
            "session_opened",
            session(tag),
            BEFORE - 3 * DAY + offset,
        );
    }
    for offset in [3, 4] {
        log.add(
            Some(71),
            Some(&run(71)),
            "session_closed",
            close("71b"),
            BEFORE - 3 * DAY + offset,
        );
    }
    // (i-a): planner 70's session (headless) before the range, and in it
    // two `planner_question`s its actor opened, of no task and of task 62.
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 70, "route": "headless"}),
        BEFORE - 1000,
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": 70, "session_id": "p-70"}),
        BEFORE - 999,
    );
    log.add(
        Some(62),
        None,
        "task_submitted",
        json!({"proposal_id": 7}),
        BEFORE + 9000,
    );
    log.landed(62, &run(62), NEAR + 900);
    for (ask, task) in [(7070, None), (7071, Some(62))] {
        log.add(
            task,
            None,
            "ask_opened",
            json!({"ask_id": ask, "kind": "planner_question"}),
            NEAR + 2000,
        );
        log.events.last_mut().unwrap().actor = Some(EventActor {
            role: "planner".into(),
            id: "planner:70".into(),
            requested_by: None,
        });
    }
    // Planner 72's session before the range, and in it a `planner_question`
    // of an older binary, its planner in `asked_by` alone (no actor).
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 72, "route": "headless"}),
        BEFORE - 2000,
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": 72, "session_id": "p-72"}),
        BEFORE - 1999,
    )
    .add(
        None,
        None,
        "ask_opened",
        json!({"ask_id": 7072, "kind": "planner_question", "asked_by": "planner:72"}),
        NEAR + 2001,
    );
    // The last landings before the range: run 67's start, run 68's (never
    // ended), then run 69's, which ends 68's attempt; a later start of 67
    // the builder has not reflected yet comes with the builder behind.
    log.landed(67, &run(67), BEFORE + 100_000);
    log.worked(68, &run(68), BEFORE + 100_000).add(
        Some(68),
        Some(&run(68)),
        "integration_started",
        json!({}),
        BEFORE + 100_800,
    );
    log.landed(69, &run(69), BEFORE + 100_200);
    // (i-d): goal 80 in proposal 9, linked by finding 51; task 81 completes
    // in the range, 82 completed and 83 was canceled before it, and 83's
    // creation names plan review 9, which the range names.
    let before = BEFORE + 60_000;
    proposed(log, 51, 9, before);
    goal_submitted(log, 80, 9, before + 2);
    for task in [81, 82, 83] {
        let mut created =
            json!({"title": format!("t{task}"), "goal_id": 80, "status": "submitted"});
        if task == 83 {
            created["plan_review_id"] = json!(9);
        }
        log.add(Some(task), None, "task_created", created, before + 3);
    }
    log.landed(82, &run(82), before + 10);
    log.add(
        Some(83),
        None,
        "task_status_changed",
        json!({"from": "submitted", "to": "canceled"}),
        before + 20,
    );
    log.landed(81, &run(81), NEAR + 3000);
    // Goal 85 in proposal 10, linked by finding 52: task 86, of a binary
    // before the creation's record, holds its goal in its baseline alone
    // (which names plan review 9) and completed before the range; task 87
    // completes in it.
    let before = BEFORE + 70_000;
    proposed(log, 52, 10, before);
    goal_submitted(log, 85, 10, before + 2);
    log.add(
        Some(86),
        None,
        "attributes_baseline",
        json!({"title": "t86", "goal_id": 85, "status": "submitted", "plan_review_id": 9}),
        before + 3,
    )
    .add(
        Some(86),
        None,
        "task_status_changed",
        json!({"from": "submitted", "to": "completed"}),
        before + 10,
    )
    .add(
        Some(87),
        None,
        "task_created",
        json!({"title": "t87", "goal_id": 85, "status": "submitted"}),
        before + 3,
    )
    .add(
        Some(87),
        None,
        "task_status_changed",
        json!({"from": "submitted", "to": "completed"}),
        NEAR + 3100,
    );
    // Proposal 40 holding goal 88 and proposal 44 holding goal 89, each
    // linked by a finding alone (nothing of the range names them): their
    // own tasks 401 (of goal 88) and 441 (of no goal) were canceled before
    // the range with the proposal (a person's `cancel`, a withdrawal), and
    // the goals' tasks 402 and 442 are still to be done, so the
    // cancellations alone keep the proposals from running.
    let before = BEFORE + 90_000;
    for (finding, proposal, goal, own, own_goal, other, ended) in [
        (53, 40, 88, 401, Some(88), 402, "plan_decided"),
        (54, 44, 89, 441, None, 442, "proposal_withdrawn"),
    ] {
        proposed(log, finding, proposal, before);
        goal_submitted(log, goal, proposal, before + 2);
        let mut created = json!({"title": format!("t{own}"), "status": "submitted"});
        if let Some(own_goal) = own_goal {
            created["goal_id"] = json!(own_goal);
        }
        log.add(Some(own), None, "task_created", created, before + 3)
            .add(
                Some(own),
                None,
                "task_submitted",
                json!({"proposal_id": proposal, "owner_origin": "planner"}),
                before + 4,
            )
            .add(
                Some(own),
                None,
                ended,
                json!({"proposal_id": proposal, "status": "canceled"}),
                before + 9,
            )
            .add(
                Some(own),
                None,
                "task_status_changed",
                json!({"from": "submitted", "to": "canceled"}),
                before + 10,
            )
            .add(
                Some(other),
                None,
                "task_created",
                json!({"title": format!("t{other}"), "goal_id": goal, "status": "submitted"}),
                before + 3,
            );
    }
    // The run an older binary writes the row of: claimed long ago, its task
    // canceled, so it holds its slot to now; on a provider no row has and
    // on `p-out` before every other.
    let old = run(OLD_WRITER_RUN);
    log.add(Some(90), None, "task_created", json!({}), BEFORE + 80_000)
        .add(
            Some(90),
            Some(&old),
            "run_claimed",
            json!({"from": "ready", "to": "in_progress", "provider": "claude"}),
            BEFORE + 80_001,
        );
    turn(log, 90, &old, "p-new", BEFORE + 80_002);
    turn(log, 90, &old, "p-out", 100);
    log.add(
        Some(90),
        None,
        "task_status_changed",
        json!({"from": "in_progress", "to": "canceled"}),
        BEFORE + 80_003,
    );
}

std::thread_local! {
    /// The share of their span the settled units of the histories are
    /// spread over from their first: all of it unless a reader asks them
    /// nearer their start ([`with_units_nearer`]).
    static UNITS_SHARE: std::cell::Cell<(i64, i64)> = const { std::cell::Cell::new((1, 1)) };
}

/// The span `span` (seconds) the settled units of a history are spread
/// over, cut to the share [`with_units_nearer`] asks.
pub(crate) fn units_span(span: i64) -> i64 {
    let (num, den) = UNITS_SHARE.get();
    span * num / den
}

/// `f` with the settled units of the histories it lays down spread over
/// `num / den` of their span, nearer their first: so that a reader whose
/// range starts earlier than the others' (the weeks of a report) still
/// finds them all before it.
pub(crate) fn with_units_nearer<T>(num: i64, den: i64, f: impl FnOnce() -> T) -> T {
    /// Puts the share back however `f` ends, a panic too.
    struct Restore((i64, i64));
    impl Drop for Restore {
        fn drop(&mut self) {
            UNITS_SHARE.set(self.0);
        }
    }
    let _restore = Restore(UNITS_SHARE.replace((num, den)));
    f()
}

/// The settled units outside the range: each a task submitted to a
/// proposal of its own and landed by a run with an ask answered and a
/// follow-up's draft canceled, and a planner's session; all ended long
/// before it.
fn settled_units(log: &mut Log, settled: usize) {
    let span = units_span(139 * DAY);
    for k in 0..settled {
        let t = 11 * DAY + i64::try_from(k).unwrap() * span / i64::try_from(settled).unwrap();
        let task = 100_000 + i64::try_from(k).unwrap();
        let draft = 200_000 + task;
        let r = unit_run(k);
        log.add(
            Some(task),
            None,
            "task_submitted",
            json!({"proposal_id": 400_000 + task}),
            t - 10,
        );
        log.landed(task, &r, t)
            .add(
                Some(task),
                Some(&r),
                "ask_opened",
                json!({"ask_id": 900_000 + task, "kind": "worker_question"}),
                t + 100,
            )
            .add(
                Some(task),
                Some(&r),
                "ask_answered",
                json!({"ask_id": 900_000 + task}),
                t + 150,
            )
            .add(
                Some(task),
                Some(&r),
                "follow_up_registered",
                json!({"task_id": draft, "category": "defect", "index": 0}),
                t + 500,
            )
            .add(
                Some(draft),
                None,
                "task_created",
                json!({"status": "draft"}),
                t + 501,
            )
            .add(
                Some(draft),
                None,
                "task_status_changed",
                json!({"from": "draft", "to": "canceled"}),
                t + 600,
            )
            .add(
                None,
                None,
                "session_opened",
                json!({"kind": "runtime_planner", "planner_id": 300_000 + task}),
                t + 700,
            )
            .add(
                None,
                None,
                "session_closed",
                json!({"kind": "runtime_planner", "planner_id": 300_000 + task,
                       "session_id": format!("p-{task}")}),
                t + 800,
            );
    }
}

/// The history of [`log_of_a_time`] with the cases of Spike 4678 and 4755,
/// `settled` units outside the range and `member_changes` changes of the
/// plan of task 66 (of a proposal the range names, completed before it).
pub(crate) fn history(settled: usize, member_changes: usize) -> Log {
    let mut log = log_of_a_time();
    cases(&mut log, member_changes);
    settled_units(&mut log, settled);
    log.events.sort_by_key(|event| event.created_at.clone());
    let mut log = numbered(log);
    // The closes of (i-c) name the open of their tag, or none.
    let opens: HashMap<String, i64> = log
        .events
        .iter()
        .filter(|e| e.kind == EventKind::SessionOpened)
        .filter_map(|e| Some((e.payload["tag"].as_str()?.to_owned(), e.id.as_i64())))
        .collect();
    for event in &mut log.events {
        let Some(closes) = event.payload["closes"].as_str().map(str::to_owned) else {
            continue;
        };
        let payload = event.payload.as_object_mut().unwrap();
        match opens.get(&closes) {
            Some(id) => payload.insert("opened_event_id".into(), json!(id)),
            None => payload.remove("opened_event_id"),
        };
    }
    log
}

/// The marks of a store that wrote every row of the events up to `through`.
fn marks(through: EventId) -> Marks {
    let position = RowPosition {
        through: Cursors {
            events: through,
            ..Cursors::default()
        },
        read_ms: NOW * 1000,
    };
    Marks {
        sessions: SessionsWritten::at(through),
        rows: Some(RowsWritten {
            queue: Some(position),
            core: Some(position),
            ..RowsWritten::at(position.through, NOW * 1000)
        }),
        omitted: None,
    }
}

/// The rows of `log` as the store holds them, the old writer's run's
/// apart.
pub(crate) fn rows_of(log: &Log) -> (Vec<LedgerRow>, LedgerRow) {
    let (old, rows): (Vec<_>, Vec<_>) = build(&log.events, NOW * 1000, BUILT)
        .into_iter()
        .partition(|row| row.kind == LedgerKind::Run && row.key == run(OLD_WRITER_RUN));
    (rows, old.into_iter().next().expect("the old writer's run"))
}

/// What [`lay_down`] laid down: the history as written (its ids past those
/// the store held), the old writer's run's row it did not write, and the
/// tasks, goals and runs it added.
pub(crate) struct Laid {
    pub(crate) log: Log,
    pub(crate) old: LedgerRow,
    tasks: Vec<i64>,
    goals: Vec<i64>,
    runs: Vec<String>,
}

/// Lay `log` down on `tx`, its ids past those the store holds: its tasks,
/// goals and runs as the EventStore's keys need them (those the store has
/// not), its events, the rows but the old writer's run's, the marks, and
/// both tables filled.
pub(crate) fn lay_down(tx: &impl SqlWrite, log: &Log) -> Result<Laid> {
    let held: Option<i64> = tx.query_row("SELECT MAX(id) FROM run_events", sql_params![], |r| {
        r.get(0)
    })?;
    let past = held.unwrap_or(0);
    let mut log = log.clone();
    for event in &mut log.events {
        event.id = EventId::new(event.id.as_i64() + past);
        if let Some(opened) = event.payload.get("opened_event_id").and_then(Value::as_i64) {
            event.payload["opened_event_id"] = json!(opened + past);
        }
    }
    let ids = |of: &dyn Fn(&crate::domain::RunEvent) -> Option<i64>| -> BTreeSet<i64> {
        log.events.iter().filter_map(of).collect()
    };
    let runs: BTreeSet<(String, i64)> = log
        .events
        .iter()
        .filter_map(|e| Some((e.run_id.as_ref()?.as_str().to_owned(), e.task_id?.as_i64())))
        .collect();
    let mut laid = Laid {
        log: Log::default(),
        old: rows_of(&log).1,
        tasks: Vec::new(),
        goals: Vec::new(),
        runs: Vec::new(),
    };
    for goal in ids(&|e| e.goal_id.map(|goal| goal.as_i64())) {
        if tx.execute(
            "INSERT INTO goals(id, title) VALUES (?1, 'contract') ON CONFLICT DO NOTHING",
            sql_params![goal],
        )? > 0
        {
            laid.goals.push(goal);
        }
    }
    for task in ids(&|e| e.task_id.map(|task| task.as_i64())) {
        if tx.execute(
            "INSERT INTO tasks(id, title, description, acceptance, verification_commands, status)
             VALUES (?1, 'contract', '', '', '[]', 'draft') ON CONFLICT DO NOTHING",
            sql_params![task],
        )? > 0
        {
            laid.tasks.push(task);
        }
    }
    for (run, task) in runs {
        if tx.execute(
            "INSERT INTO task_runs(id, task_id, status, requested_provider, actual_provider, base_commit)
             VALUES (?1, ?2, 'succeeded', 'codex', 'codex', 'base') ON CONFLICT DO NOTHING",
            sql_params![run.as_str(), task],
        )? > 0
        {
            laid.runs.push(run);
        }
    }
    for e in &log.events {
        insert_event(tx, e)?;
    }
    for row in &rows_of(&log).0 {
        view::write_row(tx, row)?;
    }
    view::write_marks(tx, &marks(log.last()))?;
    fill(tx)?;
    laid.log = log;
    Ok(laid)
}

/// Insert `e` into the EventStore with its id.
pub(crate) fn insert_event(tx: &impl SqlWrite, e: &crate::domain::RunEvent) -> Result<()> {
    let actor = e.actor.as_ref();
    tx.execute(
        "INSERT INTO run_events(id, task_id, goal_id, run_id, kind, payload, created_at, actor_role, actor_id)
         VALUES (?1, ?2, ?3, ?4, ?5, dagq_event_body(?6), ?7, ?8, ?9)",
        sql_params![
            e.id.as_i64(),
            e.task_id.map(|task| task.as_i64()),
            e.goal_id.map(|goal| goal.as_i64()),
            e.run_id.as_ref().map(|run| run.as_str().to_owned()),
            e.kind.as_str(),
            e.payload.to_string(),
            e.created_at.as_str(),
            actor.map(|actor| actor.role.clone()),
            actor.map(|actor| actor.id.clone())
        ],
    )?;
    Ok(())
}

/// Remove what [`lay_down`] laid down, and the rows and tables.
pub(crate) fn clear(tx: &impl SqlWrite, laid: &Laid) -> Result<()> {
    for table in [
        "ledger_rows",
        "ledger_open",
        "ledger_open_filled",
        "ledger_first_executions",
        "ledger_first_executions_ready",
    ] {
        tx.execute(&format!("DELETE FROM {table}"), sql_params![])?;
    }
    for e in &laid.log.events {
        tx.execute(
            "DELETE FROM run_events WHERE id=?1",
            sql_params![e.id.as_i64()],
        )?;
    }
    for run in &laid.runs {
        tx.execute(
            "DELETE FROM task_runs WHERE id=?1",
            sql_params![run.as_str()],
        )?;
    }
    for task in &laid.tasks {
        tx.execute("DELETE FROM tasks WHERE id=?1", sql_params![task])?;
    }
    for goal in &laid.goals {
        tx.execute("DELETE FROM goals WHERE id=?1", sql_params![goal])?;
    }
    Ok(())
}

/// Fill both tables to their end.
pub(crate) fn fill(tx: &impl SqlWrite) -> Result<()> {
    for _ in 0..10_000 {
        let open = view::fill(tx)?.is_done();
        let first = view::first_fill(tx)?.is_done();
        if open && first {
            return Ok(());
        }
    }
    anyhow::bail!("the tables' filling did not finish")
}

const HOST: Host = Host {
    utc_offset_secs: 0,
    cores: Some(4),
    rust_toolchain: false,
};

/// The questions of `kpi` of Spike 4678's prototype.
pub(crate) fn kpi_questions() -> Vec<KpiQuery> {
    let day_two = KpiQuery {
        period: Period::Day,
        last: 2,
        ..KpiQuery::default()
    };
    vec![
        day_two.clone(),
        KpiQuery {
            period: Period::Week,
            last: 2,
            ..KpiQuery::default()
        },
        KpiQuery {
            by: vec!["nature".parse().unwrap(), "provider".parse().unwrap()],
            ..day_two.clone()
        },
        KpiQuery {
            at: Some(Cursor::Time((NOW - 2 * DAY) * 1000)),
            ..day_two
        },
    ]
}

/// The window of `query` read from its time.
pub(crate) fn time_query(query: &KpiQuery) -> WindowQuery {
    WindowQuery {
        now_ms: now_ms(NOW),
        read_from_ms: read_from_ms(query, &KpiConfig::default(), NOW, 0),
        ..WindowQuery::default()
    }
}

pub(crate) fn whole_query() -> WindowQuery {
    WindowQuery {
        now_ms: now_ms(NOW),
        ..WindowQuery::default()
    }
}

/// The answer of `query` of `window`.
pub(crate) fn kpi_value(window: &LedgerWindow, query: &KpiQuery) -> Result<Value> {
    Ok(serde_json::to_value(kpi_of_window(
        window,
        NOW,
        HOST,
        &KpiConfig::default(),
        None,
        query,
    )?)?)
}

/// The paths of `a` and `b` whose values differ, at most a few.
pub(crate) fn differences(a: &Value, b: &Value, path: &str, out: &mut Vec<String>) {
    if out.len() >= 16 {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for key in keys {
                differences(
                    x.get(key).unwrap_or(&Value::Null),
                    y.get(key).unwrap_or(&Value::Null),
                    &format!("{path}.{key}"),
                    out,
                );
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (x, y)) in x.iter().zip(y).enumerate() {
                differences(x, y, &format!("{path}[{i}]"), out);
            }
        }
        _ if a != b => out.push(format!("{path}: {a} != {b}")),
        _ => {}
    }
}

/// Whether each question's answer read from its time on `tx` is that of
/// every row, with the parts the reads sought (of the first question).
pub(crate) fn answers_as_every_row(tx: &impl SqlRead) -> Result<view::Sought> {
    let whole = view::window(tx, &whole_query())?;
    let mut sought = None;
    for (n, query) in kpi_questions().iter().enumerate() {
        let expected = kpi_value(
            &whole,
            &KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            },
        )?;
        let window = view::window(tx, &time_query(query))?;
        let got = kpi_value(&window, query)?;
        let mut misses = Vec::new();
        differences(&got, &expected, &format!("kpi[{n}]"), &mut misses);
        ensure!(misses.is_empty(), "{misses:?}");
        if n == 0 {
            let (from, named) = view::named(tx, &time_query(query))?;
            sought = Some(view::earlier(tx, &from, &named)?.1);
        }
    }
    sought.ok_or_else(|| anyhow::anyhow!("no question"))
}

/// On Postgres, the read of the session rows by the range of their keys
/// seeks the index of their keys in byte order (`ledger_rows_session_keys`),
/// the plan forced off a scan of the table as a long history would; SQLite's
/// plan is its adapter's test (its primary key orders by bytes).
fn sessions_seek_their_key_range(tx: &impl SqlWrite) -> Result<()> {
    let dialect = SqlRead::dialect(tx);
    if dialect != crate::infrastructure::sql_dialect::SqlDialect::Postgres {
        return Ok(());
    }
    tx.execute("SET LOCAL enable_seqscan = off", sql_params![])?;
    let sql = view::session_range_sql(dialect);
    let plan: Vec<String> = SqlRead::query_map(
        tx,
        &format!("EXPLAIN {sql}"),
        sql_params![
            "runtime_planner/70/",
            "runtime_planner/700",
            0_i64,
            0_i64,
            "2030-01-01T00:00:00.000Z"
        ],
        |r| r.get::<_, String>(0),
    )?;
    ensure!(
        plan.iter()
            .any(|line| line.contains("ledger_rows_session_keys"))
            && !plan
                .iter()
                .any(|line| line.contains("Seq Scan on ledger_rows")),
        "{plan:?}"
    );
    tx.execute("SET LOCAL enable_seqscan = on", sql_params![])?;
    Ok(())
}

/// The sought parts all true.
const ALL: view::Sought = view::Sought {
    records: true,
    runs: true,
    executions: true,
};

/// The answers of every row whether each part is sought or read whole: of
/// the store with both tables filled and its marks, and with the tables'
/// marks removed (a store that never filled them), the table of what the
/// run rows leave open filled in part, and no marks of the rows (whose
/// events' visibility then cannot be judged).
pub fn ledger_earlier_answers_as_every_row<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_ledger_earlier")?;
    let laid = lay_down(&tx, &history(30, 30))?;
    // The builder behind: a later change of status of member task 66 (its
    // row ended before the range, completed) and a later start of run 67
    // before the range are in the EventStore past the marks and in no row,
    // so the whole read does not see them: the last end a seek reads of the
    // task and the last start it reads are the last it sees.
    let mut lag = Log::default();
    lag.add(
        Some(67),
        Some(&run(67)),
        "integration_started",
        json!({}),
        BEFORE + 101_200,
    );
    lag.add(
        Some(66),
        None,
        "task_status_changed",
        json!({"from": "completed", "to": "ready"}),
        BEFORE + 40_020,
    );
    lag.add(
        Some(66),
        None,
        "task_goal_changed",
        json!({"from": null, "to": 80}),
        BEFORE + 40_021,
    );
    for (n, event) in lag.events.iter_mut().enumerate() {
        event.id = EventId::new(laid.log.last().as_i64() + 1 + i64::try_from(n)?);
        insert_event(&tx, event)?;
    }
    ensure!(answers_as_every_row(&tx)? == ALL);
    sessions_seek_their_key_range(&tx)?;
    // The regression of the baseline: task 86's end is found by its goal.
    let (from, named) = view::named(&tx, &time_query(&kpi_questions()[0]))?;
    let (earlier, _) = view::earlier(&tx, &from, &named)?;
    ensure!(
        earlier
            .records
            .iter()
            .any(|e| e.task_id.is_some_and(|t| t.as_i64() == 86)
                && e.kind == EventKind::TaskStatusChanged),
        "task 86's end"
    );
    tx.execute("DELETE FROM ledger_open_filled", sql_params![])?;
    tx.execute("DELETE FROM ledger_first_executions_ready", sql_params![])?;
    let none = view::Sought {
        runs: false,
        executions: false,
        ..ALL
    };
    ensure!(answers_as_every_row(&tx)? == none);
    fill(&tx)?;
    ensure!(answers_as_every_row(&tx)? == ALL);
    // Filled in part: the mark short of the last run row.
    let first: String = tx.query_row(
        "SELECT MIN(key) FROM ledger_rows WHERE kind='run'",
        sql_params![],
        |r| r.get(0),
    )?;
    tx.execute(
        "UPDATE ledger_open_filled SET through=?1",
        sql_params![first.as_str()],
    )?;
    ensure!(!view::held(&tx)?.is_done());
    ensure!(answers_as_every_row(&tx)? == view::Sought { runs: false, ..ALL });
    fill(&tx)?;
    ensure!(answers_as_every_row(&tx)? == ALL);
    // A store with neither table (of a schema before them): the run rows'
    // records and the first Executions are read whole.
    for table in [
        "ledger_open",
        "ledger_open_filled",
        "ledger_first_executions",
        "ledger_first_executions_ready",
    ] {
        tx.execute(&format!("DROP TABLE {table}"), sql_params![])?;
    }
    ensure!(answers_as_every_row(&tx)? == none);
    for kind in [
        crate::domain::ledger::session::written::KIND,
        crate::domain::ledger::session::written::ROWS_KIND,
    ] {
        tx.execute("DELETE FROM ledger_rows WHERE kind=?1", sql_params![kind])?;
    }
    ensure!(
        answers_as_every_row(&tx)?
            == view::Sought {
                records: false,
                runs: false,
                executions: false,
            }
    );
    drop(tx);
    Ok(())
}

/// An older binary takes the write leader over and writes a run row that
/// holds its slot open with a first Execution of a new provider and one
/// before every other of `p-out`, keeping neither table: the reads trust
/// neither table, whatever their marks hold, and answer as every row;
/// once this binary takes the leader over and fills them again, they read
/// them again, and still answer so.
pub fn ledger_earlier_past_an_old_writer<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let (first, a) = taken_over(open, None, "earlier-1")?;
    let tx = first.write_entry("contract_earlier_init")?;
    let laid = lay_down(&tx, &history(30, 0))?;
    let old = laid.old.clone();
    ensure!(answers_as_every_row(&tx)? == ALL);
    S::commit(tx)?;

    let (older, b) = taken_over(open, Some(("earlier-1", a)), "earlier-old")?;
    let tx = older.write_entry("contract_earlier_old")?;
    view::write_row_alone(&tx, &old)?;
    ensure!(!view::left_open(&tx)?.contains(&old.key));
    let none = view::Sought {
        runs: false,
        executions: false,
        ..ALL
    };
    ensure!(answers_as_every_row(&tx)? == none);
    S::commit(tx)?;

    let (current, c) = taken_over(open, Some(("earlier-old", b)), "earlier-current")?;
    let tx = current.write_entry("contract_earlier_current")?;
    view::write_row(&tx, &old)?;
    ensure!(answers_as_every_row(&tx)? == none);
    fill(&tx)?;
    ensure!(view::left_open(&tx)?.contains(&old.key));
    ensure!(answers_as_every_row(&tx)? == ALL);
    clear(&tx, &laid)?;
    S::commit(tx)?;
    ensure!(current.release_write_leader("earlier-current", c)?);
    Ok(())
}
