//! Queue seeks and their fallback, on the same adapter contract on both stores.
use super::ContractStore;
use super::ledger_earlier::{self as fixture, clear, history};
use super::ledger_range::counted_window;
use crate::domain::ledger::testkit::{DAY, Log, READ_AT_DAY, T0};
use crate::infrastructure::runtime_store::ledger_open_in as view;
use crate::infrastructure::runtime_store::ledger_views_in as views;
use crate::infrastructure::sql_exec::{SqlRead, SqlWrite, sql_params};
use anyhow::{Result, ensure};
use serde_json::json;

pub(crate) fn log(settled: usize, shared: bool, in_range_config: bool) -> Log {
    let mut log = history(0, 0);
    // As in Spike 4756's near_snapshot fixture, the snapshots of the
    // settled history do not forecast a task open in the range.
    for event in &mut log.events {
        if event.kind == crate::domain::EventKind::ForecastRecorded {
            event.payload["tasks"] = json!([]);
        }
    }

    for n in 0..settled {
        let at = 11 * DAY
            + i64::try_from(n).unwrap() * fixture::units_span(130 * DAY)
                / i64::try_from(settled).unwrap();
        let token = if shared { "u".into() } else { format!("u{n}") };
        if !shared || n == 0 {
            log.add(
                None,
                None,
                "supervisor_started",
                json!({"supervisor": token, "parallel": 2}),
                at,
            );
        }
        // Complementary to shared_lives: only the start/stop history
        // grows here. Keeping unrelated settled records on the shared
        // life fixture avoids the whole baseline's O(lives * records)
        // debug-build cost without omitting either lifetime shape.
        if !shared {
            log.add(
                None,
                None,
                "supervisor_stopped",
                json!({"supervisor":token}),
                at + 20,
            );
            continue;
        }
        for (kind, payload, offset) in [
            (
                "slots_full_started",
                json!({"supervisor": token, "class": "heavy"}),
                1,
            ),
            (
                "slots_full_ended",
                json!({"supervisor": token, "class": "heavy"}),
                2,
            ),
            (
                "claim_held",
                json!({"supervisor": token, "reason": "disk"}),
                3,
            ),
            (
                "claim_resumed",
                json!({"supervisor": token, "reason": "disk"}),
                4,
            ),
            (
                "landing_held",
                json!({"supervisor": token, "reason": "disk"}),
                5,
            ),
            (
                "landing_resumed",
                json!({"supervisor": token, "reason": "disk"}),
                6,
            ),
            ("supervisor_alive", json!({"supervisor": token}), 7),
            (
                "candidates_sampled",
                json!({"supervisor": token, "candidates": 1, "free_slots": 1, "ready": 1}),
                8,
            ),
            (
                "mark_recorded",
                json!({"label": format!("unit{n}"), "by": "person"}),
                9,
            ),
            (
                "mark_retracted",
                json!({"label":format!("unit{n}"), "mark":0}),
                17,
            ),
            ("update_failed", json!({"stage": "e2e", "secs": 10}), 10),
            (
                "backend_call_failed",
                json!({"op": "claim", "retried": true}),
                11,
            ),
            (
                "auto_repaired",
                json!({"repair": "stale_lock", "layer": "runtime"}),
                12,
            ),
            ("stall_config_loaded", json!({"turn_limit_secs": 600}), 13),
            (
                "ask_opened",
                json!({"ask_id": 900000+n, "kind": "planner_question"}),
                14,
            ),
            ("ask_answered", json!({"ask_id": 900000+n}), 15),
            (
                "forecast_recorded",
                json!({"at_secs": T0+at, "method": 1, "tasks": [{"id": 900000+n}], "goals": []}),
                16,
            ),
        ] {
            log.add(None, None, kind, payload, at + offset);
        }
        log.add(
            None,
            None,
            "ask_opened",
            json!({"id":1900000+n,"kind":"planner_question"}),
            at + 18,
        )
        .add(None, None, "ask_answered", json!({"id":1900000+n}), at + 19);
        if !shared || n + 1 == settled {
            log.add(
                None,
                None,
                "supervisor_stopped",
                json!({"supervisor": token}),
                at + 20,
            );
        }
    }
    let before = 160 * DAY;
    log.add(None, None, "supervisor_started", json!({"supervisor": "range", "parallel": 2}), before)
       .add(None, None, "forecast_recorded", json!({"at_secs": T0+before, "method": 1,
           "tasks": [{"id": 1, "p50_secs": 3600, "p90_secs": 7200}, {"id": 2, "p50_secs": 3600, "p90_secs": 7200}], "goals": []}), before+1)
       .add(None, None, "ask_opened", json!({"ask_id": 80, "kind": "planner_question"}), before+2)
       .add(None, None, "ask_opened", json!({"ask_id": 81, "kind": "planner_question"}), before+3)
       .add(None, None, "update_started", json!({"sha": "new"}), before+4);
    let near = (READ_AT_DAY - 1) * DAY;
    log.add(None, None, "ask_answered", json!({"ask_id": 80}), near)
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "range"}),
            near + 1,
        )
        .add(
            None,
            None,
            "update_installed",
            json!({"sha": "new"}),
            near + 2,
        )
        .add(
            None,
            None,
            "auto_repaired",
            json!({"repair": "stale_lock", "layer": "runtime"}),
            near + 3,
        );
    if in_range_config {
        log.add(
            None,
            None,
            "stall_config_loaded",
            json!({"turn_limit_secs": 900}),
            near + 4,
        );
    }
    for (start, end, payload) in [
        (
            "slots_full_started",
            "slots_full_ended",
            json!({"supervisor":"range", "class":"heavy"}),
        ),
        (
            "claim_held",
            "claim_resumed",
            json!({"supervisor":"range", "reason":"disk"}),
        ),
        (
            "landing_held",
            "landing_resumed",
            json!({"supervisor":"range", "reason":"disk"}),
        ),
    ] {
        log.add(None, None, start, payload.clone(), before + 100)
            .add(None, None, end, payload, near + 100);
    }
    for (label, at, end) in [
        ("across", before + 200, near + 200),
        ("gone", before + 300, before + 400),
    ] {
        log.add(
            None,
            None,
            "mark_recorded",
            json!({"label":label,"by":"person"}),
            at,
        )
        .add(
            None,
            None,
            "mark_retracted",
            json!({"label":label,"mark":0}),
            end,
        );
    }
    log.add(
        None,
        None,
        "mark_recorded",
        json!({"label":"kept","by":"person"}),
        before + 500,
    );
    // An answered ask whose application is in the range: ask 1735's
    // counterexample. Another settled ask displaces both last-before-S rows.
    log.add(
        None,
        None,
        "ask_opened",
        json!({"ask_id":91,"kind":"planner_question"}),
        before + 600,
    )
    .add(
        None,
        None,
        "ask_answered",
        json!({"ask_id":91}),
        before + 610,
    )
    .add(
        None,
        None,
        "ask_opened",
        json!({"ask_id":92,"kind":"planner_question"}),
        before + 620,
    )
    .add(
        None,
        None,
        "ask_answered",
        json!({"ask_id":92}),
        before + 630,
    )
    .add(
        None,
        None,
        "auto_repaired",
        json!({"ask_id":91,"repair":"stale_lock","layer":"runtime"}),
        near + 300,
    )
    .add(
        None,
        None,
        "auto_repaired",
        json!({"ask_id":80,"repair":"stale_lock","layer":"runtime"}),
        near + 310,
    )
    .add(
        None,
        None,
        "candidates_sampled",
        json!({"supervisor":"range","candidates":3,"free_slots":1,"ready":4}),
        before + 700,
    )
    .add(
        None,
        None,
        "candidates_sampled",
        json!({"supervisor":"range","candidates":2,"free_slots":1,"ready":3}),
        near + 400,
    )
    .add(
        None,
        None,
        "backend_call_failed",
        json!({"op":"claim","retried":false}),
        near + 500,
    )
    .add(
        None,
        None,
        "update_failed",
        json!({"stage":"build","secs":10}),
        near + 600,
    );
    for (n, id) in [
        json!("key"),
        json!({"legacy":"object"}),
        json!(null),
        json!(true),
    ]
    .into_iter()
    .enumerate()
    {
        let at = before + 800 + i64::try_from(n).unwrap() * 10;
        log.add(
            None,
            None,
            "ask_opened",
            json!({"ask_id":id,"kind":"planner_question"}),
            at,
        )
        .add(None, None, "ask_answered", json!({"ask_id":id}), at + 1)
        .add(
            None,
            None,
            "auto_repaired",
            json!({"ask_id":id,"repair":"stale_lock","layer":"runtime"}),
            near + 800 + i64::try_from(n).unwrap(),
        );
    }
    log.add(
        None,
        None,
        "ask_opened",
        json!({"id":93,"kind":"planner_question"}),
        before + 900,
    )
    .add(None, None, "ask_answered", json!({"id":93}), before + 910)
    .add(
        None,
        None,
        "auto_repaired",
        json!({"ask_id":93,"repair":"stale_lock","layer":"runtime"}),
        near + 900,
    );
    renumber(log)
}

