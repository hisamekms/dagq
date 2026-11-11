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
/// and a wait of the queue, all ended long before the range; in the order
/// added.
fn unnumbered(settled: usize) -> Log {
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
    log
}

/// [`unnumbered`] sorted by time and numbered.
fn history(settled: usize) -> Log {
    history_with(settled, |_| {}, |_| {})
}

/// [`history`] with the events `add` adds before it is numbered, and
/// `fix` applied after (to the ids `numbered` puts in the payloads).
fn history_with(settled: usize, add: impl FnOnce(&mut Log), fix: impl FnOnce(&mut Log)) -> Log {
    let mut log = unnumbered(settled);
    add(&mut log);
    log.events.sort_by_key(|event| event.created_at.clone());
    let mut log = numbered(log);
    fix(&mut log);
    log
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
    /// The plan reviews named, as the text of the id (a session row's key
    /// holds it so; a payload gives it as text or as a number).
    review_keys: BTreeSet<String>,
    /// Whether an actor `planner:<id>` names its planner (an ask a planner
    /// opened names it so, [`crate::domain::stats::planner_routes`]).
    actors: bool,
    /// The tasks named only as members of a proposal the range names
    /// (directly or through a goal), whose ends alone are read
    /// ([`Shape::member_ends`]).
    member_tasks: BTreeSet<i64>,
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
            self.note_ids(event);
        }
    }

    /// Note the plan review's id of `event` as text and, when actors name,
    /// the planner its actor names.
    fn note_ids(&mut self, event: &RunEvent) {
        let review = &event.payload["plan_review_id"];
        self.review_keys.extend(
            review
                .as_str()
                .map(str::to_owned)
                .or_else(|| review.as_i64().map(|id| id.to_string())),
        );
        if self.actors {
            self.planners.extend(
                event
                    .actor
                    .as_ref()
                    .and_then(|actor| actor.id.as_str().strip_prefix("planner:"))
                    .and_then(|id| id.parse::<i64>().ok()),
            );
        }
    }

    /// Whether the session row of `key` (`{kind}/{id}/{what}/{round}`) is
    /// one the prefix seek reads: of a planner or a plan review named.
    fn prefix_names(&self, key: &str) -> bool {
        let mut parts = key.splitn(3, '/');
        let (Some(kind), Some(id)) = (parts.next(), parts.next()) else {
            return false;
        };
        match kind {
            "runtime_planner" => id
                .parse::<i64>()
                .is_ok_and(|id| self.planners.contains(&id)),
            "plan_review" => self.review_keys.contains(id),
            _ => false,
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
    fn note_members(&mut self, out: &[LedgerRow], ends: bool) {
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
                self.add_member(event.task_id.map(|task| task.as_i64()), ends);
            }
        }
    }

    fn add_member(&mut self, task: Option<i64>, ends: bool) {
        let Some(task) = task else { return };
        if !ends {
            self.tasks.insert(task);
        } else if !self.tasks.contains(&task) {
            self.member_tasks.insert(task);
        }
    }

    /// Note as members the tasks of the goals a proposal named holds
    /// (`goal_submitted` naming it, then the goal's tasks by their
    /// `task_created` and `task_goal_changed`), the proposals named being
    /// those the range names and those the read findings link (`queue`).
    fn note_goal_members(&mut self, queue: &[LedgerRow], out: &[LedgerRow], ends: bool) {
        let records = || queue.iter().flat_map(|row| &row.records);
        let mut proposals = self.proposals.clone();
        proposals.extend(
            records()
                .filter(|e| e.kind == EventKind::FindingStatusChanged)
                .filter(|e| e.payload["to"] == "proposed")
                .filter_map(|e| e.payload["proposal_id"].as_i64()),
        );
        let goals: BTreeSet<i64> = records()
            .filter_map(crate::domain::GoalSubmitted::of_event)
            .filter(|(_, submitted)| proposals.contains(&submitted.proposal_id.as_i64()))
            .map(|(goal, _)| goal.as_i64())
            .collect();
        let tasks: Vec<i64> = out
            .iter()
            .filter(|row| row.kind == LedgerKind::Task)
            .flat_map(|row| &row.records)
            .filter(|e| {
                let goal = match e.kind.known() {
                    Some(EventKind::TaskCreated) => e.payload["goal_id"].as_i64(),
                    Some(EventKind::TaskGoalChanged) => e.payload["to"].as_i64(),
                    _ => None,
                };
                goal.is_some_and(|goal| goals.contains(&goal))
            })
            .filter_map(|e| e.task_id.map(|task| task.as_i64()))
            .collect();
        for task in tasks {
            self.add_member(Some(task), ends);
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
fn open_at_end(row: &LedgerRow, paired: bool) -> bool {
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
    // Paired as `take_earlier` and `stats::sessions::spans` pair them: a
    // close ends the open its `opened_event_id` names, so a close naming
    // none or one already closed ends nothing.
    let closes: BTreeSet<EventId> = records
        .iter()
        .filter(|e| e.kind == EventKind::SessionClosed)
        .filter_map(|e| crate::domain::sessions::SpanRef::of(e).opened_event_id)
        .collect();
    let session_open = if paired {
        records
            .iter()
            .any(|e| e.kind == EventKind::SessionOpened && !closes.contains(&e.id))
    } else {
        opened > closed
    };
    (claimed && !ended) || asks.values().any(|open| *open) || session_open || waiting
}

/// Whether `event` is among the ends the shape of [`Shape::member_ends`]
/// reads of a member task's left-out row `records`: its creation, its last
/// change of goal, its last change of status and the reopenings after it.
fn member_end(event: &RunEvent, records: &[RunEvent]) -> bool {
    let last = |kind: EventKind| records.iter().rev().find(|e| e.kind == kind).map(|e| e.id);
    match event.kind.known() {
        Some(EventKind::TaskCreated) => true,
        Some(EventKind::TaskGoalChanged) => last(EventKind::TaskGoalChanged) == Some(event.id),
        Some(EventKind::TaskStatusChanged) => last(EventKind::TaskStatusChanged) == Some(event.id),
        Some(EventKind::TaskReopened) => {
            last(EventKind::TaskStatusChanged).is_none_or(|status| event.id > status)
        }
        _ => false,
    }
}

/// The shapes of the parts Spike 4755 tries ((i-a) to (i-d) of
/// docs/plans/ledger-window-earlier-visibility-spike.md), each off as
/// Spike 4678 read it.
#[derive(Debug, Clone, Copy, Default)]
struct Shape {
    /// The second step of the proposals named (their tasks).
    members: bool,
    /// (i-a): a left-out session row is read by the prefix of its key
    /// alone (`runtime_planner/<id>/`, `plan_review/<id>/`) instead of
    /// whole when any record names what the range names.
    session_prefix: bool,
    /// (i-a): an actor `planner:<id>` of the range names its planner.
    actor_planners: bool,
    /// (i-b): a member task's ends alone ([`member_end`]), not its records.
    member_ends: bool,
    /// (i-c): a run's sessions paired by `opened_event_id`.
    paired: bool,
    /// (i-d): the tasks of a goal a proposal named holds.
    goal_members: bool,
}

/// Every shape of Spike 4755 on.
const SHAPED: Shape = Shape {
    members: true,
    session_prefix: true,
    actor_planners: true,
    member_ends: true,
    paired: true,
    goal_members: true,
};

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
    let shape = Shape {
        members,
        ..Shape::default()
    };
    narrowed_shaped(rows, query, narrowed, shape)
}

/// [`narrowed`] with the parts read in `shape`.
fn narrowed_shaped(
    rows: &[LedgerRow],
    query: &WindowQuery,
    narrowed: &[Part],
    shape: Shape,
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
    let mut named = Named {
        actors: shape.actor_planners,
        ..Named::default()
    };
    if shape.actor_planners {
        // The range's records of the queue rows name a planner by an ask's
        // actor too (an ask of no task or run is the queue's).
        for event in queue.iter().flat_map(|row| &row.records) {
            let at = crate::domain::stats::timestamp_millis(&event.created_at);
            if at.is_some_and(|at| at >= from.ended_before_ms) {
                named.note_ids(event);
            }
        }
    }
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
    if shape.members {
        named.note_members(&out, shape.member_ends);
    }
    if shape.goal_members {
        named.note_goal_members(&queue, &out, shape.member_ends);
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
                let open = open_at_end(row, shape.paired);
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
                    .filter(|event| {
                        let member = row
                            .task_id
                            .is_some_and(|task| named.member_tasks.contains(&task.as_i64()));
                        !narrows(Part::TaskRecords)
                            || named.names(event)
                            || (member && member_end(event, &row.records))
                    })
                    .cloned(),
            ),
            _ => {
                let of_named = if shape.session_prefix {
                    named.prefix_names(&row.key)
                } else {
                    row.records.iter().any(|event| named.names(event))
                };
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
    if out.len() >= 64 {
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

// (iii) visibility, the follow-up: a run on its way, the session and queue
// events past W and R a pass did not write, R and W apart, the owners' rows
// R spares, and U_E (the section "追試 (iii) 可視性（Spike 4754）").

/// The marks of a store: W, where the session rows record every session
/// event up to, and R, the queue rows' and the run, task and node rows'
/// positions, each its own.
#[derive(Debug, Clone, Copy)]
struct Marks {
    w: EventId,
    queue: EventId,
    core: EventId,
}

/// A store of the history of `settled` units with a tail past it: a task
/// claimed by run 70 before R (its row written, not ended, naming the run,
/// the run's row not written as it has not ended) whose run records past R;
/// a task event of it past R its row does not hold; run 3 resumed past R;
/// run 12's row not written (its task ended); session and queue events
/// past W and R, of which a pass wrote the rows of some only (planner
/// 990001's session row and the row of mark `m-a`). R of the queue stands
/// before W, with queue events between them. The rows, the events, the
/// marks and the run on its way.
fn store_apart(settled: usize) -> (Vec<LedgerRow>, Vec<RunEvent>, Marks, RunId) {
    let mut events = history(settled).events;
    let running = RunId::new(run(70)).unwrap();
    let minute = 60_000;
    let push = |events: &mut Vec<RunEvent>,
                task: Option<i64>,
                run: Option<&str>,
                kind: &str,
                payload: Value,
                minutes_ago: i64| {
        let id = EventId::new(events.last().unwrap().id.as_i64() + 1);
        events.push(RunEvent {
            id,
            task_id: task.map(crate::domain::TaskId::new),
            goal_id: None,
            run_id: run.map(|run| RunId::new(run).unwrap()),
            kind: kind.into(),
            payload,
            created_at: crate::domain::marks::utc_text(NOW * 1000 - minutes_ago * minute),
            actor: None,
        });
        id
    };
    let planner = |id: i64| json!({"kind": "runtime_planner", "planner_id": id});
    let closed = |id: i64, opened: EventId| {
        json!({"kind": "runtime_planner", "planner_id": id,
               "session_id": format!("p-{id}"), "opened_event_id": opened.as_i64()})
    };
    let r70 = run(70);
    push(&mut events, Some(70), None, "task_created", json!({}), 180);
    push(
        &mut events,
        Some(70),
        Some(&r70),
        "run_claimed",
        json!({"from": "ready", "to": "in_progress", "provider": "claude"}),
        180,
    );
    push(
        &mut events,
        Some(70),
        Some(&r70),
        "agent_started",
        json!({}),
        179,
    );
    let first = push(
        &mut events,
        None,
        None,
        "session_opened",
        planner(990_001),
        170,
    );
    let third = push(
        &mut events,
        None,
        None,
        "session_opened",
        planner(990_003),
        169,
    );
    let core = events.last().unwrap().id;
    // Run 70's events past R: one of no task id, one of another task's id
    // (an older or another writer's record); the whole read folds them by
    // the run's id.
    push(
        &mut events,
        None,
        Some(&r70),
        "first_commit_observed",
        json!({}),
        160,
    );
    push(
        &mut events,
        Some(70),
        None,
        "task_priority_changed",
        json!({"priority": "high"}),
        150,
    );
    push(
        &mut events,
        Some(3),
        Some(&run(3)),
        "agent_started",
        json!({}),
        145,
    );
    let queue = events.last().unwrap().id;
    push(
        &mut events,
        None,
        None,
        "session_opened",
        planner(990_004),
        135,
    );
    push(
        &mut events,
        None,
        None,
        "mark_added",
        json!({"mark": "m-b"}),
        140,
    );
    push(
        &mut events,
        None,
        None,
        "claim_deferred",
        json!({"supervisor": "s2", "reason": "paths"}),
        130,
    );
    let w = events.last().unwrap().id;
    push(
        &mut events,
        None,
        None,
        "session_closed",
        closed(990_001, first),
        120,
    );
    push(
        &mut events,
        None,
        None,
        "session_closed",
        closed(990_003, third),
        110,
    );
    let second = push(
        &mut events,
        None,
        None,
        "session_opened",
        planner(990_002),
        100,
    );
    push(
        &mut events,
        None,
        None,
        "session_closed",
        closed(990_002, second),
        90,
    );
    push(
        &mut events,
        None,
        None,
        "mark_added",
        json!({"mark": "m-a"}),
        80,
    );
    push(
        &mut events,
        Some(999),
        Some(&r70),
        "receipt_observed",
        json!({}),
        60,
    );
    push(
        &mut events,
        None,
        None,
        "inbox_watcher_absent",
        json!({}),
        50,
    );
    let up_to = |id: EventId, kinds: &[LedgerKind]| {
        let upto = events.iter().take_while(|e| e.id <= id).count();
        build(&events[..upto], NOW * 1000, "2030-01-01T00:00:00.000Z")
            .into_iter()
            .filter(|row| kinds.contains(&row.kind))
            .collect::<Vec<_>>()
    };
    let mut rows = up_to(core, &[LedgerKind::Run, LedgerKind::Task]);
    rows.extend(up_to(w, &[LedgerKind::Session]));
    rows.extend(up_to(queue, &[LedgerKind::Queue]));
    // No row of run 70, not ended (the builder writes a run's row once its
    // life ended; run 7 of the history keeps the row `build` gives it, out
    // of the range), nor (past a pass's bound of writes) of run 12.
    rows.retain(|row| {
        !(row.kind == LedgerKind::Run && [r70.as_str(), &run(12)].contains(&row.key.as_str()))
    });
    // A pass past the marks wrote two rows and stopped.
    let further = build(&events, NOW * 1000, "2030-01-01T00:00:00.000Z");
    for row in further.into_iter().filter(|row| {
        (row.kind == LedgerKind::Session && row.key.contains("990001"))
            || (row.kind == LedgerKind::Queue
                && row.records.iter().any(|e| e.payload["mark"] == "m-a"))
    }) {
        rows.retain(|held| !(held.kind == row.kind && held.key == row.key));
        rows.push(row);
    }
    (rows, events, Marks { w, queue, core }, running)
}

/// What a small read looked up to judge its events.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Looked {
    /// The owners' rows (runs and tasks), each once.
    owners: usize,
    /// The task rows not ended, read once for the runs with no row.
    open_tasks: usize,
    /// The session and queue rows written past W and R.
    past_the_marks: usize,
}

/// The small read of `candidates` with the marks apart: a run's event if
/// its run row is saved with `inputs_through` at or past it, or, with no
/// run row, if a saved task row not ended names the run in its
/// `detail.runs` (the open task rows, read once, whatever the event's task
/// id: the whole read's [`runs_on_their_way`]); a task's event if its row is
/// saved with `inputs_through` at or past it, or (`spare`) at or before R
/// of the run, task and node rows, its row not looked up; a session's event
/// at or before W, a queue's at or before R of the queue, or one held by a
/// row of its kind written past that mark. `one_mark` judges the session's
/// and the queue's events by W alone, as one mark would.
fn small_read_apart(
    rows: &[LedgerRow],
    candidates: &[RunEvent],
    marks: Marks,
    spare: bool,
    one_mark: bool,
) -> (BTreeSet<EventId>, Looked) {
    let by_key: HashMap<(LedgerKind, &str), &LedgerRow> = rows
        .iter()
        .map(|row| ((row.kind, row.key.as_str()), row))
        .collect();
    let mut looked = Looked::default();
    let mut past = |kind: LedgerKind, mark: EventId| -> BTreeSet<EventId> {
        let past: Vec<&LedgerRow> = rows
            .iter()
            .filter(|row| row.kind == kind && row.inputs_through.events > mark)
            .collect();
        looked.past_the_marks += past.len();
        past.iter()
            .flat_map(|row| row.records.iter().map(|e| e.id))
            .collect()
    };
    let queue_mark = if one_mark { marks.w } else { marks.queue };
    let past_w = past(LedgerKind::Session, marks.w);
    let past_r = past(LedgerKind::Queue, queue_mark);
    let open: Vec<&LedgerRow> = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Task && row.ended_at.is_none())
        .collect();
    looked.open_tasks = open.len();
    let open_runs: BTreeSet<String> = open
        .into_iter()
        .flat_map(crate::domain::ledger::window::runs_named)
        .collect();
    let mut seen = BTreeSet::new();
    let mut look = |kind: LedgerKind, key: &str, looked: &mut Looked| {
        if seen.insert((kind, key.to_owned())) {
            looked.owners += 1;
        }
        by_key.get(&(kind, key)).copied()
    };
    let mut kept = BTreeSet::new();
    for event in candidates {
        let keep = match owner(event) {
            Owner::Session => event.id <= marks.w || past_w.contains(&event.id),
            Owner::Queue => event.id <= queue_mark || past_r.contains(&event.id),
            Owner::Task if spare && event.id <= marks.core => true,
            Owner::Task => look(
                LedgerKind::Task,
                &event.task_id.unwrap().to_string(),
                &mut looked,
            )
            .is_some_and(|row| row.inputs_through.events >= event.id),
            Owner::Run => {
                let id = event.run_id.as_ref().unwrap().as_str();
                match look(LedgerKind::Run, id, &mut looked) {
                    Some(row) => row.inputs_through.events >= event.id,
                    None => open_runs.contains(id),
                }
            }
        };
        if keep {
            kept.insert(event.id);
        }
    }
    (kept, looked)
}

/// What the whole read of the store `rows` reads, counted as [`Reads`]
/// counts it (4678): every run, task and session row and every queue row
/// (a read of no time leaves none out, so no earlier record and no first
/// Execution), and the events of the runs on their way it folds of
/// `events`; with their sum.
fn whole_reads(rows: &[LedgerRow], events: &[RunEvent]) -> (Reads, usize, usize) {
    let queue = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Queue)
        .count();
    let reads = Reads {
        rows: rows.len() - queue,
        queue,
        ..Reads::default()
    };
    let on_their_way: BTreeSet<RunId> = runs_on_their_way(rows).into_iter().collect();
    let folded = events
        .iter()
        .filter(|e| e.run_id.as_ref().is_some_and(|r| on_their_way.contains(r)))
        .count();
    (reads, folded, reads.rows + reads.queue + folded)
}

/// What the small read of `candidates` events reads, counted alike: the
/// rows it looked up ([`Looked`]) and the candidate events themselves.
const fn small_reads(looked: Looked, candidates: usize) -> usize {
    looked.owners + looked.open_tasks + looked.past_the_marks + candidates
}

/// The events of the range: the last two days.
fn of_the_range(events: &[RunEvent]) -> Vec<RunEvent> {
    events
        .iter()
        .filter(|e| {
            crate::domain::stats::timestamp_millis(&e.created_at)
                .is_some_and(|at| at >= (NOW - 2 * DAY) * 1000)
        })
        .cloned()
        .collect()
}

#[test]
fn a_run_on_its_way_and_the_rows_past_w_and_r_are_seen_as_the_whole_read_sees_them() {
    let mut looks = Vec::new();
    for settled in [30, 3000] {
        let (rows, events, marks, running) = store_apart(settled);
        assert_eq!(runs_on_their_way(&rows), vec![running.clone()]);
        let whole = visible(&rows, &events);
        let range = of_the_range(&events);
        let expected: BTreeSet<EventId> = range
            .iter()
            .map(|e| e.id)
            .filter(|id| whole.contains(id))
            .collect();
        let (small, looked) = small_read_apart(&rows, &range, marks, false, false);
        // (iii-a) the run on its way: its events past R are seen, by its
        // task row, as the whole read folds them.
        let of_running: Vec<EventId> = range
            .iter()
            .filter(|e| e.run_id.as_ref() == Some(&running) && e.id > marks.core)
            .map(|e| e.id)
            .collect();
        assert_eq!(of_running.len(), 2);
        // Neither carries the task id of the task row naming the run.
        let task_ids: Vec<Option<i64>> = range
            .iter()
            .filter(|e| of_running.contains(&e.id))
            .map(|e| e.task_id.map(|task| task.as_i64()))
            .collect();
        assert_eq!(task_ids, vec![None, Some(999)]);
        assert!(
            of_running
                .iter()
                .all(|id| whole.contains(id) && small.contains(id))
        );
        // (iii-b) the session and queue events past W and R the pass did
        // not write fall out of the whole read, and out of the small one.
        let dropped: Vec<EventId> = range
            .iter()
            .filter(|e| matches!(owner(e), Owner::Session | Owner::Queue))
            .filter(|e| e.id > marks.queue && !whole.contains(&e.id))
            .map(|e| e.id)
            .collect();
        assert!(dropped.len() >= 5, "{dropped:?}");
        assert!(dropped.iter().all(|id| !small.contains(id)));
        assert_eq!(small, expected, "{settled}");
        // One mark for both: the queue's events between R and W are taken.
        let (merged, _) = small_read_apart(&rows, &range, marks, false, true);
        let extra: Vec<_> = merged.difference(&expected).collect();
        assert_eq!(extra.len(), 2, "{extra:?}");
        // The two rows the pass wrote past the marks hold what is seen past
        // them: planner 990001's close and mark m-a.
        assert_eq!(looked.past_the_marks, 2);
        let written_past: Vec<EventId> = range
            .iter()
            .filter(|e| {
                (e.kind == EventKind::SessionClosed && e.payload["planner_id"] == 990_001)
                    || e.payload["mark"] == "m-a"
            })
            .map(|e| e.id)
            .collect();
        assert_eq!(written_past.len(), 2);
        assert!(
            written_past
                .iter()
                .all(|id| *id > marks.w && whole.contains(id) && small.contains(id))
        );
        // Planner 990004's opening, between R of the queue and W, is in its
        // row by W. Judged by R of the queue, the session rows past it are
        // read too: the answer holds, the rows read grow.
        let lower = Marks {
            w: marks.queue,
            ..marks
        };
        let (by_r, more) = small_read_apart(&rows, &range, lower, false, false);
        assert_eq!(by_r, expected);
        assert!(more.past_the_marks > looked.past_the_marks, "{more:?}");
        // What each read reads, as 4678's `Reads` counts it.
        let (every, folded, whole_total) = whole_reads(&rows, &events);
        let small_total = small_reads(looked, range.len());
        eprintln!(
            "{settled}: range {}, visible {}, dropped past the marks {}, looked {looked:?}; small read {small_total}, whole read {every:?} + {folded} folded = {whole_total}",
            range.len(),
            expected.len(),
            dropped.len()
        );
        looks.push((looked, small_total, whole_total));
    }
    assert_eq!(looks[0].0, looks[1].0);
    assert_eq!(looks[0].1, looks[1].1);
    assert!(looks[1].2 > 50 * looks[0].2 / 2, "{looks:?}");
    assert!(looks[0].1 < looks[0].2, "{looks:?}");
}

#[test]
fn r_spares_the_task_rows_of_the_task_events_up_to_it_and_not_the_run_rows() {
    let mut looks = Vec::new();
    for settled in [30, 3000] {
        let (rows, events, marks, _) = store_apart(settled);
        let whole = visible(&rows, &events);
        let range = of_the_range(&events);
        let expected: BTreeSet<EventId> = range
            .iter()
            .map(|e| e.id)
            .filter(|id| whole.contains(id))
            .collect();
        let (looked_up, full) = small_read_apart(&rows, &range, marks, false, false);
        let (spared, less) = small_read_apart(&rows, &range, marks, true, false);
        assert_eq!(looked_up, expected);
        assert_eq!(spared, expected);
        assert!(less.owners < full.owners, "{less:?} {full:?}");
        let (_, _, whole_total) = whole_reads(&rows, &events);
        let (full_total, less_total) = (
            small_reads(full, range.len()),
            small_reads(less, range.len()),
        );
        eprintln!(
            "{settled}: looked {full:?} = {full_total}, spared by R {less:?} = {less_total}, whole read {whole_total}"
        );
        looks.push((less, less_total, whole_total));
    }
    assert_eq!(looks[0].0, looks[1].0);
    assert_eq!(looks[0].1, looks[1].1);
    assert!(looks[1].2 > 50 * looks[0].2 / 2, "{looks:?}");
    // A run's event at or before R is not spared: run 12's task ended and
    // its row was not written, so the whole read sees none of its events.
    let (rows, events, marks, _) = store_apart(30);
    let whole = visible(&rows, &events);
    let unseen = events
        .iter()
        .filter(|e| e.run_id.as_ref().is_some_and(|r| r.as_str() == run(12)))
        .filter(|e| e.id <= marks.core)
        .count();
    assert!(unseen > 0);
    assert!(
        events
            .iter()
            .filter(|e| e.run_id.as_ref().is_some_and(|r| r.as_str() == run(12)))
            .all(|e| !whole.contains(&e.id))
    );
}

/// Whether a bounded read keeps `event`'s row for it: a time at or before
/// `end` (its `created_at`, or its payload's `at`, a cut's time, or one of
/// no time the read can tell), or an Execution of any time (its
/// `tokens_source`): [`LedgerWindow::execution_tokens`] reads every row's,
/// whatever rows the window chose, and its first per actor and provider
/// (`recorded_from`) whenever it ended.
fn holds_for_the_window(event: &RunEvent, end: i64) -> bool {
    let millis = crate::domain::stats::timestamp_millis;
    let at = event
        .payload
        .get("at")
        .map(|at| at.as_str().and_then(millis));
    millis(&event.created_at).is_none_or(|created| created <= end)
        || at.is_some_and(|at| at.is_none_or(|at| at <= end))
        || event.payload.get("tokens_source").is_some()
}

/// Whether a read bounded by U_E `bound` leaves `row` out: its first input
/// past `bound` and, with `end` (E), none of its records held for the
/// window ([`holds_for_the_window`], Spike 4670 (vi)'s bound and the
/// Executions); without, every row past `bound`.
fn left_out(row: &LedgerRow, bound: EventId, end: Option<i64>) -> bool {
    row.inputs_from > bound
        && end.is_none_or(|end| !row.records.iter().any(|e| holds_for_the_window(e, end)))
}

/// The window `query` asks of the rows of `rows` a read bounded by `bound`
/// keeps ([`left_out`]) and of the runs on their way, folded of `events`;
/// and how many rows it left out.
fn bounded(
    rows: &[LedgerRow],
    events: &[RunEvent],
    bound: EventId,
    end: Option<i64>,
    query: &WindowQuery,
) -> (LedgerWindow, usize) {
    let mut kept: Vec<LedgerRow> = rows
        .iter()
        .filter(|row| !left_out(row, bound, end))
        .cloned()
        .collect();
    let left = rows.len() - kept.len();
    for run in runs_on_their_way(&kept) {
        let of_run: Vec<RunEvent> = events
            .iter()
            .filter(|e| e.run_id.as_ref() == Some(&run))
            .cloned()
            .collect();
        kept.extend(run_on_its_way(&run, &of_run, NOW * 1000));
    }
    (select(kept, query), left)
}

/// K of Spike 4670 (vi): the events a search for a time's id walks before
/// it gives up and the read falls back to no bound.
const K: usize = 64;

/// U_E as a search of at most [`K`] events down the ids from the latest:
/// the id of the first at or before `end`, none past K; and the events it
/// walked.
fn u_e_capped(events: &[RunEvent], end: i64) -> (Option<EventId>, usize) {
    let mut walked = 0;
    for event in events.iter().rev().take(K) {
        walked += 1;
        if crate::domain::stats::timestamp_millis(&event.created_at).is_some_and(|at| at <= end) {
            return (Some(event.id), walked);
        }
    }
    (None, walked)
}

/// `store_apart(settled)` with, written after E: a cut of the inbox's
/// tokens whose time is in the window (recorded 5 minutes after E for the
/// 10 minutes before it), a planner's turn on codex (the first Execution of
/// that actor and provider), and `after` marks (each a queue row of its
/// own); then, if `late`, one event of a time before E written last (a
/// clock turned back), past the marks and in no row.
fn store_after(
    settled: usize,
    after: usize,
    end: i64,
    late: bool,
) -> (Vec<LedgerRow>, Vec<RunEvent>) {
    let (mut rows, mut events, _, _) = store_apart(settled);
    let first = events.len();
    let push = |events: &mut Vec<RunEvent>, kind: &str, payload: Value, at: i64| {
        let id = EventId::new(events.last().unwrap().id.as_i64() + 1);
        events.push(RunEvent {
            id,
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: crate::domain::marks::utc_text(at),
            actor: None,
        });
    };
    let tokens = json!({"input": 40, "output": 4, "cache_read": 0, "cache_creation": 0});
    push(
        &mut events,
        "session_tokens",
        json!({"kind": "inbox", "at": crate::domain::marks::utc_text(end - 10 * 60_000),
               "final": false, "tokens": tokens, "tokens_source": "transcript",
               "tokens_by_model": []}),
        end + 5 * 60_000,
    );
    push(
        &mut events,
        "turn_finished",
        json!({"planner_id": 990_009, "provider": "codex", "tokens": tokens,
               "tokens_source": "model_usage", "tokens_by_model": []}),
        end + 6 * 60_000,
    );
    let span = NOW * 1000 - end - 1;
    for n in 0..after {
        let at = end + 1 + i64::try_from(n).unwrap() * span / i64::try_from(after).unwrap();
        push(
            &mut events,
            "mark_added",
            json!({"mark": format!("after-{n}")}),
            at,
        );
    }
    let written = events.len();
    if late {
        push(&mut events, "inbox_watcher_absent", json!({}), end - 1000);
    }
    // A pass wrote the rows of the events after E (and only those).
    let ids: BTreeSet<EventId> = events[first..written].iter().map(|e| e.id).collect();
    let further = build(&events[..written], NOW * 1000, "2030-01-01T00:00:00.000Z");
    for row in further
        .into_iter()
        .filter(|row| row.records.iter().any(|e| ids.contains(&e.id)))
    {
        rows.retain(|held| !(held.kind == row.kind && held.key == row.key));
        rows.push(row);
    }
    (rows, events)
}

/// The rows a read bounded by `bound` (none: no bound) takes that start
/// past the window's end `upto`, as [`bounded`] with `end` keeps them.
fn read_past_the_end(rows: &[LedgerRow], bound: Option<EventId>, end: i64, upto: EventId) -> usize {
    rows.iter()
        .filter(|row| row.inputs_from > upto)
        .filter(|row| bound.is_none_or(|bound| !left_out(row, bound, Some(end))))
        .count()
}

#[test]
fn u_e_of_every_event_and_of_the_visible_ones_and_searched_by_k_keep_what_the_whole_read_counts() {
    let sources = StatsSources {
        config_file: &|| Ok(None),
        conflicts_file: &|| Ok(None),
        areas: None,
        utc_offset_secs: 0,
        counts_checks: true,
        rust_toolchain: false,
    };
    let end = NOW * 1000 - 85 * 60_000;
    let since = Cursor::Time((NOW - 2 * DAY) * 1000);
    let query = StatsQuery {
        since: Some(since),
        until: Some(Cursor::Time(end)),
        full: true,
        ..StatsQuery::default()
    };
    let window_query = WindowQuery {
        since: Some(since),
        until: Some(Cursor::Time(end)),
        now_ms: now_ms(NOW),
        ..WindowQuery::default()
    };
    // `stats --since --until --full`, and the window's Executions of every
    // record ([`LedgerWindow::execution_tokens`]: every row's, whatever rows
    // the window chose, a cut by its payload's time).
    let answer = |window: &LedgerWindow| {
        json!({
            "stats": serde_json::to_value(stats_of_window(window, NOW, &query, &sources).unwrap())
                .unwrap(),
            "execution_tokens": serde_json::to_value(window.execution_tokens()).unwrap(),
        })
    };
    let misses = |window: &LedgerWindow, expected: &Value| {
        let mut out = Vec::new();
        differences(&answer(window), expected, "stats", &mut out);
        out
    };
    let mut two = BTreeMap::new();
    let mut searched = BTreeMap::new();
    // The settled history before S (30, 3000 units) and the history after
    // E (30, 3000 events), a clock turned back after the long one.
    for (settled, after, late) in [
        (30, 30, false),
        (3000, 30, false),
        (30, 3000, false),
        (30, 3000, true),
    ] {
        let (rows, events) = store_after(settled, after, end, late);
        let whole = visible(&rows, &events);
        let by_time = |e: &&RunEvent| {
            crate::domain::stats::timestamp_millis(&e.created_at).is_some_and(|at| at <= end)
        };
        let of_all = events.iter().filter(by_time).map(|e| e.id).max().unwrap();
        let of_seen = events
            .iter()
            .filter(by_time)
            .filter(|e| whole.contains(&e.id))
            .map(|e| e.id)
            .max()
            .unwrap();
        assert!(of_all > of_seen, "an invisible event near E");
        let (every, _) = bounded(&rows, &events, EventId::new(i64::MAX), None, &window_query);
        let expected = answer(&every);
        // The two bounds the description names, of every event and of the
        // visible ones: by the first input alone they lose the cut in the
        // window and the first Execution of codex; kept as above, both
        // answer as the whole read.
        if after == 30 {
            for (name, bound) in [("every", of_all), ("visible", of_seen)] {
                let (plain, _) = bounded(&rows, &events, bound, None, &window_query);
                let lost = misses(&plain, &expected);
                assert!(lost.iter().any(|miss| miss.contains("inbox")), "{lost:?}");
                let planner = |value: &Value| {
                    value["execution_tokens"]["by_actor"]["runtime_planner"]["by_provider"]
                        .get("codex")
                        .is_some()
                };
                assert!(planner(&expected) && !planner(&answer(&plain)));
                let (kept, reads) = bounded(&rows, &events, bound, Some(end), &window_query);
                assert_eq!(misses(&kept, &expected), Vec::<String>::new(), "{name}");
                two.insert(
                    (settled, name),
                    (
                        reads,
                        read_past_the_end(&rows, Some(bound), end, of_seen),
                        read_past_the_end(&rows, None, end, of_seen),
                    ),
                );
            }
        }
        let (capped, walked) = u_e_capped(&events, end);
        assert!(walked <= K);
        if let Some(bound) = capped {
            assert!(bound >= of_seen);
        }
        let (window, reads) = bounded(
            &rows,
            &events,
            capped.unwrap_or(EventId::new(i64::MAX)),
            Some(end),
            &window_query,
        );
        assert_eq!(
            misses(&window, &expected),
            Vec::<String>::new(),
            "{settled} {after} {late}"
        );
        searched.insert(
            (settled, after, late),
            (
                capped.is_some(),
                walked,
                reads,
                read_past_the_end(&rows, capped, end, of_seen),
                read_past_the_end(&rows, None, end, of_seen),
            ),
        );
    }
    eprintln!("(reads, rows past the end read, of the whole read): {two:#?}");
    eprintln!("(found, walked, reads, rows past the end read, of the whole read): {searched:#?}");
    // Before S the history does not count, for either bound and for K.
    for name in ["every", "visible"] {
        assert_eq!(two[&(30, name)], two[&(3000, name)]);
    }
    assert_eq!(two[&(30, "every")], two[&(30, "visible")]);
    assert_eq!(searched[&(30, 30, false)], searched[&(3000, 30, false)]);
    let near = searched[&(30, 30, false)];
    assert!(near.0 && near.3 < near.4, "{near:?}");
    // After E it does: past K the search gives up and the read takes every
    // row past the end, as the whole read does (4670 (vi)'s fallback); a
    // clock turned back gives a bound past them all.
    let long = searched[&(30, 3000, false)];
    assert!(!long.0 && long.1 == K && long.3 == long.4 && long.4 > 3000);
    let late = searched[&(30, 3000, true)];
    assert!(late.0 && late.1 == 1 && late.3 == late.4);
    // By the first input alone, a bound below a row of the window (task
    // 70's, started in it) changes the answer: `select` itself drops only
    // rows starting past the end. Kept by the times of its records, that row
    // stays and the answer holds.
    let (rows, events, _, _) = store_apart(30);
    let started = rows
        .iter()
        .find(|row| row.kind == LedgerKind::Task && row.key == "70")
        .unwrap()
        .inputs_from;
    let (every, _) = bounded(&rows, &events, EventId::new(i64::MAX), None, &window_query);
    let low = EventId::new(started.as_i64() - 1);
    let (plain, _) = bounded(&rows, &events, low, None, &window_query);
    assert!(
        !misses(&plain, &answer(&every)).is_empty(),
        "a bound too low misses rows of the window"
    );
    let (kept, _) = bounded(&rows, &events, low, Some(end), &window_query);
    assert_eq!(misses(&kept, &answer(&every)), Vec::<String>::new());
}

// Spike 4755: (i-a) to (i-d) of the earlier records.

/// The kpi answers (the four questions) and `stats --since --full`'s of the
/// narrowed read of every part in `shape` that differ from those of every
/// row, and what the read of the day's question returned.
fn shape_misses(rows: &[LedgerRow], shape: Shape) -> (Vec<String>, Reads) {
    let whole = select(rows.to_vec(), &whole_query());
    let mut misses = Vec::new();
    let mut reads = Reads::default();
    for (n, query) in kpi_questions().iter().enumerate() {
        let expected = kpi_value(
            &whole,
            &KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            },
        );
        let (window, read) = narrowed_shaped(rows, &time_query(query), &PARTS, shape);
        if n == 0 {
            reads = read;
        }
        differences(
            &kpi_value(&window, query),
            &expected,
            &format!("kpi[{n}]"),
            &mut misses,
        );
    }
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
    let base = WindowQuery {
        since: Some(since),
        now_ms: now_ms(NOW),
        ..WindowQuery::default()
    };
    let expected = serde_json::to_value(
        stats_of_window(&select(rows.to_vec(), &base), NOW, &query, &sources).unwrap(),
    )
    .unwrap();
    let from = WindowQuery {
        read_from_ms: Some((NOW - 2 * DAY) * 1000),
        ..base
    };
    let (window, _) = narrowed_shaped(rows, &from, &PARTS, shape);
    let got =
        serde_json::to_value(stats_of_window(&window, NOW, &query, &sources).unwrap()).unwrap();
    differences(&got, &expected, "stats", &mut misses);
    (misses, reads)
}

