//! Whether the queue rows of the kinds of one event each can be left out
//! of the ledger with `stats`, `kpi`, `report`, the check of the targets,
//! the judgments of now, the findings and the queue rows
//! ([`crate::domain::ledger::queue::QueueBook`]'s) the same, the readers
//! reading from the EventStore what they need of them instead
//! (docs/plans/ledger-queue-rows-readers-spike.md "kind と読み手"). The
//! ledger of a fixed history is made whole and again without the queue
//! rows of some kinds (as a builder that does not take them as the queue's
//! own would write it, reading them all the same), and each reader's value
//! of the two is compared. Shape A gives the window the rows alone. Shape
//! B reads its edges of the EventStore as a store does: the omitted events
//! of the kinds left out, by the coverage of them the builder writes
//! (C at 0, S at the last event) and its points
//! ([`crate::domain::ledger::window::omitted`], the store in memory
//! [`EdgeStore`]: a few events near each edge, whatever the history), and
//! for a kind a reader reads, the events of that kind in the reader's range
//! besides ([`range`]).

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::application::kpi::{Host, of_window as kpi_of_window};
use crate::application::report::{DiagramRenderer, ReportSetup, diagram_draft, make_with};
use crate::application::stats::{StatsSources, now_ms, of_window as stats_of_window, window_query};
use crate::domain::host_metrics::{HOST_NODE, HostSample, NodeSample, SampleSource};
use crate::domain::kpi::config::{KpiSettings, Target};
use crate::domain::kpi::report::DiagramSection;
use crate::domain::kpi::{KpiConfig, KpiQuery, Period, read_from_ms};
use crate::domain::ledger::queue::queue_rows;
use crate::domain::ledger::testkit::omitted::EdgeStore;
use crate::domain::ledger::testkit::{Log, T0, ms, queue_across, run_id};
use crate::domain::ledger::window::omitted::OmittedEdges;
use crate::domain::ledger::window::{
    LedgerWindow, WindowQuery, select_from_time_with, select_with,
};
use crate::domain::ledger::{LedgerKind, LedgerRow, build, node, owner};
use crate::domain::stats::{Cursor, StatsQuery, timestamp_millis};
use crate::domain::{EventId, RunEvent};

/// The kinds of no run and no task of the sample of the production queue
/// that no reader of the ledger names and whose payload names no
/// supervisor (whose life any event naming it reaches,
/// [`crate::domain::supervisor_life::supervisor_of`]).
const UNREAD: [&str; 3] = [
    "authorization_denied",
    "telemetry_token_issued",
    "telemetry_token_revoked",
];

const DAY: i64 = 86_400;

/// The kinds of a finding's records.
const FINDING_KINDS: [&str; 4] = [
    "finding_recorded",
    "finding_updated",
    "finding_status_changed",
    "finding_baseline",
];

/// Whether `row` is a queue row of one of `kinds`.
fn of_kinds(row: &LedgerRow, kinds: &[&str]) -> bool {
    row.kind == LedgerKind::Queue
        && row
            .subject
            .as_ref()
            .is_some_and(|subject| kinds.contains(&subject.kind.as_str()))
}

/// `count` refusals of a telemetry report at `secs`, as executor-telemetry
/// gets them for a session not on its run (the payload of
/// `commands::denial`).
fn denied(log: &mut Log, count: usize, secs: i64) {
    for n in 0..count {
        log.add(
            None,
            None,
            "authorization_denied",
            json!({"event": "authorization_denied", "role": "executor",
                   "capability": "telemetry.report", "reason": "not_on_resource",
                   "resource": {"kind": "run", "id": run_id(9)}, "n": n}),
            secs,
        );
    }
}

