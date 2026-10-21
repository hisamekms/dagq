use serde_json::json;

use super::*;
use crate::domain::RunId;
use crate::domain::host_metrics::{HOST_NODE, SampleSource};
use crate::domain::ledger::builder::{self, Inputs, Target};
use crate::domain::ledger::node;
use crate::domain::ledger::testkit::{Log, ms, run_id};
use crate::domain::run_phase::{Attempt, Phase, PhaseChange};
use crate::domain::stats;

const BUILT: &str = "2030-01-01T00:00:00.000Z";

/// The ledger's row of `run` as what builds it writes it at `now`.
fn row_of(log: &Log, run: &str, now: i64) -> LedgerRow {
    let inputs = Inputs::new(&log.events, ms(now), BUILT, false);
    builder::build(&Target::Run(run.to_owned()), &inputs, &[]).unwrap()
}

fn stats_of(log: &Log, run: &str) -> RunStats {
    stats::run_stats(&log.events)
        .into_iter()
        .find(|stats| stats.run_id.as_str() == run)
        .unwrap()
}

fn field<'c>(comparison: &'c RunComparison, name: &str) -> &'c Diff {
    comparison
        .fields
        .iter()
        .find(|diff| diff.field == name)
        .unwrap()
}

/// A run whose `approve_landing` ask is still open when its review ends:
/// today's `stats` forgets the ask at the review's end and counts the wait
/// for the answer as `exit`; the ledger has it as `ask`. Both cut
/// `wait_to_land` at `run_integrated`, and the push is apart.
#[test]
fn an_approve_landing_ask_in_todays_exit_is_told_apart() {
    let a = run_id(1);
    let mut log = Log::default();
    let phase = |log: &mut Log, secs: i64, phase: Phase| {
        log.add(
            Some(1),
            Some(&a),
            "run_phase_changed",
            PhaseChange::new(phase, Attempt::FIRST, "x").payload(),
            secs,
        );
    };
    log.worked(1, &a, 0);
    phase(&mut log, 660, Phase::Review);
    log.add(Some(1), Some(&a), "review_started", json!({}), 660);
    phase(&mut log, 700, Phase::LandingAnswer);
    log.add(
        Some(1),
        Some(&a),
        "ask_opened",
        json!({"kind": "approve_landing", "ask_id": 9}),
        700,
    )
    .add(
        Some(1),
        Some(&a),
        "review_finished",
        json!({"verdict": "pass"}),
        710,
    )
    .add(
        Some(1),
        Some(&a),
        "ask_answered",
        json!({"kind": "approve_landing", "ask_id": 9}),
        3660,
    );
    phase(&mut log, 3660, Phase::LandingQueue);
    log.integrated(1, &a, 3700).pushed(1, &a, 3930);

    let today = stats_of(&log, &a);
    let comparison = run(&today, Some(&row_of(&log, &a, 9000)));
    let exit = field(&comparison, "land_phases.exit");
    let ask = field(&comparison, "land_phases.ask");
    assert!(exit.stats.unwrap() >= 2950, "{exit:?}");
    assert_eq!(exit.reasons, [Reason::ExitHeldAsk]);
    assert_eq!(ask.ledger, Some(2960));
    assert_eq!(ask.reasons, [Reason::ExitHeldAsk]);
    let wait = field(&comparison, "wait_to_land");
    assert_eq!(
        (wait.stats, wait.ledger, wait.diff),
        (Some(3240), Some(3240), Some(0))
    );
    assert!(wait.reasons.is_empty());
    assert_eq!(comparison.push, Some(30));
    assert!(comparison.row && !comparison.legacy);
}

/// A run the ledger has no row of differs by `no_row`; a legacy row
/// gives today's values back; a run that agrees has no reason.
#[test]
fn a_missing_row_a_legacy_row_and_an_agreeing_run() {
    let (b, d) = (run_id(2), run_id(4));
    let mut log = Log::default();
    log.landed(2, &b, 0).legacy(4, &d, 0);
    let missing = run(&stats_of(&log, &b), None);
    assert_eq!(field(&missing, "work").reasons, [Reason::NoRow]);
    assert!(!missing.row);

    let legacy = run(&stats_of(&log, &d), Some(&row_of(&log, &d, 9000)));
    assert!(legacy.legacy);
    assert!(legacy.agrees(), "{legacy:?}");

    let agreeing = run(&stats_of(&log, &b), Some(&row_of(&log, &b, 9000)));
    for diff in &agreeing.fields {
        assert!(
            diff.reasons.iter().all(|r| *r == Reason::Boundary),
            "{diff:?}"
        );
    }
}