/// What a read returned of the run, task and session rows, the earlier
/// records and the first Executions: the queue rows are (ii)'s, read whole
/// here.
fn earlier_reads(reads: Reads) -> (usize, usize, usize) {
    (reads.rows, reads.earlier, reads.executions)
}

/// What the reads of every row return of `rows`, in the units of
/// [`Reads`], for the day's question: the read from a time of today (no
/// part narrowed, [`earlier_today`]) and the read of every row (each row
/// whole, so no earlier record), and the records its run, task and
/// session rows hold.
fn full_reads(rows: &[LedgerRow]) -> (Reads, Reads, usize) {
    let (_, today) = narrowed_shaped(
        rows,
        &time_query(&kpi_questions()[0]),
        &[],
        Shape::default(),
    );
    let (queue, others): (Vec<&LedgerRow>, Vec<&LedgerRow>) =
        rows.iter().partition(|row| row.kind == LedgerKind::Queue);
    let every = Reads {
        rows: others.len(),
        earlier: 0,
        executions: 0,
        queue: queue.len(),
    };
    let records = others.iter().map(|row| row.records.len()).sum();
    (today, every, records)
}

/// Print the reads of every row of `rows` beside `shaped`'s, and keep
/// them in `fulls`.
fn note_full(
    label: &str,
    rows: &[LedgerRow],
    shaped: Reads,
    fulls: &mut Vec<(Reads, Reads, usize)>,
) {
    let full = full_reads(rows);
    eprintln!(
        "{label}: shaped {shaped:?}; today {:?}; every row {:?} holding {} records",
        full.0, full.1, full.2
    );
    fulls.push(full);
}