/// The history: a refusal first, then every kind of queue row
/// [`queue_across`] makes (a stream, an alert, a finding, a slots full, a
/// hold, a deferral, landing slots, a mark), then the attributes'
/// baseline, a supervisor `s9` started and three runs landed on three
/// days, each day with `s9`'s heartbeat, the candidates it sampled, main's
/// history and, last of `s9`'s evidence of life that day, its queue
/// service's `queue_service_running`; among them the refusals and the
/// telemetry tokens, one refusal the newest event. And the host's load,
/// one sample before the first event that is not a refusal.
fn history() -> (Vec<RunEvent>, Vec<LedgerRow>, i64) {
    let mut log = Log::default();
    denied(&mut log, 1, -100);
    for event in queue_across().0.events {
        let id = EventId::new(i64::try_from(log.events.len()).unwrap() + 1);
        log.events.push(RunEvent { id, ..event });
    }
    log.add(
        None,
        None,
        "attributes_baseline",
        json!({"tasks": 0, "goals": 0}),
        5_000,
    )
    .add(
        None,
        None,
        "supervisor_started",
        json!({"supervisor": "s9", "parallel": 2}),
        5_000,
    );
    denied(&mut log, 3, 5_001);
    // `s8`, started (with its `parallel`, so that `kpi`'s lives hold it),
    // whose last evidence of life is an update before the lower bound of
    // the readers' ranges of a day (`DAY + 500`), with none of it in them.
    log.add(
        None,
        None,
        "supervisor_started",
        json!({"supervisor": "s8", "parallel": 1}),
        DAY + 100,
    )
    .add(
        None,
        None,
        "update_started",
        json!({"pid": 42, "base": "c0", "supervisor": "s8", "version": "0.9.0",
               "log": "logs/update.log", "build_log": "logs/update-build.log"}),
        DAY + 450,
    );
    for n in 1..=3_u8 {
        let at = i64::from(n) * DAY;
        let task = 10 + i64::from(n);
        log.add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s9"}),
            at - 10,
        )
        .add(
            None,
            None,
            "candidates_sampled",
            json!({"supervisor": "s9", "candidates": n, "free_slots": 1, "ready": 2}),
            at - 5,
        )
        .add(
            None,
            None,
            "telemetry_token_issued",
            json!({"target": format!("job:{n}")}),
            at - 4,
        );
        denied(&mut log, 4, at - 3);
        log.landed(task, &run_id(10 + n), at);
        let sha = format!("c{n}");
        for event in &mut log.events {
            if event.kind.as_str() == "run_integrated"
                && event.payload.get("result_commit").is_none()
            {
                event.payload["result_commit"] = json!(sha);
            }
        }
        log.add(
            None,
            None,
            "main_commits_recorded",
            json!({"after": null, "commits": [{"sha": sha, "at": T0 + at + 900, "reverts": null}]}),
            at + 950,
        )
        .add(
            None,
            None,
            "main_observed",
            json!({"head": sha, "at": T0 + at + 950, "since": T0 - 1, "reverts": true}),
            at + 950,
        )
        .add(
            None,
            None,
            "telemetry_token_revoked",
            json!({"target": format!("job:{n}"), "reason": "ended"}),
            at + 960,
        )
        .add(
            None,
            None,
            "queue_service_running",
            json!({"supervisor": "s9", "pid": 7}),
            at + 970,
        );
        denied(&mut log, 5, at + 990);
    }
    // An update of `s7` as `host_ops::update` records it (its payload
    // names its supervisor): another supervisor's, so that `s9`'s last
    // evidence of life stays its queue service's.
    let update = |supervisor: &str, version: &str| {
        json!({"pid": 41, "base": "c3", "supervisor": supervisor, "version": version,
               "log": "logs/update.log", "build_log": "logs/update-build.log"})
    };
    log.add(
        None,
        None,
        "update_started",
        update("s7", "1.0.0"),
        3 * DAY + 2_000,
    )
    .add(
        None,
        None,
        "update_installed",
        update("s7", "1.0.1"),
        3 * DAY + 2_100,
    );
    // The newest event is a refusal.
    denied(&mut log, 1, 3 * DAY + 3_000);
    let sample = |id: i64, unix: i64| {
        let mut sample = HostSample::new(unix);
        sample.set("load1", Some(1.0 + f64::from(u8::try_from(id).unwrap())));
        NodeSample {
            resources: Default::default(),
            id,
            node_id: HOST_NODE.to_owned(),
            local_day: unix / DAY,
            source: SampleSource::Sampled,
            sample,
        }
    };
    let nodes = node::node_rows(
        &[
            sample(1, T0 - 50),
            sample(2, T0 + DAY + 100),
            sample(3, T0 + 3 * DAY + 100),
        ],
        "built",
    );
    (log.events, nodes, T0 + 3 * DAY + 4_000)
}