/// A field of no run that agrees is equal; one that differs lists the
/// values and why, by what the window's session and queue rows lack.
#[test]
fn a_field_of_no_run_lists_its_differences_and_why() {
    let today = json!({"jobs": {"observe": {"count": 3, "failed": 1}}, "sessions": {"count": 2}});
    let ledger = json!({"jobs": {"observe": {"count": 2, "failed": 1}}, "sessions": {"count": 2}});
    let count = |kind, missing: Vec<String>, dirty: Vec<String>| KindCount {
        kind,
        ended: 1,
        rows: 0,
        missing,
        not_final: Vec::new(),
        push_pending: Vec::new(),
        dirty,
    };
    let rows = [count(
        LedgerKind::Session,
        vec!["observe/1".to_owned()],
        Vec::new(),
    )];
    let compared = fields(&today, &ledger, &["jobs", "sessions"], &rows, &[]);
    assert!(!compared[0].equal);
    assert_eq!(compared[0].differences[0].path, "jobs.observe.count");
    assert_eq!(compared[0].differences[0].diff, Some(-1.0));
    assert_eq!(compared[0].reasons, [FieldReason::RowsMissing]);
    assert!(compared[1].equal && compared[1].reasons.is_empty());

    let dirty = [count(LedgerKind::Queue, Vec::new(), vec!["7".to_owned()])];
    assert_eq!(
        fields(&today, &ledger, &["jobs"], &dirty, &[])[0].reasons,
        [FieldReason::RowsDirty]
    );
    assert_eq!(
        fields(&today, &ledger, &["jobs"], &[], &[])[0].reasons,
        [FieldReason::Records]
    );
}

/// The sessions of a window by `stats --until` (its last event `upto`,
/// its end `end` seconds): over `events` as today's `stats` reads them,
/// whole, and over those to its end as the ledger's window gives them,
/// less `stale` (the events a row not folded up to them lacks).
fn sessions_of(
    events: &[RunEvent],
    upto: EventId,
    end: i64,
    stale: &[i64],
) -> (Value, Value, Vec<SpanDiff>) {
    use crate::domain::stats::sessions::{SessionWindow, by_kind, spans as span_list};
    let window = SessionWindow {
        after: EventId::new(0),
        upto,
    };
    let of = |events: &[RunEvent]| by_kind(&span_list(events), events, window, ms(end), |_| true);
    let today = of(events);
    let cut: Vec<RunEvent> = events
        .iter()
        .filter(|e| e.id <= upto && !stale.contains(&e.id.as_i64()))
        .cloned()
        .collect();
    let ledger = of(&cut);
    let diffs = spans(&today.spans, &ledger.spans);
    (
        json!({"sessions": today}),
        json!({"sessions": ledger}),
        diffs,
    )
}

