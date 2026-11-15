//! The readers' values of the inputs of
//! [`crate::domain::ledger::testkit::frozen`], fixed as the functions of
//! main at its [`FIXED_AT`] give them of the ledger the builder of then
//! made: a task that changes how the ledger is built or a window read
//! compares its values with these ([`check`], [`differs`]), so that what it keeps the
//! same is the same as before it, not only between its new functions.
//!
//! [`values`] reads a store through its [`LedgerStore::ledger_window`], as
//! `stats`, `kpi`, `report` and the check of the targets read it, and
//! gives each value by its name: for each `stats` (the whole window, each
//! time cursor of the input as `--since`, each event id as `--until`) its
//! output and its position (`next_cursor`, `window_ms`, and the window's
//! `after`, `upto`, `start_ms` and `end_ms`: the answer of a time cursor
//! is its `after`, the latest event the whole window's `upto`, the first
//! time of a window without `--since` its `window_ms`' first); `kpi` of
//! the last three days and with each time as `--since`; the check of the
//! targets (a day and a week, read from a time as
//! `application::push::check_breaches` reads them); `report`; for each
//! time, the last of the queue's records at or before it that a read from
//! it stops the rows left out at (`queue_before`,
//! [`crate::domain::ledger::window::WindowRows::rows_from`]); and the
//! queue rows the store holds (the [`crate::domain::ledger::queue::QueueBook`]'s
//! rows), each whole.
//!
//! A value is fixed as its JSON when short, else as its size and the
//! FNV-1a of its JSON ([`fixed`]); to see one whole, print [`values`] at
//! [`FIXED_AT`]. A later task runs the builder's steps
//! of each input on its store and calls [`check`] with the [`values`] of
//! every input;
//! when a value differs on purpose, the change is the task's to show.

use serde_json::{Value, json};
use std::sync::Arc;

use crate::application::kpi::{self, Host};
use crate::application::report::{self, DiagramRenderer, ReportSetup, make_with};
use crate::application::stats::{self, StatsSources, now_ms, window_query};
use crate::domain::kpi::config::{KpiSettings, Target};
use crate::domain::kpi::report::DiagramSection;
use crate::domain::kpi::{KpiConfig, KpiQuery, Period, read_from_ms};
use crate::domain::ledger::LedgerKind;
use crate::domain::ledger::testkit::frozen::{FIXED_AT, Input};
use crate::domain::ledger::testkit::{T0, ms};
use crate::domain::ledger::window::WindowQuery;
use crate::domain::stats::{Cursor, StatsQuery};

use super::LedgerStore;

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