fn host() -> Host {
    Host {
        utc_offset_secs: 0,
        cores: Some(4),
        rust_toolchain: false,
    }
}

fn config() -> KpiConfig {
    KpiConfig::merge(
        Some(&KpiSettings {
            min_samples: Some(1),
            targets: vec![Target {
                kpi: "run_duration".into(),
                change: None,
                area: None,
                stat: None,
                min: None,
                max: Some(100.0),
            }],
            ..Default::default()
        }),
        None,
    )
}

/// What a reader reads besides the rows, given its window's query and the
/// lower bound of its own range (`--since`'s, or the time the read starts
/// from): the events a test gives the window.
type Give<'a> = &'a dyn Fn(&WindowQuery, Option<Cursor>) -> Vec<RunEvent>;

/// A point every this many events of a kind: few enough that the history
/// places several.
const EVERY: u64 = 4;

/// The store in memory of `events` with the coverage of `kinds` (C at 0,
/// S at the last event) as the builder of a set of them writes it.
fn covered(events: &[RunEvent], kinds: &[&str]) -> EdgeStore {
    let last = events.last().map_or(EventId::new(0), |event| event.id);
    EdgeStore::of(events.to_vec(), kinds, (EventId::new(0), last), EVERY)
}

/// The control's read of the events at the bounds of the window `query`
/// asks and of the range from `lower`, of every kind, in memory: at each
/// cursor and at the time of the read from a time, the event of its id, the
/// last at or before it and the first after it; the first event and the
/// newest up to now. Not shape B's read of the edges, which reads the
/// omitted events of the kinds it covers alone and near each edge: a kind a
/// reader reads is never omitted, so the control asks whether the events
/// at the bounds, whatever their kind, would give its readers their values.
fn bounds(events: &[RunEvent], query: &WindowQuery, lower: Option<Cursor>) -> Vec<RunEvent> {
    let at = |event: &RunEvent| timestamp_millis(&event.created_at).unwrap_or_default();
    let mut picked: BTreeMap<EventId, RunEvent> = BTreeMap::new();
    let mut take = |event: Option<&RunEvent>| {
        if let Some(event) = event {
            picked.insert(event.id, event.clone());
        }
    };
    let cursors = [query.since, query.until, lower]
        .into_iter()
        .flatten()
        .chain(query.read_from_ms.map(Cursor::Time));
    for cursor in cursors {
        match cursor {
            Cursor::Time(time) => {
                take(events.iter().rev().find(|event| at(event) <= time));
                take(events.iter().find(|event| at(event) > time));
            }
            Cursor::Event(id) => {
                take(events.iter().find(|event| event.id == id));
                take(events.iter().rev().find(|event| event.id <= id));
                take(events.iter().find(|event| event.id > id));
            }
        }
    }
    take(events.first());
    take(events.iter().rev().find(|event| at(event) <= query.now_ms));
    picked.into_values().collect()
}

/// The events of `kinds` in the reader's range: after `lower` (from the
/// first without it) up to the window's end, read by kind and id as
/// `MarkLog::events_of_kinds` reads them.
fn range(
    events: &[RunEvent],
    kinds: &[&str],
    query: &WindowQuery,
    lower: Option<Cursor>,
) -> Vec<RunEvent> {
    let at = |event: &RunEvent| timestamp_millis(&event.created_at).unwrap_or_default();
    let after = |event: &RunEvent| match lower {
        Some(Cursor::Time(time)) => at(event) > time,
        Some(Cursor::Event(id)) => event.id > id,
        None => true,
    };
    let before = |event: &RunEvent| match query.until {
        Some(Cursor::Time(time)) => at(event) <= time,
        Some(Cursor::Event(id)) => event.id <= id,
        None => at(event) <= query.now_ms,
    };
    events
        .iter()
        .filter(|event| kinds.contains(&event.kind.as_str()) && after(event) && before(event))
        .cloned()
        .collect()
}