/// The shape Spike 4678 read: the proposals' tasks by their records.
const OF_4678: Shape = Shape {
    members: true,
    session_prefix: false,
    actor_planners: false,
    member_ends: false,
    paired: false,
    goal_members: false,
};

const BEFORE: i64 = 149 * DAY;
const NEAR: i64 = (READ_AT_DAY - 1) * DAY;

/// (i-c): run 70 landed long ago with a session closed by a close of the
/// old form (no `opened_event_id`), run 71 with two sessions and two
/// closes of the second (the first never closed): both open at their
/// row's end as the whole read pairs them, closed by a count of each.
fn add_old_closes(log: &mut Log) {
    let session = |tag: &str| json!({"kind": "review", "provider": "claude", "tag": tag});
    let close = |closes: &str| json!({"kind": "review", "provider": "claude", "closes": closes});
    log.landed(70, &run(70), BEFORE)
        .add(
            Some(70),
            Some(&run(70)),
            "session_opened",
            session("70"),
            BEFORE + 1,
        )
        .add(
            Some(70),
            Some(&run(70)),
            "session_closed",
            close(""),
            BEFORE + 2,
        );
    log.landed(71, &run(71), BEFORE + 5000)
        .add(
            Some(71),
            Some(&run(71)),
            "session_opened",
            session("71a"),
            BEFORE + 5001,
        )
        .add(
            Some(71),
            Some(&run(71)),
            "session_opened",
            session("71b"),
            BEFORE + 5002,
        )
        .add(
            Some(71),
            Some(&run(71)),
            "session_closed",
            close("71b"),
            BEFORE + 5003,
        )
        .add(
            Some(71),
            Some(&run(71)),
            "session_closed",
            close("71b"),
            BEFORE + 5004,
        );
}