fn renumber(mut log: Log) -> Log {
    log.events.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    let ids: std::collections::HashMap<_, _> = log
        .events
        .iter()
        .enumerate()
        .map(|(n, e)| (e.id.as_i64(), i64::try_from(n).unwrap() + 1))
        .collect();
    let mut marks = std::collections::HashMap::new();
    for (n, e) in log.events.iter_mut().enumerate() {
        e.id = crate::domain::EventId::new(i64::try_from(n).unwrap() + 1);
        for key in ["opened_event_id", "mark"] {
            if let Some(old) = e.payload.get(key).and_then(serde_json::Value::as_i64)
                && let Some(new) = ids.get(&old)
            {
                e.payload[key] = json!(new);
            }
        }
        if e.kind == crate::domain::EventKind::MarkRecorded {
            marks.insert(e.payload["label"].as_str().unwrap().to_owned(), e.id);
        }
        if e.kind == crate::domain::EventKind::MarkRetracted
            && let Some(label) = e.payload["label"].as_str()
            && let Some(id) = marks.get(label)
        {
            e.payload["mark"] = json!(id);
        }
    }
    log
}

/// Keep mark references aligned with the EventStore's ids just as the
/// common fixture aligns session references. Leader operations may have
/// recorded events before this history is laid down.
pub(crate) fn lay_down(tx: &impl SqlWrite, log: &Log) -> Result<fixture::Laid> {
    let past: Option<i64> = tx.query_row("SELECT MAX(id) FROM run_events", sql_params![], |r| {
        r.get(0)
    })?;
    let mut log = log.clone();
    for event in &mut log.events {
        if event.kind == crate::domain::EventKind::MarkRetracted
            && let Some(mark) = event.payload["mark"].as_i64()
        {
            event.payload["mark"] = json!(mark + past.unwrap_or(0));
        }
    }
    fixture::lay_down(tx, &log)
}