/// A queue row of no kind of its own whose records are `records`: how the
/// test gives the window the events a shape reads of the EventStore.
fn given_row(template: &LedgerRow, records: Vec<RunEvent>) -> Option<LedgerRow> {
    let first = records.first()?;
    let mut row = template.clone();
    row.key = format!("given-{}", first.id);
    row.subject = None;
    row.records = records;
    Some(row)
}

/// Every reader's value of the ledger `rows` at `now`, by its name, each
/// window given what `give` reads for it.
fn values(
    rows: &[LedgerRow],
    now: i64,
    edges: &OmittedEdges<'_, Infallible>,
    give: Give<'_>,
) -> Vec<(String, Value)> {
    let template = rows
        .iter()
        .find(|row| row.kind == LedgerKind::Queue)
        .unwrap()
        .clone();
    let window = |query: WindowQuery, lower: Option<Cursor>, from_time: bool| -> LedgerWindow {
        let mut rows = rows.to_vec();
        rows.extend(given_row(&template, give(&query, lower)));
        let read = if from_time {
            select_from_time_with(rows, &query, edges)
        } else {
            select_with(rows, &query, edges)
        };
        match read {
            Ok(window) => window,
            Err(never) => match never {},
        }
    };
    let now_ms = now_ms(now);
    let mut out = Vec::new();
    // The whole window; one of times, its `--since` at a refusal; one of
    // event ids, its `--until` a refusal.
    let stats_queries = [
        StatsQuery {
            full: true,
            ..StatsQuery::default()
        },
        StatsQuery {
            since: Some(Cursor::Time(ms(2 * DAY - 3))),
            until: Some(Cursor::Time(ms(3 * DAY))),
            full: true,
            ..StatsQuery::default()
        },
        StatsQuery {
            since: Some(Cursor::Event(EventId::new(30))),
            until: Some(Cursor::Event(last_refusal_of_day(2))),
            full: true,
            ..StatsQuery::default()
        },
    ];
    let sources = StatsSources {
        config_file: &|| Ok(None),
        conflicts_file: &|| Ok(None),
        areas: None,
        utc_offset_secs: 0,
        counts_checks: true,
        rust_toolchain: false,
    };
    for (n, query) in stats_queries.iter().enumerate() {
        let read = window(window_query(query, now), query.since, false);
        let stats = stats_of_window(&read, now, query, &sources).unwrap();
        let mut value = serde_json::to_value(&stats).unwrap();
        // The window's position and its first time: the cursor a page
        // hands on, the end of the sessions' window, and the host's span.
        let position = json!({
            "next_cursor": value["next_cursor"].take(),
            "sessions.window.upto": value["sessions"]["window"]["upto"].take(),
            "host": value["host"].take(),
        });
        out.push((format!("stats[{n}]"), value));
        out.push((format!("stats[{n}].position"), position));
    }
    let whole_query = WindowQuery {
        now_ms,
        ..WindowQuery::default()
    };
    let periods = KpiQuery {
        period: Period::Day,
        last: 3,
        ..KpiQuery::default()
    };
    let whole = window(whole_query.clone(), None, false);
    let kpi = kpi_of_window(&whole, now, host(), &config(), None, &periods).unwrap();
    out.push(("kpi".into(), serde_json::to_value(&kpi).unwrap()));
    // One window of times: the reader's range is its own.
    let since = Cursor::Time(ms(DAY + 500));
    let span = KpiQuery {
        since: Some(since),
        until: Some(Cursor::Time(ms(3 * DAY))),
        ..KpiQuery::default()
    };
    let spanned = window(whole_query.clone(), Some(since), false);
    let kpi = kpi_of_window(&spanned, now, host(), &config(), None, &span).unwrap();
    out.push((
        "kpi --since --until".into(),
        serde_json::to_value(&kpi).unwrap(),
    ));
    // The check of the targets, as `push::check_breaches` reads it: a day
    // and a week, the last two, from the earlier of their times.
    let breach_queries: Vec<KpiQuery> = [Period::Day, Period::Week]
        .into_iter()
        .map(|period| KpiQuery {
            period,
            last: 2,
            ..KpiQuery::default()
        })
        .collect();
    let from = breach_queries
        .iter()
        .filter_map(|query| read_from_ms(query, &config(), now, 0))
        .min();
    let from_query = WindowQuery {
        now_ms,
        read_from_ms: from,
        ..WindowQuery::default()
    };
    let read = window(from_query, from.map(Cursor::Time), true);
    for query in &breach_queries {
        let kpi = kpi_of_window(&read, now, host(), &config(), None, query).unwrap();
        assert!(!kpi.targets.is_empty());
        out.push((
            format!("kpi.targets {} (目標割れ)", query.period.as_str()),
            serde_json::to_value(&kpi.targets).unwrap(),
        ));
    }
    let setup = ReportSetup {
        root: Default::default(),
        host: host(),
        config: config(),
        keep: Default::default(),
        build: "test".into(),
        diagram: DiagramRenderer(Arc::new(|_| unreachable!())),
        areas: None,
    };
    let section = DiagramSection {
        tasks: Vec::new(),
        d2_source: false,
        reason: Some("not drawn".into()),
        svg: None,
    };
    let report = make_with(&whole, &setup, now, Period::Day, None, section).unwrap();
    out.push(("report".into(), serde_json::to_value(&report).unwrap()));
    out.push((
        "report.diagram".into(),
        serde_json::to_value(diagram_draft(&whole)).unwrap(),
    ));
    out.push((
        "judgments (live alert)".into(),
        json!(format!("{:?}", whole.judgments())),
    ));
    out.push(("findings".into(), json!(format!("{:?}", whole.findings()))));
    out
}