/// The ids [`add_old_closes`]' closes name, put back over those
/// `numbered` gives: none for the old form, the tagged open's else.
fn fix_old_closes(log: &mut Log) {
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
}

#[test]
fn a_run_s_session_closed_by_an_old_close_or_twice_stays_open_as_the_whole_read_pairs_it() {
    let mut reads = Vec::new();
    let mut fulls = Vec::new();
    for settled in [30, 3000] {
        let rows = rows_of(&history_with(settled, add_old_closes, fix_old_closes));
        let of = |n: u8| {
            rows.iter()
                .find(|row| row.kind == LedgerKind::Run && row.key == run(n))
                .unwrap()
        };
        // Paired, both runs leave a session open; counted, neither does.
        for n in [70, 71] {
            assert!(open_at_end(of(n), true), "run {n} paired");
            assert!(!open_at_end(of(n), false), "run {n} counted");
        }
        let counted = Shape {
            paired: false,
            ..SHAPED
        };
        let (paired_misses, paired_reads) = shape_misses(&rows, SHAPED);
        eprintln!("{settled}: paired {paired_reads:?} {paired_misses:?}");
        assert_eq!(paired_misses, Vec::<String>::new());
        if settled == 30 {
            let (counted_misses, counted_reads) = shape_misses(&rows, counted);
            eprintln!("{settled}: counted {counted_reads:?} {counted_misses:?}");
            assert!(
                !counted_misses.is_empty(),
                "the count misses the open sessions"
            );
        }
        note_full(&format!("(i-c) {settled}"), &rows, paired_reads, &mut fulls);
        reads.push(paired_reads);
    }
    assert_eq!(earlier_reads(reads[0]), earlier_reads(reads[1]));
    assert!(
        fulls[1].0.earlier > 50 * fulls[0].0.earlier / 2,
        "{fulls:?}"
    );
    assert!(fulls[1].1.rows > 50 * fulls[0].1.rows / 2, "{fulls:?}");
}

/// (i-a): planner 70's session (headless) opened and closed long before
/// the range; in the range, a `planner_question` it opened (its actor
/// `planner:70`), one of no task (the queue's row) and one on task 62.
fn add_planner_question(log: &mut Log) {
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
    for (ask, task) in [(7070, None), (7071, Some(62))] {
        log.add(
            task,
            None,
            "ask_opened",
            json!({"ask_id": ask, "kind": "planner_question"}),
            NEAR + 2000,
        );
        log.events.last_mut().unwrap().actor = Some(crate::domain::EventActor {
            role: "planner".into(),
            id: "planner:70".into(),
            requested_by: None,
        });
    }
}

#[test]
fn a_planner_s_session_row_is_read_by_the_prefix_of_its_key_and_the_planner_an_ask_s_actor_names() {
    let mut reads = Vec::new();
    let mut fulls = Vec::new();
    for settled in [30, 3000] {
        let rows = rows_of(&history_with(settled, add_planner_question, |_| {}));
        let prefix_alone = Shape {
            actor_planners: false,
            ..SHAPED
        };
        let (shaped, shaped_reads) = shape_misses(&rows, SHAPED);
        eprintln!("{settled}: prefix and actors {shaped_reads:?} {shaped:?}");
        assert_eq!(shaped, Vec::<String>::new());
        if settled == 30 {
            // Without the actor no read names planner 70: its route is
            // `unknown` instead of `headless`.
            for shape in [OF_4678, prefix_alone] {
                let (misses, _) = shape_misses(&rows, shape);
                eprintln!("{shape:?}: {} misses", misses.len());
                assert!(
                    misses.iter().any(|miss| miss.contains("planner_routes")),
                    "{misses:?}"
                );
            }
        }
        note_full(&format!("(i-a) {settled}"), &rows, shaped_reads, &mut fulls);
        reads.push(shaped_reads);
    }
    assert_eq!(earlier_reads(reads[0]), earlier_reads(reads[1]));
    assert!(
        fulls[1].0.earlier > 50 * fulls[0].0.earlier / 2,
        "{fulls:?}"
    );
    assert!(fulls[1].1.rows > 50 * fulls[0].1.rows / 2, "{fulls:?}");
}

