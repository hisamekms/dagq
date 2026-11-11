//! Whether what a read from a time still takes of the rows it leaves out
//! (the earlier records, [`crate::domain::ledger::window::EarlierRecords`])
//! and the queue rows can be read in a way that does not grow with the
//! settled history outside the range, with `kpi`'s and `stats`' answers
//! those of the whole ledger, and whether a small read of the EventStore
//! sees the events the whole read sees
//! (docs/plans/ledger-window-earlier-visibility-spike.md). Not wired: the
//! narrowed read below stands where
//! [`crate::domain::ledger::window::select_from_time`] gathers the earlier
//! records, and each test compares the answers with those of every row and
//! counts what the read returns with 30 and 3000 settled units outside
//! the range.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{Value, json};

use crate::application::kpi::{Host, of_window as kpi_of_window};
use crate::application::stats::{StatsSources, now_ms, of_window as stats_of_window};
use crate::domain::kpi::{KpiConfig, KpiQuery, Period, read_from_ms};
use crate::domain::ledger::owner::{Owner, owner};
use crate::domain::ledger::session::row_executions;
use crate::domain::ledger::testkit::{DAY, Log, READ_AT_DAY, T0, log_of_a_time, numbered};
use crate::domain::ledger::window::{
    EARLIER_RUN_KINDS, EarlierRecords, LedgerWindow, WindowQuery, WindowRows, ends_run_occupancy,
    run_on_its_way, runs_on_their_way, select, select_from_time,
};
use crate::domain::ledger::{LedgerKind, LedgerRow, build};
use crate::domain::marks::cursor_millis;
use crate::domain::run::{AskRef, restore_payload};
use crate::domain::stats::{Cursor, StatsQuery, executions, landing_utilization};
use crate::domain::{EventId, EventKind, RunEvent, RunId};

/// `score`'s private `TASK_MARK_KINDS`: the plan's changes a forecast's
/// sample counts after its snapshot.
const TASK_MARK_KINDS: [EventKind; 5] = [
    EventKind::TaskPriorityChanged,
    EventKind::DependencyAdded,
    EventKind::DependencyRemoved,
    EventKind::GoalDependencyAdded,
    EventKind::GoalDependencyRemoved,
];

const NOW: i64 = T0 + READ_AT_DAY * DAY;

/// The run id of the settled unit `k`.
fn unit_run(k: usize) -> String {
    format!("{k:08x}-2222-4222-8222-222222222222")
}

fn run(n: u8) -> String {
    crate::domain::ledger::testkit::run_id(n)
}

/// The history of [`log_of_a_time`] with the cases of the description
/// before the range and in it, and `settled` units outside the range,
/// each a task landed by a run with an ask answered, a follow-up's draft
/// canceled, a proposal of its own, a planner's session opened and closed
/// and a wait of the queue, all ended long before the range.
fn history(settled: usize) -> Log {
    let mut log = log_of_a_time();
    let near = (READ_AT_DAY - 1) * DAY;
    let before = 150 * DAY;
    // An ask opened before the range, answered in it.
    log.landed(60, &run(60), before).add(
        Some(60),
        Some(&run(60)),
        "ask_opened",
        json!({"ask_id": 7060, "kind": "worker_question"}),
        before + 1000,
    );
    log.add(
        Some(60),
        Some(&run(60)),
        "ask_answered",
        json!({"ask_id": 7060}),
        near + 700,
    );
    // A session opened before the range, closed in it.
    log.landed(61, &run(61), before + 5000)
        .add(
            Some(61),
            Some(&run(61)),
            "session_opened",
            json!({"kind": "review", "provider": "claude"}),
            before + 5001,
        )
        .add(
            Some(61),
            Some(&run(61)),
            "session_closed",
            json!({"kind": "review", "provider": "claude"}),
            near + 800,
        );
    // A proposal submitted before the range whose task lands in it.
    log.add(
        Some(62),
        None,
        "task_submitted",
        json!({"proposal_id": 7}),
        before + 9000,
    );
    log.landed(62, &run(62), near + 900);
    // A landing's wait from before the range, landed in it.
    log.worked(63, &run(63), before + 20_000)
        .integrated(63, &run(63), near + 1000)
        .pushed(63, &run(63), near + 1300);
    // A task whose only record before the range is of its attributes.
    log.add(
        Some(64),
        None,
        "task_priority_changed",
        json!({"priority": "high"}),
        before + 30_000,
    );
    log.landed(64, &run(64), near + 1500);
    // A proposal of two tasks, linked by a finding the planner proposed:
    // task 66 completed before the range, task 65 completes in it, so the
    // proposal runs no more by the range's end.
    let mut proposed = crate::domain::ledger::testkit::finding_record("proposed");
    proposed["finding_id"] = json!(50);
    proposed[crate::domain::ledger::findings::VALUES]["id"] = json!(50);
    let mut recorded = proposed.clone();
    recorded[crate::domain::ledger::findings::VALUES]["status"] = json!("open");
    proposed["from"] = json!("open");
    proposed["to"] = json!("proposed");
    proposed["proposal_id"] = json!(8);
    proposed["by"] = json!("planner");
    log.add(None, None, "finding_recorded", recorded, before + 40_000)
        .add(
            None,
            None,
            "finding_status_changed",
            proposed,
            before + 40_001,
        )
        .add(
            Some(66),
            None,
            "task_submitted",
            json!({"proposal_id": 8, "owner_origin": "planner"}),
            before + 40_002,
        )
        .add(
            Some(65),
            None,
            "task_submitted",
            json!({"proposal_id": 8, "owner_origin": "planner"}),
            before + 40_003,
        );
    log.landed(66, &run(66), before + 40_010);
    log.landed(65, &run(65), near + 1600);
    let span = 139 * DAY;
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
            )
            .add(None, None, "inbox_watcher_absent", json!({}), t + 900)
            .add(None, None, "inbox_watcher_returned", json!({}), t + 950);
        // The queue's rows of one event each or of a span each: a mark, a
        // deferral ended by the next claim, an alert, a finding resolved.
        let alert = json!({"supervisor": "s2", "judgment": "slots", "kind": "idle"});
        let mut finding = crate::domain::ledger::testkit::finding_record("open");
        let id = 10_000 + task;
        finding["finding_id"] = json!(id);
        finding[crate::domain::ledger::findings::VALUES]["id"] = json!(id);
        log.add(
            None,
            None,
            "mark_added",
            json!({"mark": format!("m{k}")}),
            t + 960,
        )
        .add(
            None,
            None,
            "claim_deferred",
            json!({"supervisor": "s2", "reason": "paths"}),
            t + 970,
        )
        .add(None, None, "live_alert_started", alert.clone(), t + 980)
        .add(None, None, "live_alert_ended", alert, t + 990)
        .add(None, None, "finding_recorded", finding.clone(), t + 995)
        .add(
            None,
            None,
            "finding_status_changed",
            {
                let mut changed = finding.clone();
                changed["from"] = json!("open");
                changed["to"] = json!("resolved");
                changed[crate::domain::ledger::findings::VALUES]["status"] = json!("resolved");
                changed
            },
            t + 999,
        );
    }
    log.events.sort_by_key(|event| event.created_at.clone());
    numbered(log)
}

