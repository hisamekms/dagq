//! The bounds the readers take inside a window
//! ([`crate::domain::ledger::window::bounds`]) of a ledger without the
//! queue rows of the omitted events of (C, S] give the values of every
//! row read by the functions before the bounds were declared (a window
//! whose query declares none, of the events as before): `kpi`'s periods,
//! their comparison, the 7 days' baseline and the window before
//! `--since`, `report`, the check of the targets, `stats` without
//! `--full` and `--since`, and `kpi`'s event id cursors, when a clock
//! turned back puts an omitted event at a bound.

use std::convert::Infallible;
use std::sync::Arc;

use serde_json::{Value, json};

use super::kpi::{self, Host};
use super::report::{self, DiagramRenderer, ReportSetup};
use super::stats::{self, StatsSources};
use crate::domain::EventId;
use crate::domain::kpi::config::{KpiSettings, Target};
use crate::domain::kpi::report::DiagramSection;
use crate::domain::kpi::{CompareSpec, DAY_MS, KpiConfig, KpiQuery, Period, read_from_ms};
use crate::domain::ledger::testkit::omitted::EdgeStore;
use crate::domain::ledger::testkit::{Log, T0, ms, run_id};
use crate::domain::ledger::window::omitted::OmittedEdges;
use crate::domain::ledger::window::{
    LedgerWindow, WindowQuery, select, select_from_time, select_from_time_with, select_with,
};
use crate::domain::ledger::{LedgerKind, LedgerRow, build};
use crate::domain::marks::utc_text;
use crate::domain::stats::{Cursor, StatsQuery};

const KIND: &str = "authorization_denied";
const EVERY: u64 = 4;
const DAY: i64 = DAY_MS / 1000;