pub(crate) fn fill(tx: &impl SqlWrite) -> Result<()> {
    for _ in 0..10000 {
        if views::fill(tx)?.is_done() {
            return Ok(());
        }
    }
    anyhow::bail!("queue views did not fill")
}

fn answers(tx: &impl SqlRead) -> Result<Vec<super::ledger_range::Returned>> {
    answers_with(tx, true)
}

fn answers_with(tx: &impl SqlRead, nonempty: bool) -> Result<Vec<super::ledger_range::Returned>> {
    let whole = view::window(tx, &fixture::whole_query())?;
    let mut returned = Vec::new();
    use crate::domain::kpi::{KpiConfig, KpiQuery};
    let default = KpiQuery::default();
    let areas =
        crate::domain::areas::AreaMap::new(vec![("source".to_owned(), vec!["src/**".to_owned()])])
            .map_err(anyhow::Error::msg)?;
    let value = |window: &crate::domain::ledger::window::LedgerWindow,
                 query: &KpiQuery|
     -> Result<serde_json::Value> {
        Ok(serde_json::to_value(crate::application::kpi::of_window(
            window,
            fixture::NOW,
            crate::application::kpi::Host {
                utc_offset_secs: 0,
                cores: Some(4),
                rust_toolchain: false,
            },
            &KpiConfig::default(),
            Some(&areas),
            query,
        )?)?)
    };
    for query in [
        default.clone(),
        KpiQuery {
            by: vec!["area".parse().map_err(anyhow::Error::msg)?],
            ..default
        },
    ] {
        let (window, read) = counted_window(tx, &fixture::time_query(&query))?;
        let expected = value(
            &whole,
            &KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            },
        )?;
        let got = value(&window, &query)?;
        for metric in [
            "ask_apply_wait",
            "ask_seen_wait",
            "candidates",
            "forecast.p50_error",
        ]
        .into_iter()
        .filter(|_| nonempty)
        {
            ensure!(
                expected["periods"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["kpis"][metric]["all"]["n"].as_u64().unwrap_or(0) > 0),
                "fixture has no {metric}"
            );
        }

        let mut misses = Vec::new();
        fixture::differences(&got, &expected, "kpi", &mut misses);
        ensure!(misses.is_empty(), "{misses:?}");
        returned.push(read);
    }
    let query = super::ledger_range::stats_questions().remove(0);
    let whole = view::window(tx, &super::ledger_range::stats_window(&query, true))?;
    let (window, read) = counted_window(tx, &super::ledger_range::stats_window(&query, false))?;
    let got = super::ledger_range::answers(&window, &query)?;
    let expected = super::ledger_range::answers(&whole, &query)?;
    let mut misses = Vec::new();
    fixture::differences(&got.0, &expected.0, "stats", &mut misses);
    ensure!(misses.is_empty(), "{misses:?}");
    ensure!(got.1 == expected.1, "execution tokens");
    returned.push(read);
    Ok(returned)
}