/// Spans open at the window's end whose turns are recorded after it:
/// today's `stats` counts those turns, the ledger's window does not. One
/// has turns before the end (its active differs), one has none (the
/// ledger leaves it out of `active.count`: the count is 1 less); a span
/// closed in the window agrees. Each is named, and the field's active
/// values are told apart from the rows' reasons.
#[test]
fn turns_recorded_after_the_window_end_are_named_span_by_span() {
    use crate::domain::marks::utc_text;
    let (a, b) = (run_id(1), run_id(2));
    let turns = |log: &mut Log, opened: i64, turns: &[(i64, i64)], secs: i64| {
        let turns: Vec<Value> = turns
            .iter()
            .map(|(from, to)| json!([utc_text(ms(*from)), utc_text(ms(*to))]))
            .collect();
        let run = log.events[usize::try_from(opened - 1).unwrap()]
            .run_id
            .clone();
        log.add(
            None,
            run.as_ref().map(RunId::as_str),
            "session_turns",
            json!({"opened_event_id": opened, "turns": turns}),
            secs,
        );
    };
    let closed = |log: &mut Log, run: Option<&str>, opened: i64, active: i64, secs: i64| {
        log.add(
            None,
            run,
            "session_closed",
            json!({"opened_event_id": opened, "reason": "exited",
                "active": "recorded", "active_secs": active}),
            secs,
        );
    };
    let mut log = Log::default();
    log.add(
        Some(1),
        Some(&a),
        "session_opened",
        json!({"kind": "worker"}),
        0,
    );
    turns(&mut log, 1, &[(10, 40)], 50);
    log.add(
        Some(2),
        Some(&b),
        "session_opened",
        json!({"kind": "revise"}),
        60,
    );
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "route": "headless", "planner_id": 3}),
        70,
    );
    turns(&mut log, 4, &[(75, 85)], 85);
    closed(&mut log, None, 4, 10, 90);
    let upto = log.last();
    // After the window's end (100 s): the worker's and the revise's turns
    // of time in the window, and their closes.
    turns(&mut log, 1, &[(80, 120)], 130);
    closed(&mut log, Some(&a), 1, 70, 130);
    turns(&mut log, 3, &[(60, 95)], 140);
    closed(&mut log, Some(&b), 3, 35, 140);

    let (today, ledger, diffs) = sessions_of(&log.events, upto, 100, &[]);
    let named: Vec<_> = diffs
        .iter()
        .map(|d| {
            (
                d.session.as_i64(),
                d.kind.as_str(),
                d.run_id.clone(),
                d.stats,
                d.ledger,
                d.diff,
                d.reason,
            )
        })
        .collect();
    assert_eq!(
        named,
        [
            (
                1,
                "worker",
                Some(a.clone()),
                Some(50),
                Some(30),
                Some(-20),
                SpanReason::TurnsAfterEnd
            ),
            (
                3,
                "revise",
                Some(b.clone()),
                Some(35),
                None,
                None,
                SpanReason::TurnsAfterEnd
            ),
        ]
    );
    assert_eq!(
        today["sessions"]["by_kind"]["revise"]["active"]["count"],
        json!(1)
    );
    assert_eq!(
        ledger["sessions"]["by_kind"]["revise"]["active"]["count"],
        json!(0)
    );
    let compared = &fields(&today, &ledger, &["sessions"], &[], &diffs)[0];
    assert!(!compared.equal);
    assert_eq!(compared.reasons, [FieldReason::TurnsAfterEnd]);
    assert_eq!(compared.spans, diffs);
    let listed = serde_json::to_value(compared).unwrap();
    assert_eq!(listed["spans"][1]["session"], json!(3));
    assert_eq!(listed["spans"][0]["reason"], json!("turns_after_end"));

    // A value not of active time still goes by the rows.
    let mut other = ledger.clone();
    other["sessions"]["by_kind"]["revise"]["count"] = json!(9);
    assert_eq!(
        fields(&today, &other, &["sessions"], &[], &diffs)[0].reasons,
        [FieldReason::TurnsAfterEnd, FieldReason::Records]
    );
    // A span not of the turns after the end leaves the field to the rows.
    let mut unexplained = diffs.clone();
    unexplained[0].reason = SpanReason::Unexplained;
    assert_eq!(
        fields(&today, &ledger, &["sessions"], &[], &unexplained)[0].reasons,
        [FieldReason::Records]
    );
    // Missing rows are named beside the turns after the end.
    let missing = [KindCount {
        kind: LedgerKind::Session,
        ended: 1,
        rows: 0,
        missing: vec!["x".to_owned()],
        not_final: Vec::new(),
        push_pending: Vec::new(),
        dirty: Vec::new(),
    }];
    assert_eq!(
        fields(&today, &ledger, &["sessions"], &missing, &diffs)[0].reasons,
        [FieldReason::TurnsAfterEnd, FieldReason::RowsMissing]
    );
    // A window that ends now: both read the same events and agree.
    let (today, ledger, diffs) = sessions_of(&log.events, log.last(), 200, &[]);
    assert!(diffs.is_empty());
    assert!(fields(&today, &ledger, &["sessions"], &[], &diffs)[0].equal);

    // A ledger row not folded up to the worker's turns recorded in the
    // window (event 2) lacks them too: the turns after the end do not
    // make the difference, and it is not told as theirs.
    let (today, ledger, diffs) = sessions_of(&log.events, upto, 100, &[2]);
    let worker = diffs.iter().find(|d| d.session.as_i64() == 1).unwrap();
    assert_eq!((worker.stats, worker.ledger), (Some(50), None));
    assert_eq!(worker.reason, SpanReason::Unexplained);
    assert_eq!(
        fields(&today, &ledger, &["sessions"], &[], &diffs)[0].reasons,
        [FieldReason::Records]
    );
    // Nor in a window to now, whose end no event comes after: the worker
    // is open at it and the row lacks its last turns.
    let now = &log.events[..7];
    let (_, _, diffs) = sessions_of(now, EventId::new(7), 135, &[7]);
    let worker = diffs.iter().find(|d| d.session.as_i64() == 1).unwrap();
    assert_eq!((worker.stats, worker.ledger), (Some(70), Some(30)));
    assert_eq!(worker.reason, SpanReason::Unexplained);
}

