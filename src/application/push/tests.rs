use super::*;
use crate::application::{kpi::Host, report::DiagramRenderer};
use crate::domain::kpi::{
    KpiConfig,
    config::{KpiSettings, Target},
};
use crate::domain::ledger::{
    LedgerKind, LedgerRow, build,
    testkit::{Log, T0, run_id},
    window::{LedgerWindow, RowsFrom, WindowQuery},
};
use crate::domain::{EventId, FindingView, RunEvent, RunId, TaskId};
use std::cell::{Cell, RefCell};
use std::sync::Arc;

// Methods outside the read and record ports exercised here must never run.
macro_rules! unused_ledger_methods {
    () => {
        fn write_ledger_row(&self, _: &LedgerRow) -> Result<bool> {
            unimplemented!()
        }
        fn write_ledger_row_over(
            &self,
            _: &LedgerRow,
            _: Option<&LedgerRow>,
        ) -> Result<Option<bool>> {
            unimplemented!()
        }
        fn write_session_rows_over(
            &self,
            _: &[(LedgerRow, Option<LedgerRow>)],
            _: Option<&crate::domain::ledger::session::written::SessionsWritten>,
        ) -> Result<bool> {
            unimplemented!()
        }
        fn sessions_written(
            &self,
        ) -> Result<Option<crate::domain::ledger::session::written::SessionsWritten>> {
            unimplemented!()
        }
        fn ledger_legacy_runs_without_row(&self) -> Result<Vec<RunId>> {
            unimplemented!()
        }
        fn ledger_runs_claimed(&self, _: crate::domain::TaskId) -> Result<Vec<RunId>> {
            unimplemented!()
        }
        fn ledger_runs_past_their_rows(&self, _: EventId) -> Result<Vec<RunId>> {
            unimplemented!()
        }
        fn remove_omitted_queue_rows(&self) -> Result<usize> {
            Ok(0)
        }
        fn remove_node_rows(&self, _: i64) -> Result<usize> {
            unimplemented!()
        }
        fn ledger_run_events(&self, _: &RunId) -> Result<Vec<RunEvent>> {
            unimplemented!()
        }
        fn last_event_naming_supervisor(&self, _: &str, _: EventId) -> Result<Option<RunEvent>> {
            unimplemented!()
        }
    };
}

#[derive(Default)]
struct Store {
    rows: Vec<LedgerRow>,
    read: Cell<usize>,
}
impl LedgerStore for Store {
    unused_ledger_methods!();
    fn ledger_row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        let row = self
            .rows
            .iter()
            .find(|r| r.kind == kind && r.key == key)
            .cloned();
        self.read.set(self.read.get() + usize::from(row.is_some()));
        Ok(row)
    }
    fn ledger_rows(&self, _: LedgerKind) -> Result<Vec<LedgerRow>> {
        unreachable!()
    }
    fn ledger_rows_after(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        self.page(kind, after, limit, None)
    }
    fn ledger_rows_from(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
        from: &RowsFrom,
    ) -> Result<Vec<LedgerRow>> {
        self.page(kind, after, limit, Some(from))
    }
}
impl Store {
    fn page(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
        from: Option<&RowsFrom>,
    ) -> Result<Vec<LedgerRow>> {
        let mut rows: Vec<_> = self
            .rows
            .iter()
            .filter(|r| {
                r.kind == kind
                    && after.is_none_or(|after| r.key.as_str() > after)
                    && from.is_none_or(|from| !from.leaves_out(r))
            })
            .collect();
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        let rows: Vec<_> = rows.into_iter().take(limit).cloned().collect();
        self.read.set(self.read.get() + rows.len());
        Ok(rows)
    }
}

struct Probe {
    store: Store,
    queries: RefCell<Vec<WindowQuery>>,
    whole: bool,
}
impl LedgerStore for Probe {
    unused_ledger_methods!();
    fn ledger_row(&self, _: LedgerKind, _: &str) -> Result<Option<LedgerRow>> {
        unreachable!()
    }
    fn ledger_rows(&self, _: LedgerKind) -> Result<Vec<LedgerRow>> {
        unreachable!()
    }
    fn ledger_window(&self, query: &WindowQuery) -> Result<LedgerWindow> {
        self.queries.borrow_mut().push(query.clone());
        let mut query = query.clone();
        if self.whole {
            query.read_from_ms = None;
        }
        // Exercise the real port's paging and dependent-row reads.
        self.store.ledger_window(&query)
    }
}