fn rows_of(log: &Log) -> Vec<LedgerRow> {
    build(&log.events, NOW * 1000, "2030-01-01T00:00:00.000Z")
}

/// A part of the earlier records the narrowed read reads its own way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    /// The records of the task rows left out: those naming what the range
    /// names (a task, a plan review, a proposal), by a seek of each.
    TaskRecords,
    /// The records of the session rows left out: those of a session whose
    /// records name a planner or plan review the range names.
    SessionRecords,
    /// The claims and slot ends of the run rows left out: of the runs
    /// open at their row's end only (a summary the builder writes).
    Claims,
    /// The asks of the run rows left out: of the runs open at their end.
    Asks,
    /// The sessions of the run rows left out: of the runs open at their end.
    Sessions,
    /// `follow_up_registered`: those naming a draft the range names.
    FollowUps,
    /// The other [`EARLIER_RUN_KINDS`]: those of a task or proposal the
    /// range names.
    TaskKinds,
    /// What the landings read: the run of the last attempt started before
    /// the time, and those open at their end.
    Landings,
}

const PARTS: [Part; 8] = [
    Part::TaskRecords,
    Part::SessionRecords,
    Part::Claims,
    Part::Asks,
    Part::Sessions,
    Part::FollowUps,
    Part::TaskKinds,
    Part::Landings,
];

/// What the range names: the ids its rows and their records give.
#[derive(Debug, Default)]
struct Named {
    tasks: BTreeSet<i64>,
    reviews: BTreeSet<i64>,
    planners: BTreeSet<i64>,
    proposals: BTreeSet<i64>,
    /// The tasks of a forecast's snapshot that names a task the range
    /// names: their plan's changes count on its samples (Spike 4670 (vii)).
    snapshot_tasks: BTreeSet<i64>,
}

impl Named {
    fn note(&mut self, row: &LedgerRow) {
        self.tasks.extend(row.task_id.map(|task| task.as_i64()));
        for event in &row.records {
            self.tasks.extend(event.task_id.map(|task| task.as_i64()));
            let field = |key: &str| event.payload.get(key).and_then(Value::as_i64);
            self.reviews.extend(field("plan_review_id"));
            self.planners.extend(field("planner_id"));
            self.proposals.extend(field("proposal_id"));
        }
    }