/// A span the ledger has alone, or with more active time, is not of the
/// turns after the end.
#[test]
fn a_span_on_one_side_or_with_more_in_the_ledger_is_unexplained() {
    use crate::domain::stats::sessions::SpanInWindow;
    let span = |opened: i64, closed_in_window: bool, active: Option<i64>| SpanInWindow {
        opened: EventId::new(opened),
        kind: "worker".to_owned(),
        run_id: None,
        closed_in_window,
        active,
        active_through_end: active,
    };
    let diffs = spans(
        &[span(1, false, Some(5)), span(2, true, Some(5))],
        &[span(1, false, Some(9)), span(3, false, Some(1))],
    );
    let reasons: Vec<_> = diffs
        .iter()
        .map(|d| (d.session.as_i64(), d.reason))
        .collect();
    assert_eq!(
        reasons,
        [
            (1, SpanReason::Unexplained),
            (2, SpanReason::Unexplained),
            (3, SpanReason::Unexplained),
        ]
    );
    assert_eq!(diffs[0].diff, Some(4));
    assert_eq!((diffs[1].stats, diffs[1].ledger), (Some(5), None));
}

fn csv_sample(unix: i64, load: f64) -> HostSample {
    let mut sample = HostSample::new(unix);
    sample.set("load1", Some(load));
    sample
}

fn node_sample(id: i64, unix: i64, load: f64) -> NodeSample {
    NodeSample {
        id,
        node_id: HOST_NODE.to_owned(),
        local_day: host_metrics::local_day(unix, 0),
        source: SampleSource::Sampled,
        sample: csv_sample(unix, load),
    }
}

/// The host's summaries side by side: a row only the CSV file has on a
/// day the store has (a new day's header written twice) is `csv_only`, a
/// day the store has none of is `import_missing`, a store's row the files
/// lack is `store_only`, a day before the retention is `retention`; the
/// statistics that differ are listed.
#[test]
fn the_hosts_samples_only_one_side_has_are_told_apart_by_why() {
    let day = 86_400;
    let (d1, d2, d0) = (20_000 * day, 20_001 * day, 19_999 * day);
    let csv = [
        csv_sample(d0 + 10, 9.0),
        csv_sample(d1 + 30, 1.0),
        csv_sample(d1 + 31, 1.5),
        csv_sample(d1 + 60, 2.0),
        csv_sample(d2 + 30, 3.0),
    ];
    let ledger = [
        node_sample(1, d1 + 30, 1.0),
        node_sample(2, d1 + 60, 2.0),
        node_sample(3, d1 + 90, 2.5),
    ];
    let compared = host("stats", (d0, d2 + day - 1), &csv, &ledger, 0, Some(20_000));
    assert_eq!(
        compared.samples,
        SampleDiff {
            both: 2,
            csv_only: 1,
            import_missing: 1,
            store_only: 1,
            retention: 1,
        }
    );
    let samples = compared
        .columns
        .iter()
        .find(|c| c.column == "all" && c.stat == "samples")
        .unwrap();
    assert_eq!((samples.csv, samples.ledger), (Some(5.0), Some(3.0)));
    assert!(
        compared
            .columns
            .iter()
            .any(|c| c.column == "load1" && c.stat == "max")
    );

    let same = host(
        "2030-01-01",
        (d1, d1 + 100),
        &csv[1..2],
        &ledger[..1],
        0,
        None,
    );
    assert!(same.columns.is_empty(), "{same:?}");
    assert_eq!(same.samples.both, 1);
}