/// The id of the last refusal of `day` in the history.
fn last_refusal_of_day(day: i64) -> EventId {
    history()
        .0
        .iter()
        .filter(|event| {
            event.kind.as_str() == "authorization_denied"
                && timestamp_millis(&event.created_at) == Some(ms(day * DAY + 990))
        })
        .map(|event| event.id)
        .max()
        .unwrap()
}

/// A value as the comparison's output shows it: its size and a hash of
/// it (FNV-1a of its JSON), or the value itself when it is short.
fn shown(value: &Value) -> String {
    let text = value.to_string();
    if text.len() <= 60 {
        return text;
    }
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{} bytes, fnv {hash:016x}", text.len())
}

/// The leaves of `a` and `b` that differ, by their path, at most `limit`.
fn leaves(a: &Value, b: &Value, path: &str, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit || a == b {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for key in keys {
                let none = Value::Null;
                leaves(
                    x.get(key).unwrap_or(&none),
                    y.get(key).unwrap_or(&none),
                    &format!("{path}.{key}"),
                    out,
                    limit,
                );
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (n, (x, y)) in x.iter().zip(y).enumerate() {
                leaves(x, y, &format!("{path}[{n}]"), out, limit);
            }
        }
        _ => {
            let cut = |v: &Value| v.to_string().chars().take(120).collect::<String>();
            out.push(format!("{path}: {} → {}", cut(a), cut(b)));
        }
    }
}