/// (i-b): `count` priority changes of task 66 (of proposal 8, completed
/// before the range) before the range.
fn add_priorities(count: usize) -> impl FnOnce(&mut Log) {
    move |log: &mut Log| {
        for _ in 0..count {
            log.add(
                Some(66),
                None,
                "task_priority_changed",
                json!({"priority": "high"}),
                150 * DAY + 40_005,
            );
        }
    }
}

#[test]
fn a_member_task_s_ends_alone_answer_as_its_whole_history_whatever_its_own_history() {
    let whole_records = Shape {
        member_ends: false,
        ..SHAPED
    };
    let mut ends = Vec::new();
    let mut fulls = Vec::new();
    let mut records = Vec::new();
    for count in [30, 3000] {
        let rows = rows_of(&history_with(30, add_priorities(count), |_| {}));
        let (misses, read) = shape_misses(&rows, SHAPED);
        let (whole_misses, whole_read) = shape_misses(&rows, whole_records);
        eprintln!(
            "{count} changes of task 66: ends {read:?} {misses:?}; records {whole_read:?} {whole_misses:?}"
        );
        assert_eq!(misses, Vec::<String>::new());
        assert_eq!(whole_misses, Vec::<String>::new());
        note_full(&format!("(i-b) {count}"), &rows, read, &mut fulls);
        ends.push(read);
        records.push(whole_read);
    }
    assert_eq!(earlier_reads(ends[0]), earlier_reads(ends[1]));
    assert!(
        records[1].earlier >= records[0].earlier + 2970,
        "{records:?}"
    );
    // Today's read and every row's records grow with task 66's history.
    assert!(fulls[1].0.earlier >= fulls[0].0.earlier + 2970, "{fulls:?}");
    assert!(fulls[1].2 >= fulls[0].2 + 2970, "{fulls:?}");
}

/// (i-d): goal 80 in proposal 9 (its `goal_submitted`, a finding the
/// planner proposed linking it); its task 81 completes in the range, task
/// 82 completed and task 83 was canceled before it. Task 83's creation
/// names plan review 9, which the range names, so a read of what the
/// range names takes that record of its row and not its end.
fn add_goal_proposal(log: &mut Log) {
    let before = 150 * DAY + 60_000;
    let mut proposed = crate::domain::ledger::testkit::finding_record("proposed");
    proposed["finding_id"] = json!(51);
    proposed[crate::domain::ledger::findings::VALUES]["id"] = json!(51);
    let mut recorded = proposed.clone();
    recorded[crate::domain::ledger::findings::VALUES]["status"] = json!("open");
    proposed["from"] = json!("open");
    proposed["to"] = json!("proposed");
    proposed["proposal_id"] = json!(9);
    proposed["by"] = json!("planner");
    log.add(None, None, "finding_recorded", recorded, before)
        .add(None, None, "finding_status_changed", proposed, before + 1)
        .add(
            None,
            None,
            "goal_submitted",
            json!({"proposal_id": 9}),
            before + 2,
        );
    log.events.last_mut().unwrap().goal_id = Some(crate::domain::GoalId::new(80));
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
}

#[test]
fn a_proposal_s_goal_tasks_are_read_by_the_goal_submitted_naming_it() {
    let mut reads = Vec::new();
    let mut fulls = Vec::new();
    for settled in [30, 3000] {
        let rows = rows_of(&history_with(settled, add_goal_proposal, |_| {}));
        let whole = select(rows.clone(), &whole_query());
        let query = kpi_questions()[0].clone();
        let running = |window: &LedgerWindow, query: &KpiQuery| {
            kpi_value(window, query)["periods"][0]["kpis"]["improvement_proposals"].clone()
        };
        let expected = running(
            &whole,
            &KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            },
        );
        let no_goal = Shape {
            goal_members: false,
            ..SHAPED
        };
        let (shaped, shaped_reads) = shape_misses(&rows, SHAPED);
        let (window, _) = narrowed_shaped(&rows, &time_query(&query), &PARTS, SHAPED);
        eprintln!(
            "{settled}: whole {expected}, shaped {}; with {shaped_reads:?} {shaped:?}",
            running(&window, &query)
        );
        assert_eq!(shaped, Vec::<String>::new());
        if settled == 30 {
            for shape in [OF_4678, no_goal] {
                let (misses, _) = shape_misses(&rows, shape);
                eprintln!("{shape:?}: {misses:?}");
                assert!(
                    misses
                        .iter()
                        .any(|miss| miss.contains("improvement_proposals")),
                    "{misses:?}"
                );
            }
        }
        note_full(&format!("(i-d) {settled}"), &rows, shaped_reads, &mut fulls);
        reads.push(shaped_reads);
    }
    assert_eq!(earlier_reads(reads[0]), earlier_reads(reads[1]));
    assert!(
        fulls[1].0.earlier > 50 * fulls[0].0.earlier / 2,
        "{fulls:?}"
    );
    assert!(fulls[1].1.rows > 50 * fulls[0].1.rows / 2, "{fulls:?}");
}

#[test]
fn every_shape_returns_as_many_records_whatever_the_settled_history() {
    let mut reads = Vec::new();
    let mut fulls = Vec::new();
    for settled in [30, 3000] {
        let log = history_with(
            settled,
            |log| {
                add_old_closes(log);
                add_planner_question(log);
                add_goal_proposal(log);
                add_priorities(30)(log);
            },
            fix_old_closes,
        );
        let rows = rows_of(&log);
        let (misses, read) = shape_misses(&rows, SHAPED);
        eprintln!("{settled}: {read:?} {misses:?}");
        assert_eq!(misses, Vec::<String>::new());
        note_full(&format!("all {settled}"), &rows, read, &mut fulls);
        reads.push(read);
    }
    assert_eq!(earlier_reads(reads[0]), earlier_reads(reads[1]));
    assert!(
        fulls[1].0.earlier > 50 * fulls[0].0.earlier / 2,
        "{fulls:?}"
    );
    assert!(fulls[1].1.rows > 50 * fulls[0].1.rows / 2, "{fulls:?}");
}

// (ii) the queue rows' kinds Spike 4678 left unresolved (Spike 4756).

/// The supervisor that lives across the range: started after the settled
/// units (whose own starts end every earlier life), alive each day.
const ACROSS: &str = "s3";

/// The days of the cases of (ii) before the range: after the settled
/// units (to day 150), before the week's start.
const QUEUE_BEFORE: i64 = 160 * DAY;

/// The commit of the settled unit `k` on main.
fn unit_commit(k: usize) -> String {
    format!("{k:040x}")
}

/// The changes the settled unit `k`'s commit makes: its file, the
/// previous unit's file deleted every second unit and renamed every
/// tenth, so that the paths main has grow and shrink with the history.
fn unit_changes(k: usize) -> Value {
    let mut changes = vec![json!({"path": format!("src/u{k}.rs")})];
    if k % 2 == 1 {
        changes.push(json!({"path": format!("src/u{}.rs", k - 1), "deleted": true}));
    } else if k.is_multiple_of(10) && k > 0 {
        changes.push(
            json!({"path": format!("src/moved{k}.rs"), "from": format!("src/u{}.rs", k - 1)}),
        );
    }
    Value::Array(changes)
}

/// The queue's events of (ii) in each of `settled` units, each of its own
/// supervisor started and stopped in the unit, and the cases across the
/// range's start once.
fn add_queue_units(settled: usize) -> impl FnOnce(&mut Log) {
    move |log: &mut Log| {
        let span = 139 * DAY;
        log.add(
            None,
            None,
            "main_paths_recorded",
            json!({"head": "base", "since": T0 + 10 * DAY, "part": 0, "parts": 1,
                   "paths": ["src/base.rs", "src/lib.rs"]}),
            10 * DAY + 10,
        );
        for k in 0..settled {
            let t = 11 * DAY + i64::try_from(k).unwrap() * span / i64::try_from(settled).unwrap();
            let task = 100_000 + i64::try_from(k).unwrap();
            let u = format!("u{k}");
            let at = |offset: i64| T0 + t + offset;
            let held = json!({"supervisor": u, "reason": "disk"});
            let full = json!({"supervisor": u, "class": "heavy"});
            log.add(
                None,
                None,
                "forecast_recorded",
                json!({"at_secs": at(-5), "method": 1, "goals": [],
                       "tasks": [{"id": task, "p50_secs": 3600, "p90_secs": 7200}]}),
                t - 5,
            )
            .add(
                None,
                None,
                "supervisor_started",
                json!({"supervisor": u, "parallel": 2}),
                t + 1000,
            )
            .add(None, None, "slots_full_started", full.clone(), t + 1010)
            .add(None, None, "slots_full_ended", full, t + 1020)
            .add(None, None, "claim_held", held.clone(), t + 1030)
            .add(None, None, "claim_resumed", held.clone(), t + 1040)
            .add(None, None, "landing_held", held.clone(), t + 1050)
            .add(None, None, "landing_resumed", held, t + 1060)
            .add(
                None,
                None,
                "supervisor_alive",
                json!({"supervisor": u}),
                t + 1070,
            )
            .add(
                None,
                None,
                "candidates_sampled",
                json!({"supervisor": u, "candidates": 1, "free_slots": 1, "ready": 1,
                       "claims_held": false}),
                t + 1080,
            )
            .add(
                None,
                None,
                "mark_recorded",
                json!({"label": u, "note": null, "at": null, "by": "person"}),
                t + 1100,
            )
            .add(
                None,
                None,
                "mark_retracted",
                json!({"mark": 0, "kind": "mark_recorded", "label": u, "by": "person"}),
                t + 1110,
            );
            let goal = 600_000 + i64::try_from(k).unwrap();
            add_goal(log, goal, "goal_created", "normal", t + 1200);
            add_goal(log, goal, "goal_updated", "high", t + 1210);
            log.add(
                None,
                None,
                "update_failed",
                json!({"stage": "e2e", "failed_tests": ["e2e::t"], "secs": 10}),
                t + 1300,
            )
            .add(
                None,
                None,
                "backend_call_failed",
                json!({"op": "claim", "retried": true}),
                t + 1310,
            )
            .add(
                None,
                None,
                "ask_opened",
                json!({"ask_id": 800_000 + task, "kind": "planner_question"}),
                t + 1320,
            )
            .add(
                None,
                None,
                "ask_answered",
                json!({"ask_id": 800_000 + task}),
                t + 1330,
            )
            .add(
                None,
                None,
                "auto_repaired",
                json!({"kind": "stale_lock", "supervisor": u}),
                t + 1340,
            )
            .add(
                None,
                None,
                "stall_config_loaded",
                json!({"turn_limit_secs": 600 + k}),
                t + 1350,
            )
            .add(
                None,
                None,
                "main_commits_recorded",
                json!({"after": k.checked_sub(1).map(unit_commit),
                       "commits": [{"sha": unit_commit(k), "at": at(1400),
                                    "changes": unit_changes(k),
                                    "reverts": (k % 2 == 1).then(|| unit_commit(k - 1))}]}),
                t + 1400,
            )
            .add(
                None,
                None,
                "main_observed",
                json!({"head": unit_commit(k), "at": at(1410), "since": T0 + 10 * DAY,
                       "reverts": true}),
                t + 1410,
            )
            .add(
                None,
                None,
                "ci_checked",
                json!({"branch": "main"}),
                t + 1420,
            )
            .add(
                None,
                None,
                "ci_turned_red",
                json!({"sha": unit_commit(k), "branch": "main"}),
                t + 1430,
            )
            .add(
                None,
                None,
                "supervisor_stopped",
                json!({"supervisor": u}),
                t + 1500,
            );
        }
        add_queue_cases(log, settled);
    }
}