/// The reconciliation counts what ended in the window and the rows of it:
/// a run without a row is missing, a pending push is listed apart from
/// the rows not settled, and a settled row an event reached after it was
/// written is dirty and not up to date. Session, queue and node rows are
/// reconciled the same way.
#[test]
fn the_reconciliation_lists_missing_unsettled_pending_and_dirty_rows() {
    let (a, b, c) = (run_id(1), run_id(2), run_id(3));
    let mut log = Log::default();
    log.landed(1, &a, 0)
        .landed(2, &b, 0)
        .worked(3, &c, 0)
        .integrated(3, &c, 700)
        .push_failed(3, &c, 910)
        .add(
            None,
            None,
            "session_opened",
            json!({"kind": "runtime_planner", "planner_id": 5}),
            0,
        );
    let opened = log.last().as_i64();
    log.add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": 5, "opened_event_id": opened}),
        100,
    );
    let start = node::bucket_of(crate::domain::ledger::testkit::T0);
    let samples = vec![
        node_sample(1, start + 5, 1.0),
        node_sample(2, start + 50, 1.0),
    ];

    // What builds the ledger wrote by 5000, but run b's row.
    let mut written = Vec::new();
    let inputs = Inputs::new(&log.events, ms(5000), BUILT, true);
    let mut targets = vec![
        Target::Run(a.clone()),
        Target::Run(c.clone()),
        Target::Task(TaskId::new(1)),
        Target::Task(TaskId::new(3)),
        Target::Node(HOST_NODE.to_owned(), start),
    ];
    targets.extend(
        inputs
            .rows
            .keys()
            .map(|(kind, key)| Target::Row(*kind, key.clone())),
    );
    for target in &targets {
        if let Some(row) = builder::build(target, &inputs, &samples) {
            written.push(row);
        }
    }
    assert!(written.iter().find(|r| r.key == a).unwrap().is_final);
    // A later event reaches run a; no pass has folded it yet.
    log.add(Some(1), Some(&a), "review_finished", json!({}), 6000);

    let mut expected = super::super::build(&log.events, ms(7000), BUILT);
    expected.extend(node::node_rows(&samples, BUILT));
    let reconciled = reconcile(&expected, &written, &log.events, None, ms(7000), ms(7000));
    let kind = |kind| reconciled.kinds.iter().find(|k| k.kind == kind).unwrap();
    let runs = kind(LedgerKind::Run);
    assert_eq!((runs.ended, runs.rows), (3, 2));
    assert_eq!(runs.missing, std::slice::from_ref(&b));
    assert_eq!(runs.push_pending, std::slice::from_ref(&c));
    assert!(runs.not_final.is_empty(), "{runs:?}");
    assert_eq!(runs.dirty, std::slice::from_ref(&a));
    let state = reconciled.rows.iter().find(|r| r.key == a).unwrap();
    assert!(state.is_final && !state.up_to_date);
    assert!(state.reach > state.folded.unwrap());
    let tasks = kind(LedgerKind::Task);
    assert_eq!(tasks.missing, ["2".to_owned()]);
    assert_eq!(tasks.push_pending, ["3".to_owned()]);
    assert_eq!(tasks.dirty, ["1".to_owned()]);

    let sessions = kind(LedgerKind::Session);
    assert!(
        sessions.ended >= 1 && sessions.missing.is_empty(),
        "{sessions:?}"
    );
    let nodes = kind(LedgerKind::Node);
    assert_eq!((nodes.ended, nodes.rows), (1, 1));
    assert!(nodes.dirty.is_empty());
    let late = [samples.clone(), vec![node_sample(3, start + 90, 1.0)]].concat();
    let mut expected = super::super::build(&log.events, ms(7000), BUILT);
    expected.extend(node::node_rows(&late, BUILT));
    let reconciled = reconcile(&expected, &written, &log.events, None, ms(7000), ms(7000));
    let nodes = reconciled
        .kinds
        .iter()
        .find(|k| k.kind == LedgerKind::Node)
        .unwrap();
    assert_eq!(nodes.dirty.len(), 1);

    // A window after every end has nothing ended.
    let after = reconcile(
        &expected,
        &written,
        &log.events,
        Some(ms(8000)),
        ms(9000),
        ms(9000),
    );
    assert!(after.kinds.iter().all(|k| k.ended == 0));
}