    /// Note the tasks of the forecasts' snapshots among `queue` that name
    /// a task the range names, once the range's tasks are known.
    fn note_snapshots(&mut self, queue: &[LedgerRow]) {
        for event in queue.iter().flat_map(|row| &row.records) {
            if event.kind != EventKind::ForecastRecorded {
                continue;
            }
            let tasks: Vec<i64> = event.payload["tasks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|task| task["id"].as_i64())
                .collect();
            if tasks.iter().any(|task| self.tasks.contains(task)) {
                self.snapshot_tasks.extend(tasks);
            }
        }
    }

    /// Note as named the tasks a proposal the range names holds, of the
    /// rows left out `out`: a proposal's end is read of all its tasks'
    /// statuses (`improvement_proposals`).
    fn note_members(&mut self, out: &[LedgerRow]) {
        for event in out.iter().flat_map(|row| &row.records) {
            if matches!(
                event.kind.known(),
                Some(EventKind::TaskSubmitted | EventKind::TaskReopened)
            ) && event
                .payload
                .get("proposal_id")
                .and_then(Value::as_i64)
                .is_some_and(|id| self.proposals.contains(&id))
            {
                self.tasks.extend(event.task_id.map(|task| task.as_i64()));
            }
        }
    }

    fn names(&self, event: &RunEvent) -> bool {
        let mark = event
            .kind
            .known()
            .is_some_and(|kind| TASK_MARK_KINDS.contains(&kind));
        if mark
            && event
                .task_id
                .is_some_and(|task| self.snapshot_tasks.contains(&task.as_i64()))
        {
            return true;
        }
        let field = |key: &str| event.payload.get(key).and_then(Value::as_i64);
        event
            .task_id
            .is_some_and(|task| self.tasks.contains(&task.as_i64()))
            || field("task_id").is_some_and(|task| self.tasks.contains(&task))
            || field("plan_review_id").is_some_and(|id| self.reviews.contains(&id))
            || field("planner_id").is_some_and(|id| self.planners.contains(&id))
            || field("proposal_id").is_some_and(|id| self.proposals.contains(&id))
    }
}

/// Whether the run row `row` is open at its end: a claim with no end of
/// its slot, an ask with no answer, a session with no close, or a wait
/// for the landing with no end. What a builder would write of the row
/// (or into a table of its own) as it writes it, which a store reads by
/// an index of the open rows alone.
fn open_at_end(row: &LedgerRow) -> bool {
    if row.kind != LedgerKind::Run {
        return false;
    }
    let records = &row.records;
    let claimed = records.iter().any(|e| e.kind == EventKind::RunClaimed);
    let ended = records.iter().any(ends_run_occupancy);
    let mut asks: HashMap<Option<String>, bool> = HashMap::new();
    let (mut opened, mut closed) = (0, 0);
    let mut waiting = false;
    for event in records {
        match event.kind.known() {
            Some(EventKind::AskOpened) => {
                asks.entry(restore_payload::<AskRef>(&event.payload).key())
                    .or_insert(true);
            }
            Some(EventKind::AskAnswered) => {
                asks.insert(restore_payload::<AskRef>(&event.payload).key(), false);
            }
            Some(EventKind::SessionOpened) => opened += 1,
            Some(EventKind::SessionClosed) => closed += 1,
            _ => {}
        }
        if landing_utilization::reads(event) {
            waiting = matches!(
                event.kind.known(),
                Some(EventKind::LandingQueued | EventKind::IntegrationStarted)
            );
        }
    }
    (claimed && !ended) || asks.values().any(|open| *open) || opened > closed || waiting
}

/// The parts of the earlier records `event` (of a run row) is of.
fn parts_of_run_record(event: &RunEvent) -> Vec<Part> {
    let mut parts = Vec::new();
    match event.kind.known() {
        Some(EventKind::RunClaimed | EventKind::RunIntegrated | EventKind::RunSatisfiedClosed) => {
            parts.push(Part::Claims);
        }
        Some(EventKind::AskOpened | EventKind::AskAnswered) => parts.push(Part::Asks),
        Some(EventKind::SessionOpened | EventKind::SessionClosed) => parts.push(Part::Sessions),
        Some(EventKind::FollowUpRegistered) => parts.push(Part::FollowUps),
        Some(kind) if EARLIER_RUN_KINDS.contains(&kind) => parts.push(Part::TaskKinds),
        _ => {}
    }
    if ends_run_occupancy(event) && !parts.contains(&Part::Claims) {
        parts.push(Part::Claims);
    }
    if landing_utilization::reads(event) {
        parts.push(Part::Landings);
    }
    parts
}

/// What a read returned.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Reads {
    /// Run, task and session rows (by the time, wanted, open at their end).
    rows: usize,
    /// Earlier records.
    earlier: usize,
    /// First Executions (one per actor and provider).
    executions: usize,
    /// Queue rows (always whole).
    queue: usize,
}

/// The window `query` asks of `rows` read as
/// [`crate::domain::ledger::window::select_from_time`] reads it, the earlier
/// records of the `narrowed` parts taken by their narrowed reads
/// ([`Part`]) instead of whole, and what it returned.
fn narrowed(rows: &[LedgerRow], query: &WindowQuery, parts: &[Part]) -> (LedgerWindow, Reads) {
    narrowed_with(rows, query, parts, true)
}