/// A goal's record of `kind` with its priority.
fn add_goal(log: &mut Log, goal: i64, kind: &str, priority: &str, secs: i64) {
    let record = json!({"title": format!("goal {goal}"), "status": "open", "priority": priority});
    let payload = if kind == "goal_created" {
        json!({"goal": record})
    } else {
        json!({"new": record})
    };
    log.add(None, None, kind, payload, secs);
    log.events.last_mut().unwrap().goal_id = Some(crate::domain::GoalId::new(goal));
}

/// The cases of (ii) across the range's start: the supervisor across it
/// and its samples, waits, marks, asks of no run, a goal of a task of the
/// range made long before, main's record and what befell a landing.
fn add_queue_cases(log: &mut Log, settled: usize) {
    let near = (READ_AT_DAY - 1) * DAY;
    let s = |extra: Value| {
        let mut payload = json!({"supervisor": ACROSS});
        for (key, value) in extra.as_object().unwrap() {
            payload[key] = value.clone();
        }
        payload
    };
    log.add(
        None,
        None,
        "supervisor_started",
        s(json!({"parallel": 4})),
        151 * DAY,
    );
    for day in 151..=READ_AT_DAY {
        log.add(None, None, "supervisor_alive", s(json!({})), day * DAY + 50);
    }
    let sample = |candidates: i64| {
        s(json!({"candidates": candidates, "free_slots": 1, "ready": 1, "claims_held": false}))
    };
    log.add(
        None,
        None,
        "candidates_sampled",
        sample(2),
        QUEUE_BEFORE + 10,
    )
    .add(None, None, "candidates_sampled", sample(0), near + 2000)
    .add(
        None,
        None,
        "slots_full_started",
        s(json!({"class": "heavy"})),
        158 * DAY,
    )
    .add(
        None,
        None,
        "slots_full_ended",
        s(json!({"class": "heavy"})),
        near + 2100,
    )
    .add(
        None,
        None,
        "claim_held",
        s(json!({"reason": "disk"})),
        159 * DAY,
    )
    .add(
        None,
        None,
        "landing_held",
        s(json!({"reason": "disk"})),
        159 * DAY + 500,
    )
    .add(
        None,
        None,
        "landing_resumed",
        s(json!({"reason": "disk"})),
        near + 2200,
    )
    .add(
        None,
        None,
        "stall_config_loaded",
        json!({"turn_limit_secs": 900}),
        152 * DAY,
    );
    for (label, at, retracted) in [
        ("across", 156 * DAY + 100, near + 2300),
        ("gone", 155 * DAY + 100, 157 * DAY + 100),
    ] {
        log.add(
            None,
            None,
            "mark_recorded",
            json!({"label": label, "note": null, "at": null, "by": "person"}),
            at,
        )
        .add(
            None,
            None,
            "mark_retracted",
            json!({"mark": 0, "kind": "mark_recorded", "label": label, "by": "person"}),
            retracted,
        );
    }
    log.add(
        None,
        None,
        "mark_recorded",
        json!({"label": "kept", "note": null, "at": null, "by": "person"}),
        159 * DAY + 100,
    );
    // Asks of no run: one answered in the range, one never.
    log.add(
        None,
        None,
        "ask_opened",
        json!({"ask_id": 8001, "kind": "planner_question"}),
        157 * DAY + 200,
    )
    .add(
        None,
        None,
        "ask_answered",
        json!({"ask_id": 8001}),
        near + 2400,
    )
    .add(
        None,
        None,
        "ask_opened",
        json!({"ask_id": 8002, "kind": "planner_question"}),
        157 * DAY + 300,
    )
    .add(
        None,
        None,
        "ask_opened",
        json!({"ask_id": 8003, "kind": "planner_question"}),
        156 * DAY + 300,
    )
    .add(
        None,
        None,
        "update_failed",
        json!({"stage": "e2e", "failed_tests": ["e2e::r"], "secs": 10}),
        near + 2800,
    );
    // Goal 700 made long before, changed before the range; its task 67
    // made and landed in the range.
    add_goal(log, 700, "goal_created", "normal", 5 * DAY);
    add_goal(log, 700, "goal_updated", "urgent", 120 * DAY);
    log.add(
        Some(67),
        None,
        "task_created",
        json!({"status": "ready", "goal_id": 700, "title": "t67"}),
        near + 2500,
    );
    log.landed(67, &run(67), near + 2510);
    // Main: run 60 (its row in the range by its ask) landed `c60` before
    // the range; the range records `r65` (run 65's), records the last
    // unit's commit again, and reverts `c60`; CI turns red on `r65` and on
    // `c60`. Git could not be read for a day before the range.
    let last = settled.checked_sub(1).map(unit_commit);
    log.add(
        None,
        None,
        "main_commits_recorded",
        json!({"after": last, "commits": [{"sha": "c60", "at": T0 + 150 * DAY + 3000,
               "changes": [{"path": "src/c60.rs"}], "reverts": null}]}),
        150 * DAY + 3000,
    )
    .add(
        None,
        None,
        "main_read_failed",
        json!({"reason": "lock", "at": T0 + 155 * DAY}),
        155 * DAY,
    )
    .add(
        None,
        None,
        "main_read_recovered",
        json!({"reason": "lock", "since": T0 + 155 * DAY, "at": T0 + 156 * DAY}),
        156 * DAY,
    )
    .add(
        None,
        None,
        "main_observed",
        json!({"head": "c60", "at": T0 + 156 * DAY, "since": T0 + 10 * DAY, "reverts": true}),
        156 * DAY + 10,
    )
    .add(
        None,
        None,
        "ci_checked",
        json!({"branch": "main"}),
        156 * DAY + 20,
    )
    .add(
        None,
        None,
        "main_commits_recorded",
        json!({"after": "c60", "commits": [
            {"sha": last, "at": T0 + 150 * DAY - 10, "changes": [], "reverts": null},
            {"sha": "r65", "at": T0 + near + 2900,
             "changes": [{"path": "src/base.rs"}, {"path": "src/r.rs", "from": "src/lib.rs"}],
             "reverts": null},
            {"sha": "rv60", "at": T0 + near + 2950, "changes": [{"path": "src/c60.rs",
             "deleted": true}], "reverts": "c60"}]}),
        near + 2950,
    )
    .add(
        None,
        None,
        "main_observed",
        json!({"head": "rv60", "at": T0 + near + 2960, "since": T0 + 10 * DAY, "reverts": true}),
        near + 2960,
    )
    .add(
        None,
        None,
        "ci_checked",
        json!({"branch": "main"}),
        near + 2970,
    )
    .add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "r65", "branch": "main"}),
        near + 2980,
    )
    .add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "c60", "branch": "main"}),
        near + 2990,
    );
}

/// The retractions' marks and the landed commits of runs 60 and 65, by
/// the ids `numbered` gives.
fn fix_queue_units(log: &mut Log) {
    let mut marks: HashMap<String, EventId> = HashMap::new();
    for event in &mut log.events {
        let label = event.payload["label"].as_str().map(str::to_owned);
        match event.kind.known() {
            Some(EventKind::MarkRecorded) => {
                marks.insert(label.unwrap(), event.id);
            }
            Some(EventKind::MarkRetracted) => {
                event.payload["mark"] = json!(marks[&label.unwrap()]);
            }
            Some(EventKind::RunIntegrated) => {
                for (n, commit) in [(60, "c60"), (65, "r65")] {
                    if event.run_id.as_ref().map(RunId::as_str) == Some(run(n).as_str()) {
                        event.payload["commit"] = json!(commit);
                    }
                }
            }
            _ => {}
        }
    }
}

fn history_ii(settled: usize) -> Log {
    history_with(settled, add_queue_units(settled), fix_queue_units)
}

/// [`history_ii`] without the settled units' tasks and runs, whose rows
/// none of (ii)'s reads names: what builds the ledger folds the waits
/// against the tasks open (a debug build takes minutes for 3000 units of
/// both), and the queue rows of (ii) are the same.
fn queue_units_only(settled: usize) -> Log {
    without_unit_tasks(history_ii(settled))
}

/// `log` without the settled units' tasks and runs.
fn without_unit_tasks(mut log: Log) -> Log {
    log.events.retain(|e| {
        e.task_id.is_none_or(|task| task.as_i64() < 100_000)
            && e.run_id
                .as_ref()
                .is_none_or(|run| !run.as_str().ends_with("-2222-4222-8222-222222222222"))
    });
    log
}

/// The subject kind of a queue row.
fn subject_kind(row: &LedgerRow) -> &str {
    row.subject.as_ref().map_or("", |s| s.kind.as_str())
}

/// What the narrowed read of (ii) reads of a kind besides [`queue_narrowed`].
#[derive(Debug, Clone, Copy, Default)]
struct QueueShape {
    /// The supervisor across the range's start (that of the last start
    /// before it): its last row of each kind before it.
    across: bool,
    /// With `across`, the supervisors the range's records name too.
    named_lives: bool,
    /// The snapshots that forecast a task of the range, and the marks
    /// after the earliest of them.
    snapshots: bool,
    /// Every row of the baselines' kinds (one each per store).
    baselines: bool,
    /// The ask of no run the range answers (by `ask_id`), and the one not
    /// answered by the range's start.
    asks: bool,
    /// The goals' rows of the goals the range's rows name.
    goals: bool,
    /// Main's record before the range as a summary the builder writes
    /// (see [`main_summary`]), with the commits the range names.
    main: bool,
}

const MAIN_KINDS: [&str; 8] = [
    "main_commits_recorded",
    "main_paths_recorded",
    "main_observed",
    "main_read_failed",
    "main_read_recovered",
    "main_rewritten",
    "main_reverts_imported",
    "ci_checked",
];