/// The readers' values of the ledger `store` holds, read at `input`'s now,
/// each by its name.
pub(crate) fn values(store: &(impl LedgerStore + ?Sized), input: &Input) -> Vec<(String, Value)> {
    let now = T0 + input.now;
    let mut out = Vec::new();
    let sources = StatsSources {
        config_file: &|| Ok(None),
        conflicts_file: &|| Ok(None),
        areas: None,
        utc_offset_secs: 0,
        counts_checks: true,
        rust_toolchain: false,
    };
    let mut queries = vec![("stats".to_owned(), StatsQuery::default())];
    for time in &input.times {
        let since = Some(Cursor::Time(ms(*time)));
        queries.push((
            format!("stats --since t{time}"),
            StatsQuery {
                since,
                ..StatsQuery::default()
            },
        ));
    }
    for until in &input.untils {
        let until_at = Some(Cursor::Event(*until));
        queries.push((
            format!("stats --until {}", until.as_i64()),
            StatsQuery {
                until: until_at,
                ..StatsQuery::default()
            },
        ));
    }
    for (name, mut query) in queries {
        query.full = true;
        let window = store.ledger_window(&window_query(&query, now)).unwrap();
        let stats = stats::of_window(&window, now, &query, &sources).unwrap();
        out.push((
            format!("{name} position"),
            json!({"next_cursor": stats.next_cursor.as_i64(), "window_ms": stats.window_ms,
                   "after": window.after.as_i64(), "upto": window.upto.as_i64(),
                   "start_ms": window.start_ms, "end_ms": window.end_ms}),
        ));
        out.push((name, serde_json::to_value(&stats).unwrap()));
    }
    let days = KpiQuery {
        period: Period::Day,
        last: 3,
        ..KpiQuery::default()
    };
    let whole = kpi::kpi(store, now, host(), &config(), None, &days).unwrap();
    out.push(("kpi".into(), serde_json::to_value(&whole).unwrap()));
    for time in &input.times {
        let since = KpiQuery {
            since: Some(Cursor::Time(ms(*time))),
            ..KpiQuery::default()
        };
        let kpi = kpi::kpi(store, now, host(), &config(), None, &since).unwrap();
        out.push((
            format!("kpi --since t{time}"),
            serde_json::to_value(&kpi).unwrap(),
        ));
    }
    // The check of the targets: a day and a week, the last two, read from
    // the earlier of their times.
    let checks: Vec<KpiQuery> = [Period::Day, Period::Week]
        .into_iter()
        .map(|period| KpiQuery {
            period,
            last: 2,
            ..KpiQuery::default()
        })
        .collect();
    let from = checks
        .iter()
        .filter_map(|query| read_from_ms(query, &config(), now, 0))
        .min();
    let read = store
        .ledger_window(&WindowQuery {
            now_ms: now_ms(now),
            read_from_ms: from,
            bounds: kpi::declared(checks.clone(), now, host(), &config(), None),
            ..WindowQuery::default()
        })
        .unwrap();
    for query in &checks {
        let kpi = kpi::of_window(&read, now, host(), &config(), None, query).unwrap();
        out.push((
            format!("targets {}", query.period.as_str()),
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
    let window = report::window(
        store,
        now,
        &setup,
        vec![report::report_query(Period::Day, None)],
    )
    .unwrap();
    let report = make_with(&window, &setup, now, Period::Day, None, section).unwrap();
    out.push(("report".into(), serde_json::to_value(&report).unwrap()));
    for time in &input.times {
        let query = WindowQuery {
            now_ms: now_ms(now),
            read_from_ms: Some(ms(*time)),
            ..WindowQuery::default()
        };
        let before = super::rows_from_of(store, &query)
            .unwrap()
            .map(|from| from.inputs_through.as_i64());
        out.push((format!("queue_before t{time}"), json!(before)));
    }
    let queue: Vec<Value> = store
        .ledger_rows(LedgerKind::Queue)
        .unwrap()
        .iter()
        .map(|row| serde_json::to_value(row).unwrap())
        .collect();
    out.push(("queue rows".into(), Value::Array(queue)));
    out
}

/// A value as it is fixed: its JSON when at most 160 bytes, else its size
/// and the FNV-1a of its JSON.
pub(crate) fn fixed(value: &Value) -> String {
    let text = value.to_string();
    if text.len() <= 160 {
        return text;
    }
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{} bytes, fnv {hash:016x}", text.len())
}

/// Whether the values of `input` differ from those fixed: if they do, the
/// values now in the table's form.
pub(crate) fn differs(input: &Input, values: &[(String, Value)]) -> Option<String> {
    let now: Vec<(String, String)> = values
        .iter()
        .map(|(name, value)| (name.clone(), fixed(value)))
        .collect();
    let expected = FIXED
        .iter()
        .find(|(name, _)| *name == input.name)
        .map_or(&[][..], |(_, values)| *values);
    let same = now.len() == expected.len()
        && now
            .iter()
            .zip(expected)
            .all(|((a, x), (b, y))| a == b && x == y);
    (!same).then(|| {
        let table: Vec<String> = now
            .iter()
            .map(|(name, value)| format!("        ({name:?}, {value:?}),"))
            .collect();
        format!(
            "the values of {:?} are not those of {FIXED_AT}; now:\n{}",
            input.name,
            table.join("\n")
        )
    })
}

/// The values of every input of `each` (its name and its values) are
/// those fixed, and no input fixed is left out.
#[track_caller]
pub(crate) fn check(each: &[(&Input, Vec<(String, Value)>)]) {
    let differing: Vec<String> = each
        .iter()
        .filter_map(|(input, values)| differs(input, values))
        .collect();
    assert!(differing.is_empty(), "{}", differing.join("\n"));
    let names: Vec<&str> = each.iter().map(|(input, _)| input.name).collect();
    let fixed: Vec<&str> = FIXED.iter().map(|(name, _)| *name).collect();
    assert_eq!(names, fixed);
}

/// The values fixed, by input and name.
const FIXED: &[(&str, &[(&str, &str)])] = &[
    (
        "reached",
        &[
            (
                "stats position",
                "{\"after\":0,\"end_ms\":1901000002999,\"next_cursor\":53,\"start_ms\":null,\"upto\":53,\"window_ms\":[1900000000000,1901000002000]}",
            ),
            ("stats", "43215 bytes, fnv 09eff9f00648167c"),
            (
                "stats --since t5 position",
                "{\"after\":6,\"end_ms\":1901000002999,\"next_cursor\":53,\"start_ms\":1900000005000,\"upto\":53,\"window_ms\":[1900000005000,1901000002000]}",
            ),
            ("stats --since t5", "43236 bytes, fnv ccba660667070a6f"),
            (
                "stats --since t960 position",
                "{\"after\":27,\"end_ms\":1901000002999,\"next_cursor\":53,\"start_ms\":1900000960000,\"upto\":53,\"window_ms\":[1900000960000,1901000002000]}",
            ),
            ("stats --since t960", "41668 bytes, fnv 117080f496f1a0dd"),
            (
                "stats --since t3000 position",
                "{\"after\":52,\"end_ms\":1901000002999,\"next_cursor\":53,\"start_ms\":1900003000000,\"upto\":53,\"window_ms\":[1900003000000,1901000002000]}",
            ),
            ("stats --since t3000", "17709 bytes, fnv 87753621d6105375"),
            (
                "stats --until 34 position",
                "{\"after\":0,\"end_ms\":1900002005000,\"next_cursor\":34,\"start_ms\":null,\"upto\":34,\"window_ms\":[1900000000000,1900002005000]}",
            ),
            ("stats --until 34", "41642 bytes, fnv 3793641a97e5a794"),
            (
                "stats --until 53 position",
                "{\"after\":0,\"end_ms\":1900003001000,\"next_cursor\":53,\"start_ms\":null,\"upto\":53,\"window_ms\":[1900000000000,1900003001000]}",
            ),
            ("stats --until 53", "43216 bytes, fnv 22045b9204f19f9b"),
            ("kpi", "109424 bytes, fnv 5a1c3fbf922b1a12"),
            ("kpi --since t5", "46449 bytes, fnv f96f9b1acb04cbff"),
            ("kpi --since t960", "46289 bytes, fnv 47f02117036dd079"),
            ("kpi --since t3000", "36149 bytes, fnv 2308bee14f0d5512"),
            ("targets day", "342 bytes, fnv 78ed68f51331a953"),
            ("targets week", "334 bytes, fnv 81a4e0b9de16f6ea"),
            ("report", "256427 bytes, fnv 20563ee08154868e"),
            ("queue_before t5", "6"),
            ("queue_before t960", "27"),
            ("queue_before t3000", "52"),
            ("queue rows", "9201 bytes, fnv 85cccee52451fd61"),
        ],
    ),
    (
        "cut",
        &[
            (
                "stats position",
                "{\"after\":0,\"end_ms\":1901000001999,\"next_cursor\":1385,\"start_ms\":null,\"upto\":1385,\"window_ms\":[1900000000000,1901000001000]}",
            ),
            ("stats", "147361 bytes, fnv 344de440dc67eacf"),
            (
                "stats --since t2000 position",
                "{\"after\":31,\"end_ms\":1901000001999,\"next_cursor\":1385,\"start_ms\":1900002000000,\"upto\":1385,\"window_ms\":[1900002000000,1901000001000]}",
            ),
            ("stats --since t2000", "145892 bytes, fnv ad446ac3a1ce4595"),
            (
                "stats --since t9001 position",
                "{\"after\":1381,\"end_ms\":1901000001999,\"next_cursor\":1385,\"start_ms\":1900009001000,\"upto\":1385,\"window_ms\":[1900009001000,1901000001000]}",
            ),
            ("stats --since t9001", "17715 bytes, fnv 49a67b024b470726"),
            (
                "stats --until 1383 position",
                "{\"after\":0,\"end_ms\":1900009002000,\"next_cursor\":1383,\"start_ms\":null,\"upto\":1383,\"window_ms\":[1900000000000,1900009002000]}",
            ),
            ("stats --until 1383", "147357 bytes, fnv e6a3709e3fccb8f5"),
            ("kpi", "109424 bytes, fnv 18e31e586cee31b6"),
            ("kpi --since t2000", "46009 bytes, fnv cc1bf932d3f24bf6"),
            ("kpi --since t9001", "36157 bytes, fnv affdb08fc4521c98"),
            ("targets day", "342 bytes, fnv 78ed68f51331a953"),
            ("targets week", "334 bytes, fnv 81a4e0b9de16f6ea"),
            ("report", "256425 bytes, fnv 08705a41316ab583"),
            ("queue_before t2000", "28"),
            ("queue_before t9001", "1381"),
            ("queue rows", "41775 bytes, fnv a7b6f8f7529ffcf9"),
        ],
    ),
    (
        "on its way",
        &[
            (
                "stats position",
                "{\"after\":0,\"end_ms\":1901000001999,\"next_cursor\":1391,\"start_ms\":null,\"upto\":1391,\"window_ms\":[1900000000000,1901000001000]}",
            ),
            ("stats", "147361 bytes, fnv 89048db7590c0c4c"),
            (
                "stats --since t1500 position",
                "{\"after\":34,\"end_ms\":1901000001999,\"next_cursor\":1391,\"start_ms\":1900001500000,\"upto\":1391,\"window_ms\":[1900001500000,1901000001000]}",
            ),
            ("stats --since t1500", "145894 bytes, fnv 0dae3851eb7ca5f3"),
            (
                "stats --since t9005 position",
                "{\"after\":1390,\"end_ms\":1901000001999,\"next_cursor\":1391,\"start_ms\":1900009005000,\"upto\":1391,\"window_ms\":[1900009005000,1901000001000]}",
            ),
            ("stats --since t9005", "17720 bytes, fnv e15e919b5536431a"),
            (
                "stats --until 1390 position",
                "{\"after\":0,\"end_ms\":1900009000000,\"next_cursor\":1390,\"start_ms\":null,\"upto\":1390,\"window_ms\":[1900000000000,1900009000000]}",
            ),
            ("stats --until 1390", "147359 bytes, fnv fc8732d87075f3f4"),
            ("kpi", "109430 bytes, fnv beae0583370a928a"),
            ("kpi --since t1500", "46001 bytes, fnv e740e906c16c4716"),
            ("kpi --since t9005", "36165 bytes, fnv 8bd8cae6bdc8382e"),
            ("targets day", "342 bytes, fnv 78ed68f51331a953"),
            ("targets week", "334 bytes, fnv 81a4e0b9de16f6ea"),
            ("report", "256439 bytes, fnv 2456b2931814e817"),
            ("queue_before t1500", "28"),
            ("queue_before t9005", "28"),
            ("queue rows", "40038 bytes, fnv e277bac7628fffbd"),
        ],
    ),
    (
        "store ahead",
        &[
            (
                "stats position",
                "{\"after\":0,\"end_ms\":1901000001999,\"next_cursor\":30,\"start_ms\":null,\"upto\":30,\"window_ms\":[1900000000000,1901000001000]}",
            ),
            ("stats", "40801 bytes, fnv 4c7e7fc188297a70"),
            (
                "stats --since t1000 position",
                "{\"after\":29,\"end_ms\":1901000001999,\"next_cursor\":30,\"start_ms\":1900001000000,\"upto\":30,\"window_ms\":[1900001000000,1901000001000]}",
            ),
            ("stats --since t1000", "17709 bytes, fnv cb69bb631f441368"),
            (
                "stats --since t2001 position",
                "{\"after\":30,\"end_ms\":1901000001999,\"next_cursor\":30,\"start_ms\":1900002001000,\"upto\":30,\"window_ms\":[1900002001000,1901000001000]}",
            ),
            ("stats --since t2001", "17709 bytes, fnv 9952aa6f4715a92c"),
            (
                "stats --until 29 position",
                "{\"after\":0,\"end_ms\":1900001000000,\"next_cursor\":29,\"start_ms\":null,\"upto\":29,\"window_ms\":[1900000000000,1900001000000]}",
            ),
            ("stats --until 29", "40797 bytes, fnv cd0e695943207cae"),
            (
                "stats --until 32 position",
                "{\"after\":0,\"end_ms\":1900001001000,\"next_cursor\":30,\"start_ms\":null,\"upto\":30,\"window_ms\":[1900000000000,1900001001000]}",
            ),
            ("stats --until 32", "40797 bytes, fnv a5825535a361ce07"),
            (
                "stats --until 53 position",
                "{\"after\":0,\"end_ms\":1900001001000,\"next_cursor\":30,\"start_ms\":null,\"upto\":30,\"window_ms\":[1900000000000,1900001001000]}",
            ),
            ("stats --until 53", "40797 bytes, fnv a5825535a361ce07"),
            ("kpi", "109424 bytes, fnv 18e31e586cee31b6"),
            ("kpi --since t1000", "36147 bytes, fnv 273a9ce67cc48242"),
            ("kpi --since t2001", "36151 bytes, fnv 0f662098ae517c62"),
            ("targets day", "342 bytes, fnv 78ed68f51331a953"),
            ("targets week", "334 bytes, fnv 81a4e0b9de16f6ea"),
            ("report", "256427 bytes, fnv 1feaabe9312a4675"),
            ("queue_before t1000", "29"),
            ("queue_before t2001", "30"),
            ("queue rows", "8638 bytes, fnv 7bbff743c2cd9c74"),
        ],
    ),
    (
        "clock back",
        &[
            (
                "stats position",
                "{\"after\":0,\"end_ms\":1901000001999,\"next_cursor\":33,\"start_ms\":null,\"upto\":33,\"window_ms\":[1900000000000,1901000001000]}",
            ),
            ("stats", "40801 bytes, fnv 83825f4b8b231086"),
            (
                "stats --since t1420 position",
                "{\"after\":31,\"end_ms\":1901000001999,\"next_cursor\":33,\"start_ms\":1900001420000,\"upto\":33,\"window_ms\":[1900001420000,1901000001000]}",
            ),
            ("stats --since t1420", "17709 bytes, fnv a3e98effc529f235"),
            (
                "stats --since t1480 position",
                "{\"after\":33,\"end_ms\":1901000001999,\"next_cursor\":33,\"start_ms\":1900001480000,\"upto\":33,\"window_ms\":[1900001480000,1901000001000]}",
            ),
            ("stats --since t1480", "17709 bytes, fnv 89034f49b6fe596f"),
            (
                "stats --since t1999 position",
                "{\"after\":33,\"end_ms\":1901000001999,\"next_cursor\":33,\"start_ms\":1900001999000,\"upto\":33,\"window_ms\":[1900001999000,1901000001000]}",
            ),
            ("stats --since t1999", "17709 bytes, fnv ab1a86fe99144992"),
            (
                "stats --since t2050 position",
                "{\"after\":33,\"end_ms\":1901000001999,\"next_cursor\":33,\"start_ms\":1900002050000,\"upto\":33,\"window_ms\":[1900002050000,1901000001000]}",
            ),
            ("stats --since t2050", "17709 bytes, fnv a880edbea01efa55"),
            (
                "stats --until 30 position",
                "{\"after\":0,\"end_ms\":1900001500000,\"next_cursor\":30,\"start_ms\":null,\"upto\":30,\"window_ms\":[1900000000000,1900001500000]}",
            ),
            ("stats --until 30", "40802 bytes, fnv 9fe5d961bd80930a"),
            ("kpi", "109424 bytes, fnv 18e31e586cee31b6"),
            ("kpi --since t1420", "36147 bytes, fnv f4824f9b914ca9a6"),
            ("kpi --since t1480", "36147 bytes, fnv 3e4e5a455222c709"),
            ("kpi --since t1999", "36151 bytes, fnv 3a725fa727f0ae59"),
            ("kpi --since t2050", "36151 bytes, fnv 912fcc401c35cd12"),
            ("targets day", "342 bytes, fnv 78ed68f51331a953"),
            ("targets week", "334 bytes, fnv 81a4e0b9de16f6ea"),
            ("report", "256427 bytes, fnv 1feaabe9312a4675"),
            ("queue_before t1420", "31"),
            ("queue_before t1480", "33"),
            ("queue_before t1999", "33"),
            ("queue_before t2050", "33"),
            ("queue rows", "10703 bytes, fnv b3b9126f0e99f2f0"),
        ],
    ),
    (
        "no r",
        &[
            (
                "stats position",
                "{\"after\":0,\"end_ms\":1901000001999,\"next_cursor\":48,\"start_ms\":null,\"upto\":48,\"window_ms\":[1900000000000,1901000001000]}",
            ),
            ("stats", "42370 bytes, fnv 79f664a80fe14dd9"),
            (
                "stats --since t2999 position",
                "{\"after\":47,\"end_ms\":1901000001999,\"next_cursor\":48,\"start_ms\":1900002999000,\"upto\":48,\"window_ms\":[1900002999000,1901000001000]}",
            ),
            ("stats --since t2999", "17709 bytes, fnv 9149883556333649"),
            (
                "stats --since t3000 position",
                "{\"after\":48,\"end_ms\":1901000001999,\"next_cursor\":48,\"start_ms\":1900003000000,\"upto\":48,\"window_ms\":[1900003000000,1901000001000]}",
            ),
            ("stats --since t3000", "17709 bytes, fnv 90cbb89ca3b8c35d"),
            (
                "stats --until 47 position",
                "{\"after\":0,\"end_ms\":1900001930000,\"next_cursor\":47,\"start_ms\":null,\"upto\":47,\"window_ms\":[1900000000000,1900001930000]}",
            ),
            ("stats --until 47", "42371 bytes, fnv 5e607a386a320255"),
            ("kpi", "109424 bytes, fnv 18e31e586cee31b6"),
            ("kpi --since t2999", "36147 bytes, fnv f5facde981180604"),
            ("kpi --since t3000", "36147 bytes, fnv 51d8e919f337d29a"),
            ("targets day", "342 bytes, fnv 78ed68f51331a953"),
            ("targets week", "334 bytes, fnv 81a4e0b9de16f6ea"),
            ("report", "256427 bytes, fnv 1feaabe9312a4675"),
            ("queue_before t2999", "28"),
            ("queue_before t3000", "48"),
            ("queue rows", "8538 bytes, fnv e151d0de0cc514d9"),
        ],
    ),
    (
        "queue and run",
        &[
            (
                "stats position",
                "{\"after\":0,\"end_ms\":1901000001999,\"next_cursor\":2722,\"start_ms\":null,\"upto\":2722,\"window_ms\":[1900000000000,1901000001000]}",
            ),
            ("stats", "243634 bytes, fnv 107bc95e285df0bb"),
            (
                "stats --since t1201 position",
                "{\"after\":70,\"end_ms\":1901000001999,\"next_cursor\":2722,\"start_ms\":1900001201000,\"upto\":2722,\"window_ms\":[1900001201000,1901000001000]}",
            ),
            ("stats --since t1201", "240669 bytes, fnv 55614e557b24b69b"),
            (
                "stats --since t3150 position",
                "{\"after\":190,\"end_ms\":1901000001999,\"next_cursor\":2722,\"start_ms\":1900003150000,\"upto\":2722,\"window_ms\":[1900003150000,1901000001000]}",
            ),
            ("stats --since t3150", "236194 bytes, fnv 05aad527f4930c86"),
            (
                "stats --since t9211 position",
                "{\"after\":2717,\"end_ms\":1901000001999,\"next_cursor\":2722,\"start_ms\":1900009211000,\"upto\":2722,\"window_ms\":[1900009211000,1901000001000]}",
            ),
            ("stats --since t9211", "17979 bytes, fnv dbb0037f9f840927"),
            (
                "stats --since t9221 position",
                "{\"after\":2717,\"end_ms\":1901000001999,\"next_cursor\":2722,\"start_ms\":1900009221000,\"upto\":2722,\"window_ms\":[1900009221000,1901000001000]}",
            ),
            ("stats --since t9221", "17979 bytes, fnv 3f7200cc5304e16d"),
            (
                "stats --until 2719 position",
                "{\"after\":0,\"end_ms\":1901000001999,\"next_cursor\":2717,\"start_ms\":null,\"upto\":2719,\"window_ms\":[1900000000000,1900009211000]}",
            ),
            ("stats --until 2719", "243367 bytes, fnv 01a5fd714ca512a9"),
            (
                "stats --until 2718 position",
                "{\"after\":0,\"end_ms\":1901000001999,\"next_cursor\":2717,\"start_ms\":null,\"upto\":2718,\"window_ms\":[1900000000000,1900009211000]}",
            ),
            ("stats --until 2718", "243367 bytes, fnv 01a5fd714ca512a9"),
            (
                "stats --until 2722 position",
                "{\"after\":0,\"end_ms\":1900009224000,\"next_cursor\":2722,\"start_ms\":null,\"upto\":2722,\"window_ms\":[1900000000000,1900009224000]}",
            ),
            ("stats --until 2722", "243631 bytes, fnv 599df95fe2aabdc5"),
            ("kpi", "109424 bytes, fnv 18e31e586cee31b6"),
            ("kpi --since t1201", "47205 bytes, fnv 7588b5ddd1c1d647"),
            ("kpi --since t3150", "47201 bytes, fnv 436f22d8007ee061"),
            ("kpi --since t9211", "37227 bytes, fnv f8a9183e10c3f0fb"),
            ("kpi --since t9221", "37229 bytes, fnv 69bc9cc8abdd317e"),
            ("targets day", "342 bytes, fnv 78ed68f51331a953"),
            ("targets week", "334 bytes, fnv 81a4e0b9de16f6ea"),
            ("report", "256425 bytes, fnv 08705a41316ab583"),
            ("queue_before t1201", "70"),
            ("queue_before t3150", "170"),
            ("queue_before t9211", "2708"),
            ("queue_before t9221", "2708"),
            ("queue rows", "102094 bytes, fnv 1c1bd86cdcd3c2ee"),
        ],
    ),
    (
        "timeless",
        &[
            (
                "stats position",
                "{\"after\":0,\"end_ms\":1901000001999,\"next_cursor\":92,\"start_ms\":null,\"upto\":93,\"window_ms\":[1900000100000,1901000001000]}",
            ),
            ("stats", "42369 bytes, fnv 747302c0ed4be352"),
            (
                "stats --since t50 position",
                "{\"after\":0,\"end_ms\":1901000001999,\"next_cursor\":92,\"start_ms\":1900000050000,\"upto\":93,\"window_ms\":[1900000050000,1901000001000]}",
            ),
            ("stats --since t50", "42391 bytes, fnv 804e5269a3b1a3a8"),
            (
                "stats --since t4000 position",
                "{\"after\":88,\"end_ms\":1901000001999,\"next_cursor\":92,\"start_ms\":1900004000000,\"upto\":93,\"window_ms\":[1900004000000,1901000001000]}",
            ),
            ("stats --since t4000", "17709 bytes, fnv cfd50dfa34ea9f37"),
            (
                "stats --since t4500 position",
                "{\"after\":90,\"end_ms\":1901000001999,\"next_cursor\":92,\"start_ms\":1900004500000,\"upto\":93,\"window_ms\":[1900004500000,1901000001000]}",
            ),
            ("stats --since t4500", "17709 bytes, fnv 37c9be50f31724f9"),
            (
                "stats --since t5000 position",
                "{\"after\":92,\"end_ms\":1901000001999,\"next_cursor\":92,\"start_ms\":1900005000000,\"upto\":93,\"window_ms\":[1900005000000,1901000001000]}",
            ),
            ("stats --since t5000", "17709 bytes, fnv 5d562c6bfee5218e"),
            (
                "stats --since t6000 position",
                "{\"after\":92,\"end_ms\":1901000001999,\"next_cursor\":92,\"start_ms\":1900006000000,\"upto\":93,\"window_ms\":[1900006000000,1901000001000]}",
            ),
            ("stats --since t6000", "17709 bytes, fnv 9b6522cd66070b00"),
            (
                "stats --until 20 position",
                "{\"after\":0,\"end_ms\":1901000001999,\"next_cursor\":20,\"start_ms\":null,\"upto\":20,\"window_ms\":[1901000001000,1901000001000]}",
            ),
            ("stats --until 20", "17633 bytes, fnv 7f4194272fb71c9a"),
            (
                "stats --until 92 position",
                "{\"after\":0,\"end_ms\":1900005000000,\"next_cursor\":92,\"start_ms\":null,\"upto\":92,\"window_ms\":[1900000100000,1900005000000]}",
            ),
            ("stats --until 92", "42371 bytes, fnv 3d2c754920468b76"),
            (
                "stats --until 93 position",
                "{\"after\":0,\"end_ms\":1901000001999,\"next_cursor\":93,\"start_ms\":null,\"upto\":93,\"window_ms\":[1900000100000,1901000001000]}",
            ),
            ("stats --until 93", "42369 bytes, fnv 4b6dc5b2f119ac6a"),
            ("kpi", "109424 bytes, fnv 18e31e586cee31b6"),
            ("kpi --since t50", "45982 bytes, fnv 99b804429620e2d9"),
            ("kpi --since t4000", "36151 bytes, fnv 3ed81c05296a9457"),
            ("kpi --since t4500", "36151 bytes, fnv e20effa943772de0"),
            ("kpi --since t5000", "36151 bytes, fnv 944b132abbb81df7"),
            ("kpi --since t6000", "36151 bytes, fnv 7c58ef13ad4455f4"),
            ("targets day", "342 bytes, fnv 78ed68f51331a953"),
            ("targets week", "334 bytes, fnv 81a4e0b9de16f6ea"),
            ("report", "256427 bytes, fnv 1feaabe9312a4675"),
            ("queue_before t50", "0"),
            ("queue_before t4000", "88"),
            ("queue_before t4500", "90"),
            ("queue_before t5000", "92"),
            ("queue_before t6000", "92"),
            ("queue rows", "40861 bytes, fnv 60108842fb798e83"),
        ],
    ),
];