/// [`narrowed`], the tasks of the proposals the range names taken among
/// those it names (`members`: a seek of each proposal's `task_submitted`
/// and `task_reopened` by its `proposal_id`, then of those tasks' records
/// by `events_by_task`) or not.
fn narrowed_with(
    rows: &[LedgerRow],
    query: &WindowQuery,
    narrowed: &[Part],
    members: bool,
) -> (LedgerWindow, Reads) {
    let mut reads = Reads::default();
    let mut read = WindowRows::new(query, 0);
    let (queue, others): (Vec<_>, Vec<_>) = rows
        .iter()
        .cloned()
        .partition(|row| row.kind == LedgerKind::Queue);
    reads.queue = queue.len();
    for row in queue.iter().cloned() {
        read.take(row);
    }
    let from = read.rows_from().expect("a read from a time");
    let (out, kept): (Vec<_>, Vec<_>) = others.into_iter().partition(|row| from.leaves_out(row));
    let mut named = Named::default();
    let mut taken: BTreeSet<(LedgerKind, String)> = BTreeSet::new();
    for row in &kept {
        named.note(row);
        taken.insert((row.kind, row.key.clone()));
    }
    // The rows the kept rows want (their tasks and the runs those name),
    // ahead of the earlier records, so that what they name is known.
    let mut wanted: Vec<LedgerRow> = Vec::new();
    loop {
        let more: Vec<LedgerRow> =
            out.iter()
                .filter(|row| !taken.contains(&(row.kind, row.key.clone())))
                .filter(|row| match row.kind {
                    LedgerKind::Task => row
                        .task_id
                        .is_some_and(|task| named.tasks.contains(&task.as_i64())),
                    LedgerKind::Run => kept.iter().chain(&wanted).any(|task| {
                        crate::domain::ledger::window::runs_named(task).contains(&row.key)
                    }),
                    _ => false,
                })
                .cloned()
                .collect();
        if more.is_empty() {
            break;
        }
        for row in more {
            named.note(&row);
            taken.insert((row.kind, row.key.clone()));
            wanted.push(row);
        }
    }
    named.note_snapshots(&queue);
    if members {
        named.note_members(&out);
    }
    let last_start = out
        .iter()
        .filter(|row| row.kind == LedgerKind::Run)
        .flat_map(|row| row.records.iter().map(move |e| (e, &row.key)))
        .filter(|(e, _)| e.kind == EventKind::IntegrationStarted)
        .filter(|(e, _)| {
            crate::domain::stats::timestamp_millis(&e.created_at)
                .is_some_and(|at| at <= from.ended_before_ms)
        })
        .max_by_key(|(e, _)| e.id)
        .map(|(_, key)| key.clone());
    let narrows = |part: Part| narrowed.contains(&part);
    let mut earlier = EarlierRecords::default();
    for row in out.iter().filter(|row| row.kind != LedgerKind::Node) {
        match row.kind {
            LedgerKind::Run => {
                let firsts = row_executions(row);
                earlier.executions.extend(firsts.iter().filter_map(|e| {
                    executions::FirstExecution::of(e.actor, e.provider.clone(), e.at)
                }));
                let open = open_at_end(row);
                for event in &row.records {
                    let parts = parts_of_run_record(event);
                    let keep = parts.iter().any(|part| {
                        !narrows(*part)
                            || match part {
                                Part::Claims | Part::Asks | Part::Sessions => open,
                                Part::FollowUps | Part::TaskKinds => named.names(event),
                                Part::Landings => open || last_start.as_ref() == Some(&row.key),
                                Part::TaskRecords | Part::SessionRecords => false,
                            }
                    });
                    if keep {
                        earlier.records.push(event.clone());
                    }
                }
            }
            LedgerKind::Task => earlier.records.extend(
                row.records
                    .iter()
                    .filter(|event| !narrows(Part::TaskRecords) || named.names(event))
                    .cloned(),
            ),
            _ => {
                let of_named = row.records.iter().any(|event| named.names(event));
                if !narrows(Part::SessionRecords) || of_named {
                    earlier.records.extend(row.records.iter().cloned());
                }
            }
        }
    }
    // One per actor and provider, as the store's GROUP BY returns them.
    let actors: BTreeSet<String> = earlier
        .executions
        .iter()
        .map(|first| {
            format!("{first:?}")
                .split(", at")
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    reads.executions = actors.len();
    reads.earlier = earlier.records.len();
    reads.rows = kept.len() + wanted.len();
    for row in kept.into_iter().chain(wanted) {
        read.take(row);
    }
    read.take_earlier(earlier);
    loop {
        let more = read.rows_wanted();
        if more.is_empty() {
            break;
        }
        for (kind, key) in more {
            let row = out
                .iter()
                .find(|row| row.kind == kind && row.key == key)
                .cloned();
            reads.rows += usize::from(row.is_some());
            read.take_row(row);
        }
    }
    (read.into_window(query), reads)
}

/// What [`select_from_time`] returns of the earlier records today: every
/// record of the task and session rows left out and the picked ones of
/// the run rows ([`crate::domain::ledger::window::select_from_time_with`]).
fn earlier_today(rows: &[LedgerRow], query: &WindowQuery) -> usize {
    narrowed(rows, query, &[]).1.earlier
}

const HOST: Host = Host {
    utc_offset_secs: 0,
    cores: Some(4),
    rust_toolchain: false,
};

fn kpi_questions() -> Vec<KpiQuery> {
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

fn kpi_value(window: &LedgerWindow, query: &KpiQuery) -> Value {
    serde_json::to_value(
        kpi_of_window(window, NOW, HOST, &KpiConfig::default(), None, query).unwrap(),
    )
    .unwrap()
}

fn time_query(query: &KpiQuery) -> WindowQuery {
    WindowQuery {
        now_ms: now_ms(NOW),
        read_from_ms: read_from_ms(query, &KpiConfig::default(), NOW, 0),
        ..WindowQuery::default()
    }
}

fn whole_query() -> WindowQuery {
    WindowQuery {
        now_ms: now_ms(NOW),
        ..WindowQuery::default()
    }
}

/// The paths of `a` and `b` whose values differ, at most a few.
fn differences(a: &Value, b: &Value, path: &str, out: &mut Vec<String>) {
    if out.len() >= 6 {
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

/// The kpi answers of the narrowed read of `parts` that differ from
/// those of every row, by question.
fn kpi_misses(rows: &[LedgerRow], parts: &[Part]) -> Vec<String> {
    let whole = select(rows.to_vec(), &whole_query());
    let mut misses = Vec::new();
    for (n, query) in kpi_questions().iter().enumerate() {
        let expected = kpi_value(
            &whole,
            &KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            },
        );
        let (window, _) = narrowed(rows, &time_query(query), parts);
        let mut out = Vec::new();
        differences(
            &kpi_value(&window, query),
            &expected,
            &format!("kpi[{n}]"),
            &mut out,
        );
        misses.extend(out);
    }
    misses
}

#[test]
fn the_read_from_a_time_today_returns_earlier_records_in_proportion_to_the_settled_history() {
    let query = time_query(&kpi_questions()[0]);
    let small = rows_of(&history(30));
    let large = rows_of(&history(3000));
    // The read of today gives the answers of every row, ...
    assert_eq!(kpi_misses(&small, &[]), Vec::<String>::new());
    let whole = select(small.clone(), &whole_query());
    let today = select_from_time(small.clone(), &query);
    assert_eq!(
        kpi_value(&today, &kpi_questions()[0]),
        kpi_value(
            &whole,
            &KpiQuery {
                read_whole_ledger: true,
                ..kpi_questions()[0].clone()
            }
        )
    );
    // ... and its earlier records and queue rows grow with the settled
    // units: (i) and (ii) of the description.
    let (_, small_reads) = narrowed(&small, &query, &[]);
    let (_, large_reads) = narrowed(&large, &query, &[]);
    eprintln!("today: 30 units {small_reads:?}, 3000 units {large_reads:?}");
    assert!(earlier_today(&large, &query) > 50 * earlier_today(&small, &query) / 2);
    assert!(large_reads.queue > 10 * small_reads.queue);
    assert_eq!(small_reads.rows, large_reads.rows);
}

#[test]
fn each_part_narrowed_alone_keeps_the_answers_of_every_row_or_names_what_it_misses() {
    let small = rows_of(&history(30));
    let mut report = BTreeMap::new();
    for part in PARTS {
        report.insert(format!("{part:?}"), kpi_misses(&small, &[part]));
    }
    eprintln!("{report:#?}");
    let missing: Vec<&String> = report
        .iter()
        .filter(|(_, misses)| !misses.is_empty())
        .map(|(part, _)| part)
        .collect();
    assert_eq!(missing, Vec::<&String>::new(), "{report:#?}");
}

#[test]
fn every_part_narrowed_returns_as_many_records_whatever_the_settled_history() {
    let query = time_query(&kpi_questions()[0]);
    let small = rows_of(&history(30));
    let large = rows_of(&history(3000));
    assert_eq!(kpi_misses(&large, &PARTS), Vec::<String>::new());
    let (_, small_reads) = narrowed(&small, &query, &PARTS);
    let (_, large_reads) = narrowed(&large, &query, &PARTS);
    eprintln!("narrowed: 30 units {small_reads:?}, 3000 units {large_reads:?}");
    assert_eq!(small_reads.earlier, large_reads.earlier);
    assert_eq!(small_reads.rows, large_reads.rows);
    assert_eq!(small_reads.executions, large_reads.executions);
}

// (iii) visibility.

/// The events the window of `rows` read whole sees, with the runs on their
/// way folded of `events` (the EventStore) as
/// [`crate::application::LedgerStore::ledger_window`] folds them.
fn visible(rows: &[LedgerRow], events: &[RunEvent]) -> BTreeSet<EventId> {
    let mut all = rows.to_vec();
    for run in runs_on_their_way(rows) {
        let of_run: Vec<RunEvent> = events
            .iter()
            .filter(|e| e.run_id.as_ref() == Some(&run))
            .cloned()
            .collect();
        all.extend(run_on_its_way(&run, &of_run, NOW * 1000));
    }
    select(all, &whole_query())
        .events()
        .iter()
        .map(|e| e.id)
        .collect()
}

/// What the small read keeps of `events`: an event whose owner's row is
/// saved with `inputs_through` at or past it (a run's by its run id, a
/// task's by its task id), or of a run on its way the saved task rows not
/// ended name. A session's or the queue's event has no key of its row in
/// it (a session row's key is the book's, [`crate::domain::ledger::session`]):
/// one at or before the mark (`mark`, W for the sessions and R for the
/// queue, read in the snapshot) is in a row by what the mark says, and one
/// past it only in a row whose `inputs_through` is past the mark, read by an
/// index on it (as many as the builder's lag). Counts the rows it looks up.
fn small_read(
    rows: &[LedgerRow],
    events: &[RunEvent],
    mark: EventId,
    looked: &mut usize,
) -> BTreeSet<EventId> {
    let by_key: HashMap<(LedgerKind, &str), &LedgerRow> = rows
        .iter()
        .map(|row| ((row.kind, row.key.as_str()), row))
        .collect();
    let on_their_way: BTreeSet<RunId> = runs_on_their_way(rows).into_iter().collect();
    let past_the_mark: BTreeSet<EventId> = rows
        .iter()
        .filter(|row| matches!(row.kind, LedgerKind::Session | LedgerKind::Queue))
        .filter(|row| row.inputs_through.events > mark)
        .flat_map(|row| row.records.iter().map(|record| record.id))
        .collect();
    let mut seen: BTreeSet<(LedgerKind, String)> = BTreeSet::new();
    let mut kept = BTreeSet::new();
    for event in events {
        let (kind, key) = match owner(event) {
            Owner::Run => (
                LedgerKind::Run,
                event.run_id.as_ref().unwrap().as_str().to_owned(),
            ),
            Owner::Task => (LedgerKind::Task, event.task_id.unwrap().to_string()),
            Owner::Session | Owner::Queue => {
                if event.id <= mark || past_the_mark.contains(&event.id) {
                    kept.insert(event.id);
                }
                continue;
            }
        };
        if seen.insert((kind, key.clone())) {
            *looked += 1;
        }
        let saved = by_key
            .get(&(kind, key.as_str()))
            .is_some_and(|row| row.inputs_through.events >= event.id);
        let on_its_way =
            kind == LedgerKind::Run && on_their_way.contains(event.run_id.as_ref().unwrap());
        if saved || on_its_way {
            kept.insert(event.id);
        }
    }
    kept
}

/// A store of the history `log` written up to `written` (R and W there),
/// the events after it not written yet; then the cases of the description:
/// a run resumed after its row (an event past its `inputs_through`), a
/// task ended whose run row is not written (a pass's bound of writes), a
/// run named by a legacy task row alone.
fn store_cases(log: &Log) -> (Vec<LedgerRow>, Vec<RunEvent>, Vec<EventId>, EventId) {
    let mut events = log.events.clone();
    let written = events.len() - 40;
    let mut rows = build(&events[..written], NOW * 1000, "2030-01-01T00:00:00.000Z");
    // A pass that wrote some of its session and queue rows past W and R
    // before it stopped: those rows hold records past the mark.
    let further = build(&log.events, NOW * 1000, "2030-01-01T00:00:00.000Z");
    let past: Vec<LedgerRow> = further
        .into_iter()
        .filter(|row| matches!(row.kind, LedgerKind::Session | LedgerKind::Queue))
        .filter(|row| row.inputs_through.events > events[written - 1].id)
        .collect();
    assert!(!past.is_empty(), "a row written past the mark");
    rows.retain(|row| !past.iter().any(|p| p.kind == row.kind && p.key == row.key));
    rows.extend(past);
    let mut next = events.last().unwrap().id.as_i64();
    let mut add = |events: &mut Vec<RunEvent>, task: i64, run: &str, kind: &str| {
        next += 1;
        events.push(RunEvent {
            id: EventId::new(next),
            task_id: Some(crate::domain::TaskId::new(task)),
            goal_id: None,
            run_id: Some(RunId::new(run).unwrap()),
            kind: kind.into(),
            payload: json!({}),
            created_at: crate::domain::marks::utc_text(NOW * 1000 - 10),
            actor: None,
        });
        EventId::new(next)
    };
    // Resumed: run 3 (landed long ago) gets an event after its row.
    let resumed = add(&mut events, 3, &run(3), "agent_started");
    // A task ended, its run row not written: drop run 12's row.
    rows.retain(|row| !(row.kind == LedgerKind::Run && row.key == run(12)));
    // A legacy task row naming run 16 alone, run 16's row not written.
    rows.retain(|row| !(row.kind == LedgerKind::Run && row.key == run(16)));
    for row in &mut rows {
        if row.kind == LedgerKind::Task && row.key == "6" {
            row.legacy = Some(json!({"runs": [run(16)]}));
            if let Some(detail) = row.detail.as_object_mut() {
                detail.remove("runs");
            }
        }
    }
    // Run 7's row dropped too. Its task was canceled, so its row ended and
    // names no run on its way: this store holds no run on its way (no task
    // row not ended names a run with no row), which the test does not reach.
    rows.retain(|row| !(row.kind == LedgerKind::Run && row.key == run(7)));
    let lagging = events[written..].iter().map(|e| e.id).collect::<Vec<_>>();
    let _ = resumed;
    let mark = events[written - 1].id;
    (rows, events, lagging, mark)
}

#[test]
fn the_small_read_of_the_saved_owners_and_the_runs_on_their_way_sees_what_the_whole_read_sees() {
    for settled in [30, 3000] {
        let log = history(settled);
        let (rows, events, lagging, mark) = store_cases(&log);
        let whole = visible(&rows, &events);
        let mut looked = 0;
        // Candidates: the events of a range (the last two days).
        let range: Vec<RunEvent> = events
            .iter()
            .filter(|e| {
                crate::domain::stats::timestamp_millis(&e.created_at)
                    .is_some_and(|at| at >= (NOW - 2 * DAY) * 1000)
            })
            .cloned()
            .collect();
        let small = small_read(&rows, &range, mark, &mut looked);
        let expected: BTreeSet<EventId> = range
            .iter()
            .map(|e| e.id)
            .filter(|id| whole.contains(id))
            .collect();
        let past_mark = small.iter().filter(|id| **id > mark).count();
        let extra: Vec<_> = small.difference(&expected).collect();
        let lacking: Vec<_> = expected.difference(&small).collect();
        eprintln!(
            "{settled}: range {}, visible {}, lagging {}, visible past the mark {past_mark}, looked up {looked}, extra {extra:?}, lacking {lacking:?}",
            range.len(),
            expected.len(),
            lagging.len()
        );
        assert_eq!(small, expected);
    }
}

#[test]
fn a_cursors_time_of_every_event_runs_past_that_of_the_visible_ones() {
    let log = history(30);
    let (rows, events, _, _) = store_cases(&log);
    let whole = visible(&rows, &events);
    let seen: Vec<RunEvent> = events
        .iter()
        .filter(|e| whole.contains(&e.id))
        .cloned()
        .collect();
    // An event no row holds (of run 12, whose task ended with no run row,
    // or past the mark) as the cursor: its time is later than every
    // visible event's up to it, so the time of every event up to it is.
    let (cursor, of_all, of_seen) = events
        .iter()
        .filter(|e| !whole.contains(&e.id))
        .map(|e| {
            let cursor = Cursor::Event(e.id);
            (
                e.id,
                cursor_millis(cursor, &events),
                cursor_millis(cursor, &seen),
            )
        })
        .find(|(_, all, seen)| all > seen)
        .expect("an invisible event later than the visible ones before it");
    eprintln!("cursor {cursor:?}: all {of_all}, visible {of_seen}");
}

#[test]
fn stats_since_a_time_answers_as_the_whole_read_with_the_narrowed_earlier() {
    let sources = StatsSources {
        config_file: &|| Ok(None),
        conflicts_file: &|| Ok(None),
        areas: None,
        utc_offset_secs: 0,
        counts_checks: true,
        rust_toolchain: false,
    };
    let since = Cursor::Time((NOW - 2 * DAY) * 1000);
    let query = StatsQuery {
        since: Some(since),
        full: true,
        ..StatsQuery::default()
    };
    for settled in [30, 3000] {
        let rows = rows_of(&history(settled));
        let base = WindowQuery {
            since: Some(since),
            now_ms: now_ms(NOW),
            ..WindowQuery::default()
        };
        let whole = select(rows.clone(), &base);
        let expected =
            serde_json::to_value(stats_of_window(&whole, NOW, &query, &sources).unwrap()).unwrap();
        let from = WindowQuery {
            read_from_ms: Some((NOW - 2 * DAY) * 1000),
            ..base.clone()
        };
        let kinds: BTreeSet<String> = rows
            .iter()
            .filter_map(|row| row.subject.as_ref().map(|s| s.kind.clone()))
            .collect();
        let queue_from = from.read_from_ms.unwrap();
        let (window, reads) = narrowed(&queue_narrowed(&rows, queue_from, &kinds), &from, &PARTS);
        let got =
            serde_json::to_value(stats_of_window(&window, NOW, &query, &sources).unwrap()).unwrap();
        let mut out = Vec::new();
        differences(&got, &expected, "stats", &mut out);
        eprintln!("stats {settled}: {reads:?} {out:#?}");
        assert_eq!(out, Vec::<String>::new());
    }
}

// (ii) queue rows.

/// The queue rows of `rows` narrowed for the subject kinds `kinds`: of
/// each such kind, a row that has not ended by the time `from_ms`, holds a
/// record from it on, or is the last of its kind ended before it, and the
/// row of the queue's last record at or before it (`queue_before`); every
/// row of another kind.
fn queue_narrowed(rows: &[LedgerRow], from_ms: i64, kinds: &BTreeSet<String>) -> Vec<LedgerRow> {
    use crate::domain::stats::timestamp_millis;
    let kind_of = |row: &LedgerRow| {
        row.subject
            .as_ref()
            .map_or_else(String::new, |s| s.kind.clone())
    };
    let reaches = |row: &LedgerRow| {
        row.ended_at.is_none_or(|end| end >= from_ms)
            || row
                .records
                .iter()
                .any(|e| timestamp_millis(&e.created_at).is_none_or(|at| at >= from_ms))
    };
    // A finding open at the time (its last status before it open or
    // proposed): what `findings_open` counts, whenever it was recorded.
    let open_finding = |row: &LedgerRow| {
        kind_of(row) == "finding"
            && row
                .records
                .iter()
                .rev()
                .filter(|e| timestamp_millis(&e.created_at).is_some_and(|at| at < from_ms))
                .find_map(|e| match e.kind.known() {
                    Some(EventKind::FindingRecorded) => Some("open"),
                    Some(EventKind::FindingStatusChanged) => e.payload["to"].as_str(),
                    _ => None,
                })
                .is_some_and(|status| matches!(status, "open" | "proposed"))
    };
    let mut last: BTreeMap<String, (i64, String)> = BTreeMap::new();
    let mut before: Option<(EventId, String)> = None;
    for row in rows.iter().filter(|row| row.kind == LedgerKind::Queue) {
        if let Some(end) = row.ended_at
            && end < from_ms
            && last.get(&kind_of(row)).is_none_or(|(at, _)| *at < end)
        {
            last.insert(kind_of(row), (end, row.key.clone()));
        }
        for e in &row.records {
            if timestamp_millis(&e.created_at).is_some_and(|at| at <= from_ms)
                && before.as_ref().is_none_or(|(id, _)| *id < e.id)
            {
                before = Some((e.id, row.key.clone()));
            }
        }
    }
    rows.iter()
        .filter(|row| {
            row.kind != LedgerKind::Queue
                || !kinds.contains(&kind_of(row))
                || reaches(row)
                || open_finding(row)
                || last
                    .get(&kind_of(row))
                    .is_some_and(|(_, key)| *key == row.key)
                || before.as_ref().is_some_and(|(_, key)| *key == row.key)
        })
        .cloned()
        .collect()
}

#[test]
fn each_queue_kind_read_by_its_range_keeps_the_answers_of_every_row_or_names_what_it_misses() {
    let small = rows_of(&history(30));
    let large = rows_of(&history(3000));
    let from_ms = time_query(&kpi_questions()[1]).read_from_ms.unwrap();
    let kinds: BTreeSet<String> = small
        .iter()
        .filter(|row| row.kind == LedgerKind::Queue)
        .map(|row| {
            row.subject
                .as_ref()
                .map_or_else(String::new, |s| s.kind.clone())
        })
        .collect();
    let count = |rows: &[LedgerRow], kind: &str| {
        rows.iter()
            .filter(|row| row.kind == LedgerKind::Queue)
            .filter(|row| row.subject.as_ref().map_or("", |s| s.kind.as_str()) == kind)
            .count()
    };
    let mut report = BTreeMap::new();
    for kind in &kinds {
        let only: BTreeSet<String> = [kind.clone()].into();
        let narrowed_small = queue_narrowed(&small, from_ms, &only);
        let narrowed_large = queue_narrowed(&large, from_ms, &only);
        let whole = select(small.clone(), &whole_query());
        let mut wrong = Vec::new();
        for (n, query) in kpi_questions().iter().enumerate() {
            let expected = kpi_value(
                &whole,
                &KpiQuery {
                    read_whole_ledger: true,
                    ..query.clone()
                },
            );
            let (window, _) = narrowed(&narrowed_small, &time_query(query), &PARTS);
            differences(
                &kpi_value(&window, query),
                &expected,
                &format!("kpi[{n}]"),
                &mut wrong,
            );
        }
        report.insert(
            kind.clone(),
            (
                (count(&small, kind), count(&large, kind)),
                (count(&narrowed_small, kind), count(&narrowed_large, kind)),
                wrong.len(),
                wrong.into_iter().take(2).collect::<Vec<_>>(),
            ),
        );
    }
    eprintln!("{report:#?}");
    let missing: Vec<&String> = report
        .iter()
        .filter(|(_, (_, (small, large), misses, _))| *misses > 0 || small != large)
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(missing, Vec::<&String>::new(), "{report:#?}");
}

// (iv) the reads of the whole path.

/// The kpi answers of `got`'s rows read by the narrowed read of every
/// part that differ from those of every row of `whole`.
fn kpi_misses_against(whole: &[LedgerRow], got: &[LedgerRow]) -> (Vec<String>, Reads) {
    let every = select(whole.to_vec(), &whole_query());
    let mut wrong = Vec::new();
    let mut reads = Reads::default();
    // The day's and the week's periods: the other two questions are
    // compared part by part on the small history.
    for (n, query) in kpi_questions().iter().take(2).enumerate() {
        let expected = kpi_value(
            &every,
            &KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            },
        );
        let (window, read) = narrowed(got, &time_query(query), &PARTS);
        if n == 0 {
            reads = read;
        }
        differences(
            &kpi_value(&window, query),
            &expected,
            &format!("kpi[{n}]"),
            &mut wrong,
        );
    }
    (wrong, reads)
}

#[test]
fn the_reads_of_the_whole_path_add_up_to_as_many_whatever_the_settled_history() {
    let from_ms = time_query(&kpi_questions()[1]).read_from_ms.unwrap();
    let mut totals = Vec::new();
    for settled in [30, 3000] {
        let rows = rows_of(&history(settled));
        let kinds: BTreeSet<String> = rows
            .iter()
            .filter_map(|row| row.subject.as_ref().map(|s| s.kind.clone()))
            .collect();
        let narrowed_rows = queue_narrowed(&rows, from_ms, &kinds);
        let (wrong, reads) = kpi_misses_against(&rows, &narrowed_rows);
        assert_eq!(wrong, Vec::<String>::new(), "{settled}");
        let (_, today) = narrowed(&rows, &time_query(&kpi_questions()[0]), &[]);
        // The small read's events: those of the range.
        let range = history(settled)
            .events
            .iter()
            .filter(|e| {
                crate::domain::stats::timestamp_millis(&e.created_at)
                    .is_some_and(|at| at >= (NOW - 2 * DAY) * 1000)
            })
            .count();
        let total = reads.rows + reads.earlier + reads.executions + reads.queue + range;
        let today_total = today.rows + today.earlier + today.executions + today.queue + range;
        eprintln!(
            "{settled}: narrowed {reads:?} + {range} events = {total}; today {today:?} = {today_total}"
        );
        totals.push(total);
    }
    assert_eq!(totals[0], totals[1]);
}

#[test]
fn a_proposal_the_range_names_is_read_with_the_ends_of_its_tasks_before_the_range() {
    let rows = rows_of(&history(30));
    let whole = select(rows.clone(), &whole_query());
    let query = kpi_questions()[0].clone();
    let expected = kpi_value(
        &whole,
        &KpiQuery {
            read_whole_ledger: true,
            ..query.clone()
        },
    );
    let running = |value: &Value| value["periods"][0]["kpis"]["improvement_proposals"].clone();
    // Of every row: both tasks completed, the proposal runs no more.
    eprintln!("whole: {}", running(&expected));
    let (with, _) = narrowed_with(&rows, &time_query(&query), &PARTS, true);
    let (without, _) = narrowed_with(&rows, &time_query(&query), &PARTS, false);
    let mut out = Vec::new();
    differences(&kpi_value(&without, &query), &expected, "without", &mut out);
    eprintln!("without the members: {out:?}");
    assert!(
        out.iter()
            .any(|miss| miss.contains("improvement_proposals")),
        "{out:?}"
    );
    assert_eq!(kpi_value(&with, &query), expected);
}