/// The queue rows of `rows` narrowed for `kinds` as [`queue_narrowed`]
/// reads them and what `shape` adds.
fn queue_ii(
    rows: &[LedgerRow],
    from_ms: i64,
    kinds: &BTreeSet<String>,
    shape: QueueShape,
) -> Vec<LedgerRow> {
    use crate::domain::stats::timestamp_millis;
    let before = |e: &RunEvent| timestamp_millis(&e.created_at).is_some_and(|at| at < from_ms);
    let queue = || rows.iter().filter(|row| row.kind == LedgerKind::Queue);
    let mut keep: BTreeSet<String> = BTreeSet::new();
    let reaches = |row: &LedgerRow| {
        row.ended_at.is_none_or(|end| end >= from_ms) || row.records.iter().any(|e| !before(e))
    };
    if shape.across {
        // The supervisors whose life may reach the range: the last started
        // before it, and those its records name (a life is read up to its
        // last event, whoever started since).
        let mut tokens: BTreeSet<String> = queue()
            .filter(|row| subject_kind(row) == "supervisor_started")
            .flat_map(|row| &row.records)
            .filter(|e| before(e))
            .max_by_key(|e| e.id)
            .and_then(|e| e.payload["supervisor"].as_str().map(str::to_owned))
            .into_iter()
            .collect();
        if shape.named_lives {
            tokens.extend(
                rows.iter()
                    .flat_map(|row| &row.records)
                    .filter(|e| !before(e))
                    .filter_map(|e| e.payload["supervisor"].as_str().map(str::to_owned)),
            );
        }
        let mut last: BTreeMap<(&str, &str), (EventId, &str)> = BTreeMap::new();
        for row in queue() {
            for e in row.records.iter().filter(|e| before(e)) {
                let Some(token) = e.payload["supervisor"].as_str() else {
                    continue;
                };
                if tokens.contains(token) {
                    if subject_kind(row) == "supervisor_started" {
                        keep.insert(row.key.clone());
                    }
                    let at = last
                        .entry((token, subject_kind(row)))
                        .or_insert((e.id, &row.key));
                    if at.0 < e.id {
                        *at = (e.id, &row.key);
                    }
                }
            }
        }
        keep.extend(last.values().map(|(_, key)| (*key).to_owned()));
    }
    if shape.snapshots {
        // The snapshots that forecast a task of the range (read by the
        // task, as a table of the snapshots' targets would give them), and
        // the marks after the earliest of them with their retractions.
        let tasks: BTreeSet<i64> = rows
            .iter()
            .filter(|row| row.kind == LedgerKind::Task && reaches(row))
            .filter_map(|row| row.task_id.map(|task| task.as_i64()))
            .collect();
        let snapshots: Vec<&LedgerRow> = queue()
            .filter(|row| subject_kind(row) == "forecast_recorded")
            .filter(|row| {
                row.records.iter().any(|e| {
                    e.payload["tasks"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|task| task["id"].as_i64().is_some_and(|id| tasks.contains(&id)))
                })
            })
            .collect();
        keep.extend(snapshots.iter().map(|row| row.key.clone()));
        let first = snapshots.iter().filter_map(|row| row.started_at).min();
        let marks: BTreeSet<EventId> = queue()
            .filter(|row| subject_kind(row) == "mark_recorded")
            .filter(|row| first.is_some_and(|first| row.started_at.is_some_and(|at| at > first)))
            .flat_map(|row| row.records.iter().map(|e| e.id))
            .collect();
        keep.extend(
            queue()
                .filter(|row| {
                    row.records.iter().any(|e| {
                        marks.contains(&e.id)
                            || (e.kind == EventKind::MarkRetracted
                                && e.payload["mark"]
                                    .as_i64()
                                    .is_some_and(|mark| marks.contains(&EventId::new(mark))))
                    })
                })
                .map(|row| row.key.clone()),
        );
    }
    if shape.baselines {
        keep.extend(
            queue()
                .filter(|row| subject_kind(row).ends_with("_baseline"))
                .map(|row| row.key.clone()),
        );
    }
    if shape.asks {
        let ask = |e: &RunEvent| e.payload["ask_id"].as_i64();
        let answered_before: BTreeSet<i64> = queue()
            .filter(|row| subject_kind(row) == "ask_answered")
            .flat_map(|row| &row.records)
            .filter(|e| before(e))
            .filter_map(ask)
            .collect();
        let answered_after: BTreeSet<i64> = queue()
            .filter(|row| subject_kind(row) == "ask_answered")
            .flat_map(|row| &row.records)
            .filter(|e| !before(e))
            .filter_map(ask)
            .collect();
        keep.extend(
            queue()
                .filter(|row| subject_kind(row) == "ask_opened")
                .filter(|row| {
                    row.records
                        .iter()
                        .filter_map(ask)
                        .any(|id| answered_after.contains(&id) || !answered_before.contains(&id))
                })
                .map(|row| row.key.clone()),
        );
    }
    if shape.goals {
        let named: BTreeSet<i64> = rows
            .iter()
            .filter(|row| row.kind != LedgerKind::Queue && reaches(row))
            .flat_map(|row| &row.records)
            .filter_map(|e| {
                e.payload["goal_id"].as_i64().or_else(|| {
                    (e.kind == EventKind::TaskGoalChanged)
                        .then(|| e.payload["to"].as_i64())
                        .flatten()
                })
            })
            .collect();
        keep.extend(
            queue()
                .filter(|row| {
                    row.records
                        .iter()
                        .any(|e| e.goal_id.is_some_and(|goal| named.contains(&goal.as_i64())))
                })
                .map(|row| row.key.clone()),
        );
    }
    let narrowed = queue_narrowed(rows, from_ms, kinds);
    let mut out: Vec<LedgerRow> = narrowed;
    let have: BTreeSet<String> = out
        .iter()
        .filter(|row| row.kind == LedgerKind::Queue)
        .map(|row| row.key.clone())
        .collect();
    out.extend(
        queue()
            .filter(|row| keep.contains(&row.key) && !have.contains(&row.key))
            .cloned(),
    );
    if shape.main {
        out = main_summary(rows, out, from_ms);
    }
    out
}

/// `narrowed` with main's record before `from_ms` replaced by a summary
/// row the builder would write (the paths main has, where the record
/// starts and reaches, Git's reading, where the evidence of reverts and
/// CI's record of each branch start and reach), and the rows of main's
/// commits the range names (a landed commit of a run row the range reads,
/// a commit the range's records record again or revert): read by the sha,
/// as a table of the commits by sha the builder writes would give them.
/// The summary's records take the ids of the rows it replaces.
fn main_summary(whole: &[LedgerRow], narrowed: Vec<LedgerRow>, from_ms: i64) -> Vec<LedgerRow> {
    use crate::domain::stats::main_log::fold_main_log;
    use crate::domain::stats::timestamp_millis;
    let before = |e: &RunEvent| timestamp_millis(&e.created_at).is_some_and(|at| at < from_ms);
    let is_main = |row: &LedgerRow| {
        row.kind == LedgerKind::Queue
            && MAIN_KINDS.contains(&subject_kind(row))
            && row.records.iter().all(&before)
    };
    let mut old: Vec<&LedgerRow> = whole.iter().filter(|row| is_main(row)).collect();
    old.sort_by_key(|row| row.records.first().map(|e| e.id));
    let events: Vec<RunEvent> = old.iter().flat_map(|row| row.records.clone()).collect();
    // The shas the range names: the landed commits of the run rows it
    // reads, and the commits and the reverts' targets of its records.
    let mut named: BTreeSet<String> = BTreeSet::new();
    for row in narrowed.iter().filter(|row| !is_main(row)) {
        for e in &row.records {
            if row.kind == LedgerKind::Run && e.kind == EventKind::RunIntegrated {
                named.extend(e.payload["commit"].as_str().map(str::to_owned));
            }
            if e.kind == EventKind::MainCommitsRecorded {
                for commit in e.payload["commits"].as_array().into_iter().flatten() {
                    named.extend(commit["sha"].as_str().map(str::to_owned));
                    named.extend(commit["reverts"].as_str().map(str::to_owned));
                }
            }
            if e.kind == EventKind::CiTurnedRed {
                named.extend(e.payload["sha"].as_str().map(str::to_owned));
            }
        }
    }
    let log = fold_main_log(&events);
    let mut summary = Vec::new();
    if log.paths_known {
        let mut paths: Vec<&String> = log.history.paths.iter().collect();
        paths.sort();
        summary.push((
            "main_paths_recorded",
            json!({"head": log.history.commits.last().map(|c| c.sha.clone()),
                   "since": log.since, "part": 0, "parts": 1, "paths": paths}),
        ));
    }
    if let Some(at) = log.recorded_through {
        summary.push((
            "main_observed",
            json!({"head": log.head, "at": at, "since": log.since, "reverts": true}),
        ));
    }
    let last_of = |kinds: &[EventKind]| {
        events
            .iter()
            .rev()
            .find(|e| kinds.iter().any(|kind| e.kind == *kind))
            .cloned()
    };
    let reading = last_of(&[EventKind::MainReadFailed, EventKind::MainReadRecovered]);
    let checks: Vec<&RunEvent> = events
        .iter()
        .filter(|e| e.kind == EventKind::CiChecked)
        .collect();
    let mut seek: Vec<RunEvent> = events
        .iter()
        .filter(|e| {
            e.kind == EventKind::MainCommitsRecorded
                && e.payload["commits"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|commit| {
                        commit["sha"]
                            .as_str()
                            .is_some_and(|sha| named.contains(sha))
                    })
        })
        .cloned()
        .collect();
    let mut records: Vec<RunEvent> = Vec::new();
    // CI's record of the branch: its first and its last check.
    records.extend(checks.first().map(|e| (*e).clone()));
    records.extend(
        checks
            .last()
            .filter(|_| checks.len() > 1)
            .map(|e| (*e).clone()),
    );
    records.append(&mut seek);
    records.extend(reading);
    let mut ids: Vec<EventId> = events.iter().map(|e| e.id).collect();
    ids.retain(|id| !records.iter().any(|e| e.id == *id));
    let template = old.last().map(|row| (*row).clone());
    for (n, (kind, payload)) in summary.into_iter().enumerate() {
        let mut event = events[events.len() - 1 - n].clone();
        event.id = ids[ids.len() - 1 - n];
        event.kind = kind.into();
        event.payload = payload;
        records.push(event);
    }
    records.sort_by_key(|e| e.id);
    let mut out: Vec<LedgerRow> = narrowed.into_iter().filter(|row| !is_main(row)).collect();
    if let Some(mut row) = template {
        row.key = "main_summary".to_owned();
        row.records = records;
        out.push(row);
    }
    out
}

fn area_map() -> crate::domain::areas::AreaMap {
    crate::domain::areas::AreaMap::new(vec![
        ("base".to_owned(), vec!["src/base.rs".to_owned()]),
        (
            "units".to_owned(),
            vec!["src/u*.rs".to_owned(), "src/moved*.rs".to_owned()],
        ),
        (
            "landed".to_owned(),
            vec!["src/c60.rs".to_owned(), "src/r.rs".to_owned()],
        ),
    ])
    .unwrap()
}

/// The answers of `got` (rows narrowed of the queue rows) that differ
/// from those of every row of `whole`: the kpi's four questions and its
/// `--by area`, `stats --since` (two days) `--full` with the areas, and
/// of each window main's cover and what befell runs 60 and 65; with the
/// reads of the day's question.
fn queue_misses(whole: &[LedgerRow], got: &[LedgerRow]) -> (Vec<String>, Reads) {
    let areas = area_map();
    let every = select(whole.to_vec(), &whole_query());
    let mut misses = Vec::new();
    let mut reads = Reads::default();
    let mut questions = kpi_questions();
    questions.push(KpiQuery {
        by: vec!["area".parse().unwrap()],
        ..questions[0].clone()
    });
    let befell = |window: &LedgerWindow| {
        let book = window.post_landing_book();
        json!({
            "covers": format!("{:?}", window.main_covers()),
            "60": serde_json::to_value(book.of_run(&RunId::new(run(60)).unwrap())).unwrap(),
            "65": serde_json::to_value(book.of_run(&RunId::new(run(65)).unwrap())).unwrap(),
            // What a report reads of the goal of the range's task 67.
            "goal": match window.attributes() {
                crate::domain::ledger::attributes::Folded::Attributes(known) => format!(
                    "{:?}",
                    known
                        .goal_summaries()
                        .into_iter()
                        .filter(|goal| goal.id == crate::domain::GoalId::new(700))
                        .collect::<Vec<_>>()
                ),
                crate::domain::ledger::attributes::Folded::BeforeBaseline => String::new(),
            },
        })
    };
    for (n, query) in questions.iter().enumerate() {
        let value = |window: &LedgerWindow, query: &KpiQuery| {
            serde_json::to_value(
                kpi_of_window(
                    window,
                    NOW,
                    HOST,
                    &KpiConfig::default(),
                    Some(&areas),
                    query,
                )
                .unwrap(),
            )
            .unwrap()
        };
        let expected = value(
            &every,
            &KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            },
        );
        let (window, read) = narrowed_shaped(got, &time_query(query), &PARTS, SHAPED);
        if n == 0 {
            reads = read;
            differences(&befell(&window), &befell(&every), "befell", &mut misses);
        }
        differences(
            &value(&window, query),
            &expected,
            &format!("kpi[{n}]"),
            &mut misses,
        );
    }
    let sources = StatsSources {
        config_file: &|| Ok(None),
        conflicts_file: &|| Ok(None),
        areas: Some(&areas),
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
    let base = WindowQuery {
        since: Some(since),
        now_ms: now_ms(NOW),
        ..WindowQuery::default()
    };
    let expected = serde_json::to_value(
        stats_of_window(&select(whole.to_vec(), &base), NOW, &query, &sources).unwrap(),
    )
    .unwrap();
    let from = WindowQuery {
        read_from_ms: Some((NOW - 2 * DAY) * 1000),
        ..base
    };
    let (window, _) = narrowed_shaped(got, &from, &PARTS, SHAPED);
    let got =
        serde_json::to_value(stats_of_window(&window, NOW, &query, &sources).unwrap()).unwrap();
    differences(&got, &expected, "stats", &mut misses);
    (misses, reads)
}