/// The comparison of `now` and `shape` as the plan quotes it: per value,
/// the value now and the shape's, and the leaves that differ.
fn compared(label: &str, now: &[(String, Value)], shape: &[(String, Value)]) {
    println!("## {label}");
    println!("| value | now | {label} | same |");
    for ((name, x), (_, y)) in now.iter().zip(shape) {
        println!("| {name} | {} | {} | {} |", shown(x), shown(y), x == y);
    }
    for ((name, x), (_, y)) in now.iter().zip(shape) {
        let mut out = Vec::new();
        leaves(x, y, "", &mut out, 6);
        for leaf in out {
            println!("- {name}{leaf}");
        }
    }
}

/// The names of the readers whose values of `a` and `b` differ.
fn differ(a: &[(String, Value)], b: &[(String, Value)]) -> BTreeSet<String> {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .filter(|(x, y)| x != y)
        .map(|(x, _)| x.0.clone())
        .collect()
}

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// The rows of the history: the fold of its events and the host's.
fn ledger() -> (Vec<RunEvent>, Vec<LedgerRow>, i64) {
    let (events, nodes, now) = history();
    let mut rows = build(&events, now * 1000, "built");
    rows.extend(nodes);
    (events, rows, now)
}

/// Shape A: without the queue rows of [`UNREAD`] `kpi`, `report`, the
/// targets, the judgments and the findings are the same, but each `stats`
/// differs, whose window's bounds a refusal moves (the first event, the
/// newest, an event at a cursor). Shape B: with the edges read of the
/// coverage of them ([`OmittedEdges`]), every value is the same. And the queue rows of every
/// other kind ([`crate::domain::ledger::queue::QueueBook`]'s) are the same
/// when the builder does not take those events as the queue's own but
/// reads them with every event.
#[test]
fn the_unread_kinds_left_out_of_the_queue_rows_give_every_reader_the_same() {
    let (events, whole, now) = ledger();
    let left: Vec<LedgerRow> = whole
        .iter()
        .filter(|row| !of_kinds(row, &UNREAD))
        .cloned()
        .collect();
    let refusals = events
        .iter()
        .filter(|event| event.kind.as_str() == "authorization_denied")
        .count();
    assert_eq!(refusals, 1 + 3 + 3 * 9 + 1);
    assert_eq!(whole.len() - left.len(), refusals + 3 * 2);

    let nothing = |_: &WindowQuery, _: Option<Cursor>| Vec::new();
    let none = OmittedEdges::none();
    let store = covered(&events, &UNREAD);
    let edges = OmittedEdges::of(&store).unwrap();
    let now_values = values(&whole, now, &none, &nothing);
    let a = values(&left, now, &none, &nothing);
    let b = values(&left, now, &edges, &nothing);
    compared("shape A", &now_values, &a);
    compared("shape B", &now_values, &b);
    assert_eq!(
        differ(&now_values, &a),
        names(&[
            "stats[0]",
            "stats[0].position",
            "stats[1]",
            "stats[2]",
            "stats[2].position",
        ]),
    );
    assert!(
        differ(&now_values, &b).is_empty(),
        "{:?}",
        differ(&now_values, &b)
    );
    // The input is fixed: a second read gives the same.
    assert_eq!(now_values, values(&whole, now, &none, &nothing));

    // QueueBook: the builder that does not take them as the queue's own
    // (but reads them with every event) gives the other rows the same.
    let owned: Vec<RunEvent> = events
        .iter()
        .filter(|event| owner::owner(event) == owner::Owner::Queue)
        .cloned()
        .collect();
    let taken: Vec<RunEvent> = owned
        .iter()
        .filter(|event| !UNREAD.contains(&event.kind.as_str()))
        .cloned()
        .collect();
    let rows_now: Vec<LedgerRow> = queue_rows(&owned, &events, now * 1000, "built")
        .into_iter()
        .filter(|row| !of_kinds(row, &UNREAD))
        .collect();
    assert_eq!(queue_rows(&taken, &events, now * 1000, "built"), rows_now);
}