fn ok<T>(result: Result<T, Infallible>) -> T {
    match result {
        Ok(value) => value,
        Err(never) => match never {},
    }
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

fn setup() -> ReportSetup {
    ReportSetup {
        root: Default::default(),
        host: host(),
        config: config(),
        keep: Default::default(),
        build: "test".into(),
        diagram: DiagramRenderer(Arc::new(|_| unreachable!())),
        areas: None,
    }
}

fn section() -> DiagramSection {
    DiagramSection {
        tasks: Vec::new(),
        d2_source: false,
        reason: Some("not drawn".into()),
        svg: None,
    }
}

/// A queue's ledger two ways: every row (`whole`), and without the queue
/// rows of the omitted events of (C, S] with the coverage that stands for
/// them (`left`, `store`).
struct Ledgers {
    whole: Vec<LedgerRow>,
    left: Vec<LedgerRow>,
    store: EdgeStore,
    now: i64,
}

impl Ledgers {
    fn of(log: &Log, (c, s): (i64, i64), now: i64) -> Self {
        let whole = build(&log.events, now * 1000, "built");
        let (c, s) = (EventId::new(c), EventId::new(s));
        let left: Vec<LedgerRow> = whole
            .iter()
            .filter(|row| {
                row.kind != LedgerKind::Queue
                    || row
                        .subject
                        .as_ref()
                        .is_none_or(|subject| subject.kind.as_str() != KIND)
                    || row
                        .records
                        .iter()
                        .any(|event| event.id <= c || event.id > s)
            })
            .cloned()
            .collect();
        assert!(left.len() < whole.len(), "some queue rows are omitted");
        Self {
            store: EdgeStore::of(log.events.clone(), &[KIND], (c, s), EVERY),
            whole,
            left,
            now,
        }
    }

    /// The window of `query` of every row, read as before the bounds (its
    /// query declares none: what it takes inside is of the events).
    fn before(&self, query: &WindowQuery) -> LedgerWindow {
        let query = WindowQuery {
            bounds: Default::default(),
            ..query.clone()
        };
        if query.read_from_ms.is_some() {
            select_from_time(self.whole.clone(), &query)
        } else {
            select(self.whole.clone(), &query)
        }
    }

    /// The window of `query` of the rows left and the coverage, its bounds
    /// declared and answered.
    fn after(&self, query: &WindowQuery) -> LedgerWindow {
        let edges = ok(OmittedEdges::of(&self.store));
        if query.read_from_ms.is_some() {
            ok(select_from_time_with(self.left.clone(), query, &edges))
        } else {
            ok(select_with(self.left.clone(), query, &edges))
        }
    }

    /// Of the rows left, declaring no bounds: the values a bound worked out
    /// of the events gives.
    fn undeclared(&self, query: &WindowQuery) -> LedgerWindow {
        self.after(&WindowQuery {
            bounds: Default::default(),
            ..query.clone()
        })
    }

    /// The window `kpi` of `queries` reads, as `application::kpi::kpi`
    /// (and the check of the targets, of the earliest time of its
    /// queries) asks for it.
    fn kpi_query(&self, queries: &[KpiQuery]) -> WindowQuery {
        WindowQuery {
            now_ms: stats::now_ms(self.now),
            read_from_ms: queries
                .iter()
                .map(|query| read_from_ms(query, &config(), self.now, 0))
                .min()
                .flatten(),
            bounds: kpi::declared(queries.to_vec(), self.now, host(), &config(), None),
            ..WindowQuery::default()
        }
    }

    /// The KPIs of `query` of `window`.
    fn kpi(&self, window: &LedgerWindow, query: &KpiQuery) -> Value {
        serde_json::to_value(
            kpi::of_window(window, self.now, host(), &config(), None, query).unwrap(),
        )
        .unwrap()
    }

    /// The values of `kpi` of `query` (every period's and its comparison's),
    /// before and after.
    fn kpis(&self, query: &KpiQuery) -> (Value, Value) {
        let window = self.kpi_query(std::slice::from_ref(query));
        (
            self.kpi(&self.before(&window), query),
            self.kpi(&self.after(&window), query),
        )
    }

    /// The report of `period` holding `at`, before and after.
    fn report(&self, period: Period, at: Option<Cursor>) -> (Value, Value) {
        let window = self.kpi_query(&[report::report_query(period, at)]);
        let of = |window: &LedgerWindow| {
            serde_json::to_value(
                report::make_with(window, &setup(), self.now, period, at, section()).unwrap(),
            )
            .unwrap()
        };
        (of(&self.before(&window)), of(&self.after(&window)))
    }

    /// The check of the targets of a day and a week, the last two, read
    /// from the earlier of their times as `application::push` reads them,
    /// before and after.
    fn targets(&self) -> (Value, Value) {
        let checks: Vec<KpiQuery> = [Period::Day, Period::Week]
            .into_iter()
            .map(|period| KpiQuery {
                period,
                last: 2,
                ..KpiQuery::default()
            })
            .collect();
        let window = self.kpi_query(&checks);
        let of = |window: &LedgerWindow| {
            Value::Array(
                checks
                    .iter()
                    .map(|query| {
                        let kpi = kpi::of_window(window, self.now, host(), &config(), None, query)
                            .unwrap();
                        serde_json::to_value(kpi.targets).unwrap()
                    })
                    .collect(),
            )
        };
        (of(&self.before(&window)), of(&self.after(&window)))
    }

    /// `stats` of `query`: its `window_ms`, `host` and
    /// `landing_utilization`, and the whole output, before and after.
    fn stats(&self, query: &StatsQuery) -> (Value, Value) {
        let sources = StatsSources {
            config_file: &|| Ok(None),
            conflicts_file: &|| Ok(None),
            areas: None,
            utc_offset_secs: 0,
            counts_checks: true,
            rust_toolchain: false,
        };
        let window = stats::window_query(query, self.now);
        let of = |window: &LedgerWindow| {
            let stats = stats::of_window(window, self.now, query, &sources).unwrap();
            json!({
                "window_ms": stats.window_ms,
                "host": stats.host,
                "landing_utilization": stats.landing_utilization,
                "stats": stats,
            })
        };
        (of(&self.before(&window)), of(&self.after(&window)))
    }
}

/// The secs of midnight after `T0` (UTC).
const MIDNIGHT: i64 = DAY - T0.rem_euclid(DAY);

/// A queue whose clock turned back at the midnight after T0: the queue's
/// refusal before midnight, then run 2's life from just past it, then a
/// refusal of a time before midnight; and a refusal before run 1's life of
/// a time before every other event, and one right after it of a time
/// before every other but that one. So the last event at or
/// before midnight (each day's bound, the 7 days' baseline's, the window
/// before `--since`, a comparison's) is that refusal, past run 2's events
/// by id (a later refusal is the latest), and the first time of the page of `stats` (from run 1's
/// claim) is the second refusal's, not the window's first.
fn turned_back() -> Log {
    let mut log = Log::default();
    log.add(None, None, KIND, json!({}), -800);
    log.landed(1, &run_id(1), 0);
    log.add(None, None, KIND, json!({}), -500);
    log.add(None, None, KIND, json!({}), MIDNIGHT - 10);
    log.landed(2, &run_id(2), MIDNIGHT + 5);
    log.add(None, None, KIND, json!({}), MIDNIGHT - 5);
    log.landed(3, &run_id(3), MIDNIGHT + 2000);
    log.add(None, None, KIND, json!({}), MIDNIGHT + 3500);
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s1"}),
        MIDNIGHT + 4000,
    );
    log
}