/// The same seek contract on SQLite and Postgres, at 30 and 300 units.
pub fn ledger_queue_lives_answers_as_every_row<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    ledger_queue_lives_with(open, 300)
}

pub(crate) fn ledger_queue_lives_with<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    large: usize,
) -> Result<()> {
    for (shared, config) in [(true, false), (false, true)] {
        let mut counts = Vec::new();
        for settled in [30, large] {
            let store = open()?;
            let tx = store.write_entry("contract_queue_lives")?;
            let laid = lay_down(&tx, &log(settled, shared, config))?;
            fill(&tx)?;
            // Snapshot-to-S marks are excluded from this comparison by
            // putting the relevant snapshot after the settled history:
            // 4670 (vii), planner decision A, 4756 does_not_hold. Their
            // volume is the target's open period, not a bounded history.
            counts.push(answers(&tx)?);
            if settled == 30 && shared {
                for change in [
                    "DELETE FROM ledger_queue_views_filled",
                    "UPDATE ledger_queue_views_filled SET generation=NULL",
                    "UPDATE ledger_queue_views_filled SET generation=-1",
                    "UPDATE ledger_queue_views_filled SET through='partial'",
                    "DELETE FROM ledger_open_filled",
                    "UPDATE ledger_open_filled SET generation=NULL",
                    "UPDATE ledger_open_filled SET generation=-1",
                    "UPDATE ledger_open_filled SET through='partial'",
                    "DROP TABLE ledger_snapshot_targets",
                    "DROP INDEX asks_by_legacy_id",
                    "UPDATE ledger_rows SET ledger_v=ledger_v+1 WHERE kind='queue'",
                ] {
                    tx.execute("SAVEPOINT fallback", sql_params![])?;
                    tx.execute(change, sql_params![])?;
                    view::with_snapshot(&tx, |s| {
                        ensure!(!s.reads_named_queue()?);
                        Ok(())
                    })?;
                    // One full-JSON comparison suffices for each fallback:
                    // all kinds revert together in the same snapshot.
                    answers(&tx)?;
                    tx.execute("ROLLBACK TO SAVEPOINT fallback", sql_params![])?;
                    tx.execute("RELEASE SAVEPOINT fallback", sql_params![])?;
                }
            }
            clear(&tx, &laid)?;
            reset(&tx)?;
        }
        ensure!(
            counts[0] == counts[1],
            "settled history grew the reads: {counts:?}"
        );
    }
    Ok(())
}

