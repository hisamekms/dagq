//! The generation and bounded-fill boundary of the first-Execution view,
//! driven identically by SQLite's memdb and the Postgres CI example.
use super::{ContractStore, ledger_open::taken_over};
use crate::domain::ledger::open::{FILL_BYTES, Filled, ROWS_PER_FILL};
use crate::domain::ledger::{LedgerKind, LedgerRow, build};
use crate::domain::{EventId, RunEvent, RunId};
use crate::infrastructure::runtime_store::ledger_open_in as view;
use crate::infrastructure::sql_exec::{SqlRead, SqlWrite, sql_params};
use anyhow::{Result, ensure};
use serde_json::json;
use std::collections::BTreeMap;

type Firsts = BTreeMap<(String, String), i64>;

fn row(n: usize, at: i64) -> LedgerRow {
    let key = format!("{n:08x}-5555-4555-8555-555555555555");
    let event = RunEvent {
        id: EventId::new(1),
        task_id: None,
        goal_id: None,
        run_id: Some(RunId::new(key).unwrap()),
        kind: "run_claimed".into(),
        payload: json!({"from":"ready", "to":"in_progress"}),
        created_at: "2030-01-01T00:00:00Z".into(),
        actor: None,
    };
    let mut row = build(&[event], 1_900_000_000_000, "t")
        .into_iter()
        .find(|row| row.kind == LedgerKind::Run)
        .unwrap();
    row.detail = json!({"executions":[{"actor":"worker", "provider":"codex", "at":at}]});
    row
}

fn entries(tx: &impl SqlRead) -> Result<Firsts> {
    Ok(SqlRead::query_map(
        tx,
        "SELECT actor, provider, at FROM ledger_first_executions",
        sql_params![],
        |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)),
    )?
    .into_iter()
    .collect())
}

/// The reference folds every stored run's detail, independently of the view.
fn all_minima(tx: &impl SqlRead) -> Result<Firsts> {
    let mut all = Firsts::new();
    for detail in SqlRead::query_map(
        tx,
        "SELECT detail FROM ledger_rows WHERE kind='run'",
        sql_params![],
        |r| r.get::<_, String>(0),
    )? {
        let detail: serde_json::Value = serde_json::from_str(&detail)?;
        for entry in detail["executions"].as_array().into_iter().flatten() {
            let key = (
                entry["actor"].as_str().unwrap().into(),
                entry["provider"].as_str().unwrap().into(),
            );
            let at = entry["at"].as_i64().unwrap();
            all.entry(key)
                .and_modify(|held| *held = (*held).min(at))
                .or_insert(at);
        }
    }
    Ok(all)
}

fn fill(tx: &impl SqlWrite) -> Result<()> {
    for _ in 0..1000 {
        if view::first_fill(tx)?.is_done() {
            return Ok(());
        }
    }
    anyhow::bail!("first-Execution filling did not finish")
}

/// Absent and unversioned marks are not complete. A restart carries the
/// mark on; an old writer's generation leaves it behind and forces a refill,
/// including both a new pair and an earlier time for an existing pair.
pub fn ledger_first_executions_generations<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let (first, a) = taken_over(open, None, "first-executions-1")?;
    let tx = first.write_entry("contract_first_init")?;
    tx.execute("DELETE FROM ledger_first_executions_ready", sql_params![])?;
    ensure!(!view::first_held(&tx)?);
    tx.execute(
        "INSERT INTO ledger_first_executions_ready(singleton) VALUES (1)",
        sql_params![],
    )?;
    ensure!(!view::first_held(&tx)?);
    view::write_row(&tx, &row(1, 50))?;
    fill(&tx)?;
    ensure!(view::first_held(&tx)? && entries(&tx)? == all_minima(&tx)?);
    S::commit(tx)?;

    let (second, b) = taken_over(open, Some(("first-executions-1", a)), "first-executions-2")?;
    let tx = second.write_entry("contract_first_restart")?;
    ensure!(!view::first_held(&tx)?);
    view::write_row(&tx, &row(2, 20))?;
    ensure!(view::first_held(&tx)? && entries(&tx)? == all_minima(&tx)?);
    S::commit(tx)?;

    let (old, c) = taken_over(
        open,
        Some(("first-executions-2", b)),
        "first-executions-old",
    )?;
    let tx = old.write_entry("contract_first_old")?;
    ensure!(!view::first_held(&tx)?);
    // Only the run row changes, as on a binary unaware of this view.
    let detail = json!({"executions":[
        {"actor":"worker","provider":"codex","at":-50},
        {"actor":"review","provider":"claude","at":10}]});
    tx.execute(
        "UPDATE ledger_rows SET detail=?1 WHERE kind='run' AND key=?2",
        sql_params![detail.to_string(), row(1, 50).key],
    )?;
    ensure!(entries(&tx)? != all_minima(&tx)?);
    S::commit(tx)?;

    let (current, d) = taken_over(
        open,
        Some(("first-executions-old", c)),
        "first-executions-current",
    )?;
    let tx = current.write_entry("contract_first_refill")?;
    ensure!(!view::first_held(&tx)?);
    view::write_row(&tx, &row(3, 100))?;
    ensure!(!view::first_held(&tx)?);
    fill(&tx)?;
    ensure!(view::first_held(&tx)? && entries(&tx)? == all_minima(&tx)?);
    tx.execute("DELETE FROM ledger_rows WHERE kind='run'", sql_params![])?;
    tx.execute("DELETE FROM ledger_first_executions", sql_params![])?;
    tx.execute("DELETE FROM ledger_first_executions_ready", sql_params![])?;
    tx.execute("DELETE FROM ledger_open", sql_params![])?;
    tx.execute("DELETE FROM ledger_open_filled", sql_params![])?;
    S::commit(tx)?;
    ensure!(current.release_write_leader("first-executions-current", d)?);
    Ok(())
}

/// A partial fill cannot be trusted; pages and an oversized detail stop
/// progress at the same bounds as ledger_open, without skipping the row.
pub fn ledger_first_executions_parts<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_first_parts")?;
    for n in 0..ROWS_PER_FILL + 1 {
        view::write_row(&tx, &row(n, n as i64))?;
    }
    tx.execute("DELETE FROM ledger_first_executions_ready", sql_params![])?;
    let part = view::first_fill(&tx)?;
    ensure!(matches!(part, Filled::Through(_)));
    ensure!(!view::first_held(&tx)?);
    fill(&tx)?;
    ensure!(view::first_held(&tx)? && entries(&tx)? == all_minima(&tx)?);
    let mut large = row(0, -100);
    large.detail["executions"][0]["padding"] = json!("x".repeat(FILL_BYTES + 1));
    view::write_row(&tx, &large)?;
    tx.execute("DELETE FROM ledger_first_executions_ready", sql_params![])?;
    ensure!(view::first_fill(&tx)? == Filled::Through(large.key));
    ensure!(!view::first_held(&tx)?);
    fill(&tx)?;
    ensure!(view::first_held(&tx)? && entries(&tx)? == all_minima(&tx)?);
    Ok(())
}