/// The kinds a reader reads, left out of the queue rows, with the window
/// given the control's events at its bounds ([`bounds`]), and with the
/// events of the kind (its events of `event_kinds`) in each reader's range
/// besides: the readers whose values differ from the whole ledger's, each
/// way.
fn left_out(kinds: &[&str], event_kinds: &[&str]) -> (BTreeSet<String>, BTreeSet<String>) {
    let (events, whole, now) = ledger();
    let left: Vec<LedgerRow> = whole
        .iter()
        .filter(|row| !of_kinds(row, kinds))
        .cloned()
        .collect();
    assert!(left.len() < whole.len(), "{kinds:?}");
    let nothing = |_: &WindowQuery, _: Option<Cursor>| Vec::new();
    let none = OmittedEdges::none();
    let now_values = values(&whole, now, &none, &nothing);
    let at_bounds = |query: &WindowQuery, lower: Option<Cursor>| bounds(&events, query, lower);
    let in_range = |query: &WindowQuery, lower: Option<Cursor>| {
        let mut given = bounds(&events, query, lower);
        given.extend(range(&events, event_kinds, query, lower));
        given.sort_by_key(|event| event.id);
        given.dedup_by_key(|event| event.id);
        given
    };
    let bounded_values = values(&left, now, &none, &at_bounds);
    let ranged_values = values(&left, now, &none, &in_range);
    compared(&format!("{kinds:?} bounds"), &now_values, &bounded_values);
    compared(
        &format!("{kinds:?} bounds and range"),
        &now_values,
        &ranged_values,
    );
    let bounded = differ(&now_values, &bounded_values);
    let ranged = differ(&now_values, &ranged_values);
    println!("{kinds:?}: bounds {bounded:?}; bounds and range {ranged:?}");
    (bounded, ranged)
}

/// The control and the range read: the queue rows of a kind a reader
/// reads cannot be left out with only the window's bounds read. With the
/// events of the kind from each reader's lower bound read besides,
/// `main_*` gives every value the same; `kpi --since` still differs
/// without `finding` (a finding recorded before the bound is open in the
/// range), and without `candidates_sampled`, `supervisor_*` and `update_*`
/// only in its comparison with the period before and the 7-day baseline,
/// which read before the bound (`s8`'s last evidence of life is an update
/// before it). `queue_service_running`
/// changes nothing here: `s9`'s life is the same without it in this
/// history. The values that differ are in the test's output (the plan's
/// compare.txt).
#[test]
fn the_kinds_a_reader_reads_left_out_of_the_queue_rows_need_their_history() {
    let windows = names(&[
        "stats[1]",
        "stats[2]",
        "kpi --since --until",
        "kpi.targets day (目標割れ)",
        "kpi.targets week (目標割れ)",
    ]);
    let cases: [(&[&str], &[&str], &[&str]); 6] = [
        (
            &["candidates_sampled"],
            &["kpi", "kpi --since --until", "report"],
            &["kpi --since --until"],
        ),
        (
            &["update_started", "update_installed"],
            &["kpi", "kpi --since --until", "report", "stats[0]"],
            &["kpi --since --until"],
        ),
        (
            &["main_commits_recorded", "main_observed"],
            &["kpi", "kpi --since --until", "report"],
            &[],
        ),
        (
            &["supervisor_alive", "supervisor_started"],
            &["kpi", "kpi --since --until", "report"],
            &["kpi --since --until"],
        ),
        (&["queue_service_running"], &[], &[]),
        (
            &["finding"],
            &["findings", "kpi", "kpi --since --until", "report"],
            &["kpi --since --until"],
        ),
    ];
    let found: Vec<_> = cases
        .iter()
        .map(|(kinds, _, _)| {
            let event_kinds: &[&str] = if kinds == &["finding"] {
                &FINDING_KINDS
            } else {
                kinds
            };
            left_out(kinds, event_kinds)
        })
        .collect();
    for ((kinds, bounded, ranged), (b, r)) in cases.iter().zip(&found) {
        assert_eq!(b, &names(bounded), "{kinds:?}");
        assert_eq!(r, &names(ranged), "{kinds:?}");
        assert!(r.is_subset(&windows), "{kinds:?}: {r:?}");
    }
}