/// An older writer commits queue rows while R and the view's mark stay
/// unchanged. The reader falls back until the next writer refills its view.
pub fn ledger_queue_lives_past_an_old_writer<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    use super::ledger_open::taken_over;
    use crate::domain::ledger::{LedgerKind, build};
    use crate::domain::{EventId, RunEvent};
    let (first, a) = taken_over(open, None, "queue-named-new")?;
    let tx = first.write_entry("contract_queue_lives_first")?;
    let mut laid = lay_down(&tx, &log(30, true, true))?;
    fill(&tx)?;
    answers_with(&tx, false)?;
    let marks = views::marks(&tx)?;
    let saved: (Option<String>, Option<i64>) = tx.query_row(
        "SELECT through, generation FROM ledger_queue_views_filled",
        sql_params![],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    S::commit(tx)?;
    let (old, b) = taken_over(open, Some(("queue-named-new", a)), "queue-named-old")?;
    let tx = old.write_entry("contract_queue_lives_old")?;
    let max: i64 = tx.query_row(
        "SELECT COALESCE(MAX(id),0) FROM run_events",
        sql_params![],
        |r| r.get(0),
    )?;
    let mut additions = Vec::new();
    for (n, (kind, payload)) in [
        (
            "forecast_recorded",
            json!({"at_secs": T0+161*DAY, "tasks": [{"id": 1}], "goals": [], "method": 1}),
        ),
        (
            "forecast_recorded",
            json!({"at_secs": T0+162*DAY, "tasks": [{"id": 2}], "goals": [], "method": 1}),
        ),
        (
            "ask_opened",
            json!({"ask_id": 901, "kind": "planner_question"}),
        ),
        (
            "ask_opened",
            json!({"ask_id": 902, "kind": "planner_question"}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let e = RunEvent {
            id: EventId::new(max + i64::try_from(n).unwrap() + 1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: crate::domain::marks::utc_text(
                (T0 + 163 * DAY + i64::try_from(n).unwrap()) * 1000,
            ),
            actor: None,
        };
        fixture::insert_event(&tx, &e)?;
        additions.push(e);
    }
    let rows = build(&additions, fixture::NOW * 1000, "2030-01-01T00:00:00.000Z");
    for row in rows.iter().filter(|r| r.kind == LedgerKind::Queue) {
        views::write_row_alone(&tx, row)?;
    }
    ensure!(views::marks(&tx)? == marks, "old writer left R unchanged");
    let after: (Option<String>, Option<i64>) = tx.query_row(
        "SELECT through, generation FROM ledger_queue_views_filled",
        sql_params![],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(saved == after, "old writer left view mark unchanged");
    view::with_snapshot(&tx, |s| {
        ensure!(!s.reads_named_queue()?);
        Ok(())
    })?;
    answers_with(&tx, false)?;
    S::commit(tx)?;
    let (next, c) = taken_over(open, Some(("queue-named-old", b)), "queue-named-refill")?;
    let tx = next.write_entry("contract_queue_lives_refill")?;
    views::write_row(
        &tx,
        rows.iter().find(|r| r.kind == LedgerKind::Queue).unwrap(),
    )?;
    fixture::fill(&tx)?;
    fill(&tx)?;
    view::with_snapshot(&tx, |s| {
        ensure!(s.reads_named_queue()?);
        Ok(())
    })?;
    answers_with(&tx, false)?;
    // The older writer's backdated ids move time cursors beyond earlier
    // applications, so compare the full JSON without assuming nonempty
    // period samples. The named snapshots and open asks must still be in
    // the actual range read after the view is filled again.
    let window = view::window(
        &tx,
        &fixture::time_query(&crate::domain::kpi::KpiQuery::default()),
    )?;
    for event in additions.iter().skip(1) {
        ensure!(
            window.events().iter().any(|e| e.id == event.id),
            "named old-writer event {} was not read",
            event.id
        );
    }
    laid.log.events.extend(additions);
    clear(&tx, &laid)?;
    reset(&tx)?;
    S::commit(tx)?;
    ensure!(next.release_write_leader("queue-named-refill", c)?);
    Ok(())
}

pub(crate) fn reset(tx: &impl SqlWrite) -> Result<()> {
    for table in [
        "ledger_snapshot_targets",
        "ledger_answered_asks",
        "ledger_main_summaries",
        "ledger_main_commits",
        "ledger_queue_views_filled",
    ] {
        tx.execute(&format!("DELETE FROM {table}"), sql_params![])?;
    }
    Ok(())
}

/// [`log`] of `settled` shared units whose snapshot of task 2, open in the
/// range, is from before every unit: the units' marks are of its open
/// period.
pub(crate) fn old_snapshot_log(settled: usize) -> Log {
    let mut history = log(settled, true, true);
    for event in &mut history.events {
        if event.kind == crate::domain::EventKind::ForecastRecorded
            && event.payload["tasks"]
                .as_array()
                .is_some_and(|tasks| tasks.iter().any(|t| t["id"] == 2))
        {
            event.created_at = crate::domain::marks::utc_text((T0 + 3 * DAY) * 1000);
            event.payload["at_secs"] = json!(T0 + 3 * DAY);
        }
    }
    renumber(history)
}

/// The task whose ends [`ends_log`] repeats before the range.
pub(crate) const REOPENED_TASK: i64 = 950;
/// The goal [`ends_log`] forecasts alone before its first close.
pub(crate) const FORECAST_GOAL: i64 = 7100;
/// The planner whose revision of a proposal [`ends_log`] records on
/// [`REOPENED_TASK`] (its anchor) before the range, its session closed then.
pub(crate) const REVISING_PLANNER: i64 = 77;

/// [`log`] of `settled` shared units with, in each unit, a snapshot of
/// [`REOPENED_TASK`] and its close and reopening (the periods it ended
/// before the range, each scored by its own end), its last snapshot after
/// the units and its close in the range; and [`FORECAST_GOAL`] opened after
/// the units, a snapshot of it alone (no task), a later snapshot of other
/// targets, and its first close in the range.
pub(crate) fn ends_log(settled: usize) -> Log {
    let log = log(settled, true, true);
    let units: Vec<i64> = log
        .events
        .iter()
        .filter(|e| {
            e.kind == crate::domain::EventKind::ForecastRecorded
                && e.payload["tasks"][0]["id"]
                    .as_i64()
                    .is_some_and(|id| id >= 900_000)
        })
        .map(|e| crate::domain::stats::timestamp_millis(&e.created_at).unwrap() / 1000 - T0)
        .collect();
    assert_eq!(units.len(), settled, "a snapshot of each unit");
    ends_with(log, units)
}

/// [`ends_log`] of no unit but [`REOPENED_TASK`]'s `cycles` snapshots,
/// closes and reopenings, spread as [`log`] spreads its units: the periods
/// the task ended before the range alone grow.
pub(crate) fn cycles_log(cycles: usize) -> Log {
    let units = (0..cycles)
        .map(|n| {
            11 * DAY
                + i64::try_from(n).unwrap() * fixture::units_span(130 * DAY)
                    / i64::try_from(cycles).unwrap()
                + 16
        })
        .collect();
    ends_with(log(0, true, true), units)
}

/// `log` with [`ends_log`]'s task and goal, the task closed and reopened
/// after each time of `units`.
fn ends_with(mut log: Log, units: Vec<i64>) -> Log {
    let forecast = |at: i64, tasks: serde_json::Value, goals: serde_json::Value| json!({"at_secs": T0 + at, "method": 1, "tasks": tasks, "goals": goals});
    let row = |id: i64| json!([{"id": id, "p50_secs": 3600, "p90_secs": 7200}]);
    let task = Some(REOPENED_TASK);
    log.add(
        task,
        None,
        "task_created",
        json!({"title": "t950", "status": "ready", "priority": "high"}),
        10 * DAY,
    );
    for at in units {
        log.add(
            None,
            None,
            "forecast_recorded",
            forecast(at + 5, row(REOPENED_TASK), json!([])),
            at + 5,
        )
        .add(
            task,
            None,
            "task_status_changed",
            json!({"from": "ready", "to": "completed"}),
            at + 6,
        )
        .add(
            task,
            None,
            "task_status_changed",
            json!({"from": "completed", "to": "ready"}),
            at + 7,
        );
    }
    let before = 160 * DAY;
    let near = (READ_AT_DAY - 1) * DAY;
    let goal = |log: &mut Log, kind: &str, payload: serde_json::Value, at: i64| {
        log.add(None, None, kind, payload, at);
        log.events.last_mut().unwrap().goal_id = Some(crate::domain::GoalId::new(FORECAST_GOAL));
    };
    // A planner's revision of the task's proposal before the range: a
    // record of the planner's session, closed then, on the task.
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": REVISING_PLANNER}),
        before + 50,
    )
    .add(
        task,
        None,
        "plan_revise_sent",
        json!({"proposal_id": 8000, "planner_id": REVISING_PLANNER, "opened": false}),
        before + 55,
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": REVISING_PLANNER, "session_id": "p-77"}),
        before + 60,
    );
    goal(
        &mut log,
        "goal_created",
        json!({"goal": {"title": "g7100", "status": "open", "priority": "normal"}}),
        before + 30,
    );
    log.add(
        None,
        None,
        "forecast_recorded",
        forecast(before + 35, json!([]), row(FORECAST_GOAL)),
        before + 35,
    )
    .add(
        None,
        None,
        "forecast_recorded",
        forecast(before + 40, row(REOPENED_TASK), json!([])),
        before + 40,
    )
    .add(
        task,
        None,
        "task_status_changed",
        json!({"from": "ready", "to": "completed"}),
        near + 700,
    );
    goal(
        &mut log,
        "goal_closed",
        json!({"verdict": "achieved"}),
        near + 710,
    );
    renumber(log)
}

/// The old snapshot is deliberately kept. The second-stage mark records
/// grow with the target's open period (4670 (vii), decision A, 4756's
/// does_not_hold); they are excluded from the bounded-count assertion.
pub fn ledger_queue_lives_keeps_an_old_snapshot<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_queue_old_snapshot")?;
    let laid = lay_down(&tx, &old_snapshot_log(30))?;
    fill(&tx)?;
    answers(&tx)?;
    clear(&tx, &laid)?;
    reset(&tx)?;
    Ok(())
}