#[derive(Default)]
struct Records {
    open: RefCell<Vec<Value>>,
    checked: RefCell<Vec<Value>>,
    recorded: RefCell<Vec<(EventKind, Value)>>,
}
impl QueueRecords for Records {
    fn findings(&self, _: &crate::domain::FindingQuery) -> Result<Vec<FindingView>> {
        unimplemented!()
    }
    fn reports_written(&self) -> Result<std::collections::HashSet<(String, String)>> {
        unimplemented!()
    }
    fn record_report_written(&self, _: serde_json::Value) -> Result<bool> {
        unimplemented!()
    }
    fn record_kpi_push_abandoned(&self, _: serde_json::Value) -> Result<bool> {
        unimplemented!()
    }
    fn record_forecast(&self, _: serde_json::Value, _: Option<EventId>) -> Result<Option<EventId>> {
        unimplemented!()
    }
    fn ci_watch_events(&self) -> Result<Vec<RunEvent>> {
        unimplemented!()
    }
    fn record_ci_check(
        &self,
        _: crate::domain::ci_watch::CiCheckRecord,
    ) -> Result<Option<crate::domain::ci_watch::CiCheckRecorded>> {
        unimplemented!()
    }
    fn record_finding_once(
        &self,
        _: crate::domain::NewFinding,
    ) -> Result<Option<crate::domain::FindingId>> {
        unimplemented!()
    }
    fn ci_scheduled_events(&self) -> Result<Vec<RunEvent>> {
        unimplemented!()
    }
    fn record_ci_scheduled_check(
        &self,
        _: crate::domain::ci_watch::scheduled::ScheduledCheckRecord,
    ) -> Result<Option<crate::domain::ci_watch::CiCheckRecorded>> {
        unimplemented!()
    }
    fn ci_freshness_events(&self) -> Result<Vec<RunEvent>> {
        unimplemented!()
    }
    fn record_ci_freshness(
        &self,
        _: crate::domain::ci_watch::freshness::FreshnessRecord,
    ) -> Result<Option<EventId>> {
        unimplemented!()
    }
    fn ci_failure_findings_of(&self, _: TaskId) -> Result<Vec<crate::domain::FindingId>> {
        unimplemented!()
    }
    fn ci_failure_fix_tasks(&self) -> Result<std::collections::BTreeMap<i64, Vec<TaskId>>> {
        unimplemented!()
    }

    fn kpi_breaches_checked(&self) -> Result<std::collections::HashSet<(String, String)>> {
        Ok(self
            .checked
            .borrow()
            .iter()
            .map(|p| {
                (
                    p["period"].as_str().unwrap().into(),
                    p["label"].as_str().unwrap().into(),
                )
            })
            .collect())
    }
    fn record_kpi_breach_checked(&self, payload: Value) -> Result<bool> {
        self.checked.borrow_mut().push(payload);
        Ok(true)
    }
    fn kpi_breaches_open(&self) -> Result<Vec<Value>> {
        Ok(self.open.borrow().clone())
    }
    fn record_kpi_breach(
        &self,
        kind: EventKind,
        mut payload: Value,
        push_day: Option<(i64, usize)>,
    ) -> Result<Option<Value>> {
        assert!(push_day.is_none());
        if kind == EventKind::KpiBreachStarted {
            payload["pushed"] = json!(false);
            self.open.borrow_mut().push(payload.clone());
        } else {
            self.open
                .borrow_mut()
                .retain(|p| p["period"] != payload["period"]);
        }
        self.recorded.borrow_mut().push((kind, payload.clone()));
        Ok(Some(payload))
    }
}

const DAY: i64 = 86_400;