/// The kinds of (ii) by part, as the description names them.
const QUEUE_PARTS: [(&str, &[&str]); 6] = [
    (
        "ii-a",
        &[
            "slots_full",
            "claim_hold",
            "landing_hold",
            "supervisor_started",
            "supervisor_alive",
            "supervisor_stopped",
            "mark_recorded",
            "mark_retracted",
            "forecast_recorded",
            "attributes_baseline",
            "finding_baseline",
        ],
    ),
    ("ii-f", &["candidates_sampled"]),
    ("ii-b", &MAIN_KINDS),
    ("ii-c", &["ci_turned_red"]),
    ("ii-d", &["goal_created", "goal_updated"]),
    (
        "ii-e",
        &[
            "update_failed",
            "backend_call_failed",
            "ask_opened",
            "ask_answered",
            "auto_repaired",
            "stall_config_loaded",
        ],
    ),
];

/// The queue rows of `kind` among `rows`.
fn queue_count(rows: &[LedgerRow], kind: &str) -> usize {
    rows.iter()
        .filter(|row| row.kind == LedgerKind::Queue && subject_kind(row) == kind)
        .count()
}

const QUEUE_SHAPED: QueueShape = QueueShape {
    across: true,
    named_lives: true,
    snapshots: true,
    baselines: true,
    asks: true,
    goals: true,
    main: true,
};

/// The read of (ii) of each part's kinds alone, `shape` for the kinds of
/// the part and every other queue row read whole: the misses against the
/// whole read of `rows`, and the part's queue rows that read and the whole
/// read return.
fn part_report(rows: &[LedgerRow], part: &[&str], shape: QueueShape) -> (Vec<String>, [usize; 2]) {
    let from_ms = time_query(&kpi_questions()[1]).read_from_ms.unwrap();
    let kinds: BTreeSet<String> = part.iter().map(|kind| (*kind).to_owned()).collect();
    let count = |rows: &[LedgerRow]| {
        part.iter()
            .map(|kind| queue_count(rows, kind))
            .sum::<usize>()
            + rows.iter().filter(|row| row.key == "main_summary").count()
    };
    let narrowed = queue_ii(rows, from_ms, &kinds, shape);
    let (misses, _) = queue_misses(rows, &narrowed);
    (misses, [count(&narrowed), count(rows)])
}

/// [`queue_units_only`] without the settled units' supervisors' lives
/// (each start ends the life before it, so a whole read's `kpi` reads
/// every start for each: minutes for 3000 in a debug build): for the parts
/// that read no life.
fn queue_units_without_lives(settled: usize) -> Log {
    let mut log = queue_units_only(settled);
    log.events.retain(|e| {
        !(e.kind.is_in(&crate::domain::supervisor_life::KINDS)
            && e.payload["supervisor"]
                .as_str()
                .is_some_and(|token| token.starts_with('u')))
    });
    log
}

/// Check the part `name` of (ii): with 30 settled units the read of
/// Spike 4678 ([`queue_narrowed`]) and that of [`QUEUE_SHAPED`] against the
/// whole read, the shaped read's queue rows of the part the same with 30
/// and 3000 units while the whole read's grow, and the shaped read's
/// answers against the whole read's with `large` units of `history`.
/// Returns the misses of the read of Spike 4678.
fn check_part(name: &str, history: fn(usize) -> Log) -> Vec<String> {
    check_part_of(name, history_ii, queue_units_only, history)
}

/// The settled units the answers are compared with besides 30: 300 by
/// default (a whole read of 3000 units' lives and waits takes minutes in a
/// debug build), `DAGQ_SPIKE_4756_LARGE` for another number.
fn large_answers() -> usize {
    std::env::var("DAGQ_SPIKE_4756_LARGE")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(300)
}

/// [`check_part`] of the histories `small` (of 30 units), `units` (whose
/// queue rows are counted with 3000) and `history` (of the answers with
/// [`large_answers`]).
fn check_part_of(
    name: &str,
    small: fn(usize) -> Log,
    units: fn(usize) -> Log,
    history: fn(usize) -> Log,
) -> Vec<String> {
    let large = large_answers();
    let part = QUEUE_PARTS.iter().find(|(n, _)| *n == name).unwrap().1;
    let small = rows_of(&small(30));
    let (of_4678, _) = part_report(&small, part, QueueShape::default());
    let (misses, [narrowed_small, whole_small]) = part_report(&small, part, QUEUE_SHAPED);
    let from_ms = time_query(&kpi_questions()[1]).read_from_ms.unwrap();
    let kinds: BTreeSet<String> = part.iter().map(|kind| (*kind).to_owned()).collect();
    let count = |rows: &[LedgerRow]| {
        part.iter()
            .map(|kind| queue_count(rows, kind))
            .sum::<usize>()
            + rows.iter().filter(|row| row.key == "main_summary").count()
    };
    let units = rows_of(&units(3000));
    let by_kind = |rows: &[LedgerRow]| {
        part.iter()
            .map(|kind| queue_count(rows, kind))
            .collect::<Vec<_>>()
    };
    let narrowed_units = queue_ii(&units, from_ms, &kinds, QUEUE_SHAPED);
    eprintln!(
        "{name} by kind {part:?}: {:?} {:?}",
        by_kind(&queue_ii(&small, from_ms, &kinds, QUEUE_SHAPED)),
        by_kind(&narrowed_units)
    );
    let narrowed_large = count(&narrowed_units);
    let whole_large = count(&units);
    let rows = rows_of(&history(large));
    let (large_misses, _) = queue_misses(&rows, &queue_ii(&rows, from_ms, &kinds, QUEUE_SHAPED));
    eprintln!(
        "{name}: 4678 {} misses {:?}; shaped {narrowed_small}/{narrowed_large} of {whole_small}/{whole_large} \
         (30/3000), misses {misses:?}, with {large}: {large_misses:?}",
        of_4678.len(),
        of_4678.iter().take(4).collect::<Vec<_>>()
    );
    assert!(
        whole_large > 50 * whole_small / 2 || whole_small < 10,
        "{name}"
    );
    if name == "ii-a" && small.iter().any(old_snapshot) {
        // A snapshot older than the settled history: the marks between it
        // and the range (a supervisor's start is one) are read whole.
        let marks = |misses: &[String]| {
            !misses.is_empty()
                && misses
                    .iter()
                    .all(|miss| miss.contains("forecast.marks_between"))
        };
        assert!(
            marks(&misses) && marks(&large_misses),
            "{misses:?} {large_misses:?}"
        );
        assert!(narrowed_large > 25 * narrowed_small, "{name}");
        return of_4678;
    }
    assert_eq!(misses, Vec::<String>::new(), "{name} 30");
    assert_eq!(large_misses, Vec::<String>::new(), "{name} {large}");
    assert_eq!(narrowed_small, narrowed_large, "{name}");
    of_4678
}

/// Whether `row` is the snapshot of [`log_of_a_time`] (day 3) forecasting
/// tasks 1 and 2.
fn old_snapshot(row: &LedgerRow) -> bool {
    subject_kind(row) == "forecast_recorded"
        && row.records.iter().any(|e| {
            e.payload["tasks"]
                .as_array()
                .is_some_and(|tasks| !tasks.is_empty())
                && timestamp_secs(e) < 10 * DAY
        })
}

fn timestamp_secs(event: &RunEvent) -> i64 {
    crate::domain::stats::timestamp_millis(&event.created_at).unwrap_or(0) / 1000 - T0
}

/// [`history_ii`] with the snapshot of tasks 1 and 2 taken after the
/// settled units (day 158) instead of on day 3.
fn near_snapshot(settled: usize) -> Log {
    let mut log = history_with(
        settled,
        |log| {
            add_queue_units(settled)(log);
            log.add(
                None,
                None,
                "forecast_recorded",
                json!({"at_secs": T0 + 158 * DAY, "method": 1, "goals": [],
                       "tasks": [{"id": 2, "p50_secs": 3600, "p90_secs": 7200},
                                 {"id": 1, "p50_secs": 3600, "p90_secs": 7200}]}),
                158 * DAY,
            );
        },
        fix_queue_units,
    );
    for event in &mut log.events {
        if event.kind == EventKind::ForecastRecorded && timestamp_secs(event) < 10 * DAY {
            event.payload["tasks"] = json!([]);
        }
    }
    log
}

fn near_snapshot_units(settled: usize) -> Log {
    without_unit_tasks(near_snapshot(settled))
}

#[test]
fn the_waits_lives_marks_and_snapshots_read_by_their_range_answer_as_every_row() {
    let of_4678 = check_part("ii-a", queue_units_only);
    assert!(!of_4678.is_empty());
}

#[test]
fn a_snapshot_after_the_settled_history_reads_its_marks_by_the_range() {
    check_part_of(
        "ii-a",
        near_snapshot,
        near_snapshot_units,
        near_snapshot_units,
    );
}

#[test]
fn the_candidates_sampled_read_by_their_range_answer_as_every_row() {
    check_part("ii-f", queue_units_only);
}

#[test]
fn main_s_record_read_from_a_summary_answers_as_every_row() {
    let of_4678 = check_part("ii-b", queue_units_without_lives);
    assert!(!of_4678.is_empty());
}

#[test]
fn what_befell_a_landing_read_by_the_range_answers_as_every_row() {
    check_part("ii-c", queue_units_without_lives);
}

#[test]
fn the_goals_of_the_range_s_tasks_read_by_the_goal_answer_as_every_row() {
    let of_4678 = check_part("ii-d", queue_units_without_lives);
    assert!(!of_4678.is_empty());
}

#[test]
fn the_updates_failures_and_asks_of_no_run_read_by_their_range_answer_as_every_row() {
    check_part("ii-e", queue_units_without_lives);
}

#[test]
fn every_queue_part_of_ii_read_by_its_range_together_answers_as_every_row() {
    let from_ms = time_query(&kpi_questions()[1]).read_from_ms.unwrap();
    let queue = |rows: &[LedgerRow]| {
        rows.iter()
            .filter(|row| row.kind == LedgerKind::Queue)
            .count()
    };
    let mut counts = Vec::new();
    for (settled, log) in [
        (30, near_snapshot(30)),
        (30, near_snapshot_units(30)),
        (large_answers(), near_snapshot_units(large_answers())),
        (3000, near_snapshot_units(3000)),
    ] {
        let rows = rows_of(&log);
        // Every queue kind: (ii)'s by their shapes, 4678's by its read.
        let kinds: BTreeSet<String> = rows
            .iter()
            .filter(|row| row.kind == LedgerKind::Queue)
            .map(|row| subject_kind(row).to_owned())
            .collect();
        let narrowed = queue_ii(&rows, from_ms, &kinds, QUEUE_SHAPED);
        if settled < 3000 || large_answers() == 3000 {
            let (misses, _) = queue_misses(&rows, &narrowed);
            assert_eq!(misses, Vec::<String>::new(), "{settled}");
        }
        eprintln!(
            "{settled}: queue rows {} of {}",
            queue(&narrowed),
            queue(&rows)
        );
        counts.push((queue(&narrowed), queue(&rows)));
    }
    // Of the units without their tasks: the same with 30 and 3000.
    assert_eq!(counts[1].0, counts[3].0, "{counts:?}");
    assert!(counts[3].1 > 50 * counts[1].1 / 2, "{counts:?}");
}