/// `kpi`'s periods and their bounds, the 7 days' baseline, the window
/// before `--since`, a comparison, `report`, the check of the targets and
/// `stats` without `--full` and `--since` (its `window_ms`, `host` and
/// `landing_utilization`) of the ledger without the omitted events' rows
/// are those of every row as the functions read them before, though the
/// clock turned back at their bounds; worked out of the events of the
/// rows left, as before, they are not.
#[test]
fn the_readers_bounds_inside_the_window_give_the_values_of_every_row() {
    let log = turned_back();
    let s = log.last().as_i64() - 1;
    let ledgers = Ledgers::of(&log, (0, s), T0 + MIDNIGHT + 5000);
    let midnight = ms(MIDNIGHT);
    let queries = [
        KpiQuery {
            period: Period::Day,
            last: 3,
            ..KpiQuery::default()
        },
        KpiQuery {
            period: Period::Week,
            last: 1,
            ..KpiQuery::default()
        },
        KpiQuery {
            since: Some(Cursor::Time(midnight)),
            until: Some(Cursor::Time(midnight + 4_000_000)),
            ..KpiQuery::default()
        },
        KpiQuery {
            until: Some(Cursor::Time(midnight + 4_000_000)),
            ..KpiQuery::default()
        },
        KpiQuery {
            period: Period::Day,
            last: 1,
            compare: Some(CompareSpec::Windows([
                (Cursor::Time(midnight - DAY_MS), Cursor::Time(midnight)),
                (Cursor::Time(midnight), Cursor::Time(midnight + DAY_MS)),
            ])),
            ..KpiQuery::default()
        },
        KpiQuery {
            period: Period::Day,
            last: 1,
            compare: Some(CompareSpec::At(Cursor::Time(midnight))),
            window_days: 1,
            ..KpiQuery::default()
        },
    ];
    let mut moved = false;
    for query in &queries {
        let (before, after) = ledgers.kpis(query);
        assert_eq!(after, before, "{query:?}");
        let undeclared = ledgers.kpi(
            &ledgers.undeclared(&ledgers.kpi_query(std::slice::from_ref(query))),
            query,
        );
        moved |= undeclared != before;
    }
    assert!(moved, "the input puts an omitted event at a bound");
    let (before, after) = ledgers.report(Period::Day, None);
    assert_eq!(after, before);
    let (before, after) = ledgers.report(Period::Day, Some(Cursor::Time(midnight + 1000)));
    assert_eq!(after, before);
    let (before, after) = ledgers.targets();
    assert_eq!(after, before);
    let page = StatsQuery::default();
    let (before, after) = ledgers.stats(&page);
    assert_eq!(before["window_ms"][0], json!(ms(-500)));
    assert_eq!(after, before);
    let undeclared = ledgers.undeclared(&stats::window_query(&page, ledgers.now));
    let sources = StatsSources {
        config_file: &|| Ok(None),
        conflicts_file: &|| Ok(None),
        areas: None,
        utc_offset_secs: 0,
        counts_checks: true,
        rust_toolchain: false,
    };
    let of_events = stats::of_window(&undeclared, ledgers.now, &page, &sources).unwrap();
    assert_ne!(of_events.window_ms.0, ms(-500));
    // `--until` too.
    for until in [
        Cursor::Time(midnight + 3000),
        Cursor::Event(EventId::new(s - 3)),
    ] {
        let (before, after) = ledgers.stats(&StatsQuery {
            until: Some(until),
            ..StatsQuery::default()
        });
        assert_eq!(after, before, "{until:?}");
    }
}