fn setup() -> ReportSetup {
    ReportSetup {
        root: Default::default(),
        host: Host {
            utc_offset_secs: 9 * 3600,
            cores: Some(4),
            rust_toolchain: false,
        },
        config: KpiConfig::merge(
            Some(&KpiSettings {
                min_samples: Some(1),
                breach_periods: Some(3),
                breach_weeks: Some(2),
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
        ),
        keep: Default::default(),
        build: "test".into(),
        diagram: DiagramRenderer(Arc::new(|_| unreachable!())),
        areas: None,
    }
}

fn probe(rows: Vec<LedgerRow>, whole: bool) -> Probe {
    Probe {
        store: Store {
            rows,
            ..Default::default()
        },
        queries: RefCell::default(),
        whole,
    }
}

// An old queue record establishes the input watermark before the read's
// cutoff. Recent days are slow until day 100, then meet the same target.
fn history(now: i64) -> Vec<LedgerRow> {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "supervisor_stopped",
        json!({"supervisor_id": "old"}),
        0,
    );
    for day in 1..u8::try_from((now - T0) / DAY).unwrap() {
        let start = log.events.len();
        log.landed(i64::from(day), &run_id(day), i64::from(day) * DAY);
        if day >= 100 {
            // Keep the life and all ordering, but shorten it to < 100s.
            for event in &mut log.events[start..] {
                let at = crate::domain::stats::timestamp_millis(&event.created_at).unwrap();
                let base = (T0 + i64::from(day) * DAY) * 1000;
                event.created_at = crate::domain::marks::utc_text(base + (at - base) / 100);
            }
        }
        if day == 20 {
            log.add(
                None,
                None,
                "supervisor_stopped",
                json!({"supervisor_id": "watermark"}),
                20 * DAY + 1000,
            );
        }
    }
    let mut rows = build(&log.events, now * 1000, "2030-01-01T00:00:00.000Z");
    // The snapshot fold leaves settlement to the builder; these finished
    // histories represent the settled rows already held by a store.
    for row in &mut rows {
        row.is_final = row.ended_at.is_some();
    }
    rows
}

#[test]
fn breach_window_starts_at_the_earlier_day_or_week_read_once() {
    let now = T0 + 100 * DAY;
    // Either period can set the earlier cutoff when streak settings grow.
    for (days, weeks) in [(3, 2), (40, 2), (3, 8)] {
        let mut setup = setup();
        setup.config.breach_periods = days;
        setup.config.breach_weeks = weeks;
        let ledger = probe(Vec::new(), false);
        check_breaches(
            &Records::default(),
            &ledger,
            &setup,
            None,
            now,
            &crate::domain::LeaseToken::new("s"),
        )
        .unwrap();
        let starts = [Period::Day, Period::Week].map(|period| {
            read_from_ms(
                &KpiQuery {
                    period,
                    last: 2,
                    ..Default::default()
                },
                &setup.config,
                now,
                setup.host.utc_offset_secs,
            )
            .unwrap()
        });
        let queries = ledger.queries.borrow();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].read_from_ms, Some(starts[0].min(starts[1])));
        assert_eq!(queries[0].now_ms, crate::application::stats::now_ms(now));
    }
}

#[test]
fn settled_history_outside_the_window_adds_no_store_reads() {
    let now = T0 + 100 * DAY;
    let rows = history(now);
    let old: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.kind != LedgerKind::Queue
                && row.ended_at.is_some_and(|end| end < (T0 + 20 * DAY) * 1000)
        })
        .cloned()
        .collect();
    assert!(!old.is_empty());
    assert!(old.iter().all(|row| row.is_final));
    let recent = probe(
        rows.iter()
            .filter(|row| !old.contains(row))
            .cloned()
            .collect(),
        false,
    );
    let expanded = probe(rows, false);
    let a = check_breaches(
        &Records::default(),
        &recent,
        &setup(),
        None,
        now,
        &crate::domain::LeaseToken::new("s"),
    )
    .unwrap();
    let b = check_breaches(
        &Records::default(),
        &expanded,
        &setup(),
        None,
        now,
        &crate::domain::LeaseToken::new("s"),
    )
    .unwrap();
    assert!(!a.started.is_empty());
    assert_eq!(a, b);
    assert!(recent.store.read.get() > 0);
    assert_eq!(recent.store.read.get(), expanded.store.read.get());
    assert!(expanded.store.read.get() < expanded.store.rows.len());
}

#[test]
fn bounded_and_whole_windows_record_the_same_day_and_week_transitions() {
    let setup = setup();
    let bounded = Records::default();
    let whole = Records::default();
    for (day, kind) in [
        (100, EventKind::KpiBreachStarted),
        (115, EventKind::KpiBreachResolved),
    ] {
        let now = T0 + day * DAY;
        let rows = history(now);
        let a = check_breaches(
            &bounded,
            &probe(rows.clone(), false),
            &setup,
            None,
            now,
            &crate::domain::LeaseToken::new("s"),
        )
        .unwrap();
        let b = check_breaches(
            &whole,
            &probe(rows, true),
            &setup,
            None,
            now,
            &crate::domain::LeaseToken::new("s"),
        )
        .unwrap();
        assert_eq!(a, b);
        let payloads = if kind == EventKind::KpiBreachStarted {
            &a.started
        } else {
            &a.resolved
        };
        let full = crate::domain::ledger::window::select(
            history(now),
            &WindowQuery {
                now_ms: crate::application::stats::now_ms(now),
                ..Default::default()
            },
        );
        let kpi = crate::application::kpi::of_window(
            &full,
            now,
            setup.host,
            &setup.config,
            None,
            &KpiQuery {
                period: Period::Day,
                last: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            payloads.len(),
            2,
            "day {day}: {a:?}, targets: {:?}",
            kpi.targets
        );
        for period in ["day", "week"] {
            assert!(payloads.iter().any(|p| p["period"] == period));
        }
        assert_eq!(*bounded.recorded.borrow(), *whole.recorded.borrow());
        assert!(
            bounded
                .recorded
                .borrow()
                .iter()
                .rev()
                .take(2)
                .all(|(k, _)| *k == kind)
        );
    }
    assert!(bounded.open.borrow().is_empty());
}

#[test]
fn restarting_a_checked_job_reads_no_window_and_records_no_transitions() {
    let now = T0 + 100 * DAY;
    let records = Records::default();
    let ledger = probe(history(now), false);
    let first = check_breaches(
        &records,
        &ledger,
        &setup(),
        None,
        now,
        &crate::domain::LeaseToken::new("first"),
    )
    .unwrap();
    assert_eq!(first.started.len(), 2);
    assert_eq!(records.checked.borrow().len(), 2);
    records.recorded.borrow_mut().clear();
    let restarted = probe(Vec::new(), false);
    assert_eq!(
        check_breaches(
            &records,
            &restarted,
            &setup(),
            None,
            now,
            &crate::domain::LeaseToken::new("restarted")
        )
        .unwrap(),
        BreachCheck::default()
    );
    assert!(restarted.queries.borrow().is_empty());
    assert!(records.recorded.borrow().is_empty());
    assert_eq!(records.open.borrow().len(), 2);
    assert_eq!(records.checked.borrow().len(), 2);
}

#[test]
fn only_the_unchecked_day_is_judged_and_recorded() {
    let now = T0 + 100 * DAY;
    let records = Records::default();
    let token = crate::domain::LeaseToken::new("s");
    check_breaches(
        &records,
        &probe(history(now), false),
        &setup(),
        None,
        now,
        &token,
    )
    .unwrap();
    records
        .checked
        .borrow_mut()
        .retain(|p| p["period"] == "week");
    records.open.borrow_mut().retain(|p| p["period"] == "week");
    records.recorded.borrow_mut().clear();
    let ledger = probe(history(now), false);
    let check = check_breaches(&records, &ledger, &setup(), None, now, &token).unwrap();
    assert_eq!(check.started.len(), 1);
    assert_eq!(check.started[0]["period"], "day");
    assert!(check.resolved.is_empty());
    assert_eq!(records.recorded.borrow().len(), 1);
    let checked = records.checked.borrow();
    assert_eq!(checked.len(), 2);
    assert_eq!(checked[1]["period"], "day");
    assert_eq!(checked[1]["build"], "test");
    assert_eq!(checked[1]["supervisor"], "s");
    assert_eq!(ledger.queries.borrow().len(), 1);
    assert_eq!(
        ledger.queries.borrow()[0].read_from_ms,
        read_from_ms(
            &KpiQuery {
                period: Period::Day,
                last: 2,
                ..Default::default()
            },
            &setup().config,
            now,
            setup().host.utc_offset_secs
        )
    );
}

#[test]
fn after_stopping_across_midnight_only_the_latest_finished_periods_are_checked() {
    let token = crate::domain::LeaseToken::new("s");
    let records = Records::default();
    let setup = setup();
    // Sunday 23:59 JST, then Tuesday 00:01 JST: a whole daily job missed,
    // and both a day and an ISO week ended while the process was stopped.
    let before = crate::domain::stats::timestamp_millis("2026-11-08T14:59:00Z").unwrap() / 1000;
    let after = crate::domain::stats::timestamp_millis("2026-11-09T15:01:00Z").unwrap() / 1000;
    check_breaches(
        &records,
        &probe(Vec::new(), false),
        &setup,
        None,
        before,
        &token,
    )
    .unwrap();
    assert_eq!(
        records.kpi_breaches_checked().unwrap(),
        std::collections::HashSet::from([
            ("day".into(), "2026-11-07".into()),
            ("week".into(), "2026-W44".into()),
        ])
    );
    let ledger = probe(Vec::new(), false);
    check_breaches(&records, &ledger, &setup, None, after, &token).unwrap();
    assert_eq!(ledger.queries.borrow().len(), 1);
    let checked = records.checked.borrow();
    assert_eq!(checked.len(), 4);
    assert_eq!(checked[2]["label"], "2026-11-09");
    assert_eq!(checked[3]["label"], "2026-W45");
}