/// The queue's refusals at `times` (days after T0, `None` of no time),
/// then `more` landed runs, a run of no row's token after the first two
/// refusals, and an event of a row after them all.
fn refusals(times: &[Option<i64>]) -> (Log, EventId, EventId) {
    let mut log = Log::default();
    let mut token = EventId::new(0);
    for (n, time) in times.iter().enumerate() {
        log.add(None, None, KIND, json!({}), time.unwrap_or(0) * DAY);
        if time.is_none() {
            log.events.last_mut().unwrap().created_at = "not a time".into();
        }
        if n == 1 {
            log.add(
                None,
                Some(&run_id(9)),
                "telemetry_token_issued",
                json!({}),
                times[1].unwrap_or(0) * DAY + 1,
            );
            token = log.last();
        }
    }
    log.landed(1, &run_id(1), 3 * DAY);
    log.landed(2, &run_id(2), 13 * DAY);
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s1"}),
        35 * DAY,
    );
    let row = log.last();
    (log, token, row)
}

/// The queries of `kpi` an event id cursor `u` stands in: `--since`,
/// `--until`, `--at` and each side of `--compare`'s windows.
fn cursor_queries(u: EventId) -> Vec<(&'static str, KpiQuery)> {
    let early = Cursor::Time(ms(-DAY));
    let late = Cursor::Time(ms(40 * DAY));
    vec![
        (
            "--since",
            KpiQuery {
                since: Some(Cursor::Event(u)),
                ..KpiQuery::default()
            },
        ),
        (
            "--until",
            KpiQuery {
                since: Some(early),
                until: Some(Cursor::Event(u)),
                ..KpiQuery::default()
            },
        ),
        (
            "--at",
            KpiQuery {
                period: Period::Day,
                last: 2,
                at: Some(Cursor::Event(u)),
                ..KpiQuery::default()
            },
        ),
        (
            "--compare",
            KpiQuery {
                period: Period::Day,
                last: 1,
                compare: Some(CompareSpec::Windows([
                    (early, Cursor::Event(u)),
                    (Cursor::Event(u), late),
                ])),
                ..KpiQuery::default()
            },
        ),
    ]
}

/// `kpi`'s event id cursors (`--since`, `--until`, `--at`, `--compare`'s)
/// stand for the latest time of the events of an id at or before theirs
/// with the omitted events' rows or without them: of the queue's refusals
/// (1, B), (2, B + 10), (3, B + 20) `--since 2` is B + 10, not B; of (1, B +
/// 20), (2, B), (3, B + 30) it is B + 20, of the id before the one before
/// it. So the KPIs, the report and the check of the targets of the periods
/// they make are those of every row read as before, for an id of an
/// omitted event, of no row (a run's token), at or before C, past S and of
/// no time.
#[test]
fn kpis_event_id_cursors_stand_for_the_times_of_every_row() {
    let b = 0;
    let rising = [Some(b), Some(b + 10), Some(b + 20)];
    let back = [Some(b + 20), Some(b), Some(b + 30)];
    let timeless = [Some(b + 20), Some(b), None, Some(b + 30)];
    for (times, since_2) in [
        (&rising[..], Some(b + 10)),
        (&back[..], Some(b + 20)),
        (&timeless[..], None),
    ] {
        let (log, token, row) = refusals(times);
        let last_refusal = EventId::new(i64::try_from(times.len()).unwrap() + 1);
        for c in [0, 1] {
            let ledgers = Ledgers::of(&log, (c, last_refusal.as_i64()), T0 + 50 * DAY);
            let refusal_2 = EventId::new(2);
            let mut ids = vec![EventId::new(1), refusal_2, token, last_refusal, row];
            ids.extend([EventId::new(3), EventId::new(row.as_i64() + 5)]);
            for u in ids {
                for (flag, query) in cursor_queries(u) {
                    let (before, after) = ledgers.kpis(&query);
                    assert_eq!(after, before, "{times:?} c {c} {flag} {u:?}");
                    if flag == "--since"
                        && u == refusal_2
                        && c == 0
                        && let Some(days) = since_2
                    {
                        assert_eq!(
                            before["periods"][0]["start"],
                            json!(utc_text(ms(days * DAY))),
                        );
                    }
                }
                let (before, after) = ledgers.report(Period::Day, Some(Cursor::Event(u)));
                assert_eq!(after, before, "{times:?} c {c} report --at {u:?}");
            }
        }
    }
}
