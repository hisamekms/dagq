//! What the run rows leave open at their end (`ledger_open`,
//! [`crate::domain::ledger::open`]) through the store's functions on a
//! transaction: written with each run row and taken back by a later write
//! of it, the rows written before the table taken in a part at a time,
//! bounded by bytes, up to the mark, and the mark kept through the write
//! leader's generations only while each writer kept the table. What a row
//! leaves open, and that it is what the whole read of the earlier records
//! leaves open, are domain decisions tested in `ledger::open`. Every case
//! but the one of the generations rolls back; that one removes its rows.
use super::ContractStore;
use crate::domain::ledger::open::{FILL_BYTES, Filled, Left, ROWS_PER_FILL, left_open};
use crate::domain::ledger::{LedgerKind, LedgerRow, build};
use crate::domain::marks::utc_text;
use crate::domain::{EventId, RunEvent, RunId, TaskId};
use crate::infrastructure::runtime_store::ledger_open_in as open_in;
use crate::infrastructure::sql_exec::{SqlWrite, sql_params};
use crate::infrastructure::sqlite::WriteIdentity;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const LEASE: Duration = Duration::from_secs(86_400);
/// The time of the first event, unix ms.
const T0_MS: i64 = 1_900_000_000_000;
const BUILT_AT: &str = "2030-01-01T00:00:00.000Z";

/// The run of the case `n`.
fn run_key(n: u64) -> String {
    format!("{n:08x}-4444-4444-8444-444444444444")
}

/// The events of a queue, ascending id and time.
#[derive(Default)]
struct Events(Vec<RunEvent>);

impl Events {
    fn add(&mut self, n: u64, kind: &str, payload: Value) -> EventId {
        let id = EventId::new(i64::try_from(self.0.len()).expect("few events") + 1);
        self.0.push(RunEvent {
            id,
            task_id: Some(TaskId::new(i64::try_from(n).expect("a small task"))),
            goal_id: None,
            run_id: Some(RunId::new(run_key(n)).expect("a run id")),
            kind: kind.into(),
            payload,
            created_at: utc_text(T0_MS + id.as_i64() * 1000),
            actor: None,
        });
        id
    }

    /// Run `n` claimed and, when `ends`, integrated.
    fn claimed(&mut self, n: u64, ends: bool) -> &mut Self {
        self.add(
            n,
            "run_claimed",
            json!({"from": "ready", "to": "in_progress"}),
        );
        if ends {
            self.add(
                n,
                "run_integrated",
                json!({"from": "validating", "to": "integrated", "status": "integrated"}),
            );
        }
        self
    }

    fn session(&mut self, n: u64) -> EventId {
        self.add(n, "session_opened", json!({"kind": "review"}))
    }

    fn close(&mut self, n: u64, opened: Option<EventId>) {
        let mut payload = json!({"kind": "review"});
        if let Some(opened) = opened {
            payload["opened_event_id"] = json!(opened);
        }
        self.add(n, "session_closed", payload);
    }
}

/// Run rows with a record of each kind the filling's SQL picks for what a
/// row leaves open: run 1's claim with no end, run 2's ask and run 3's
/// session with no answer or close, run 4's landing queued, run 7's ask
/// answered when `answered`, and `settled` runs each landed with an ask
/// answered and a session closed. What each leaves open is the domain's
/// (`ledger::open`); the cases compare the store with it.
fn rows(settled: u64, answered: bool) -> Vec<LedgerRow> {
    let mut events = Events::default();
    events.claimed(1, false);
    events
        .claimed(2, true)
        .add(2, "ask_opened", json!({"ask_id": 2}));
    events.claimed(3, true).session(3);
    events
        .claimed(4, false)
        .add(4, "landing_queued", json!({"via": "exit"}));
    events
        .claimed(7, true)
        .add(7, "ask_opened", json!({"ask_id": 7}));
    if answered {
        events.add(7, "ask_answered", json!({"ask_id": 7}));
    }
    for k in 0..settled {
        let n = 1000 + k;
        events
            .claimed(n, true)
            .add(n, "ask_opened", json!({"ask_id": n}));
        events.add(n, "ask_answered", json!({"ask_id": n}));
        let opened = events.session(n);
        events.close(n, Some(opened));
    }
    let now = T0_MS + i64::try_from(events.0.len()).expect("few events") * 1000 + 60_000;
    build(&events.0, now, BUILT_AT)
        .into_iter()
        .filter(|row| row.kind == LedgerKind::Run)
        .collect()
}

/// What each of `rows` leaves open, of those that leave any.
fn table_of(rows: &[LedgerRow]) -> BTreeMap<String, BTreeSet<Left>> {
    rows.iter()
        .map(|row| (row.key.clone(), left_open(row)))
        .filter(|(_, left)| !left.is_empty())
        .collect()
}

fn write_rows(tx: &impl SqlWrite, rows: &[LedgerRow]) -> Result<()> {
    for row in rows {
        open_in::write_row(tx, row)?;
    }
    Ok(())
}

/// Fill on `tx` until done: the parts it took, each before the last not
/// done for a reader.
fn fill_all(tx: &impl SqlWrite) -> Result<usize> {
    for parts in 1..10_000 {
        if open_in::fill(tx)?.is_done() {
            return Ok(parts);
        }
        ensure!(!open_in::held(tx)?.is_done(), "done before the last part");
    }
    anyhow::bail!("the filling never ended")
}

/// The table as the migration leaves it on a store whose rows were written
/// before it.
fn as_before_the_table(tx: &impl SqlWrite) -> Result<()> {
    tx.execute("DELETE FROM ledger_open", sql_params![])?;
    tx.execute("DELETE FROM ledger_open_filled", sql_params![])?;
    Ok(())
}

/// (a) Each write of a run row holds what the domain says it leaves open
/// in place of what the table held of it: a later write of the row with
/// its close takes the entry back, and a row that leaves nothing open has
/// none. All rows roll back.
pub fn ledger_open_written_with_its_rows<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_ledger_open_written")?;
    let earlier = rows(3, false);
    write_rows(&tx, &earlier)?;
    let held = open_in::entries(&tx)?;
    ensure!(held == table_of(&earlier), "{held:?}");
    ensure!(held.contains_key(&run_key(7)));
    // The later pass writes the rows again with run 7's answer.
    let later = rows(3, true);
    write_rows(&tx, &later)?;
    let held = open_in::entries(&tx)?;
    ensure!(held == table_of(&later), "{held:?}");
    ensure!(!held.contains_key(&run_key(7)));
    ensure!((1000..1003).all(|n| !held.contains_key(&run_key(n))));
    Ok(())
}

/// (b) What the table holds counts the runs left open, not the settled
/// runs beside them: 30 and 3000 settled runs hold as many rows. All rows
/// roll back.
pub fn ledger_open_counts_the_runs_left_open<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    let mut counts = Vec::new();
    for settled in [30, 3000] {
        let tx = store.write_entry("contract_ledger_open_counts")?;
        let rows = rows(settled, true);
        ensure!(rows.len() > usize::try_from(settled)?);
        write_rows(&tx, &rows)?;
        let held = open_in::entries(&tx)?;
        counts.push((held.len(), held.values().map(BTreeSet::len).sum::<usize>()));
        drop(tx);
    }
    ensure!(counts[0] == counts[1], "{counts:?}");
    let left = table_of(&rows(0, true));
    ensure!(
        counts[0] == (left.len(), left.values().map(BTreeSet::len).sum()),
        "{counts:?}"
    );
    Ok(())
}

/// (c) The rows written before the table are taken in a part at a time up
/// to the mark: a reader takes no part for the whole, a row written
/// meanwhile is taken as it is written, and the table filled is the one
/// the writes of the rows make. All rows roll back.
pub fn ledger_open_filled_in_parts<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_ledger_open_filled")?;
    let rows = rows(2 * u64::try_from(ROWS_PER_FILL)?, true);
    write_rows(&tx, &rows)?;
    let written = open_in::entries(&tx)?;
    as_before_the_table(&tx)?;
    ensure!(open_in::held(&tx)? == Filled::Not);
    let mut keys: Vec<&str> = rows.iter().map(|row| row.key.as_str()).collect();
    keys.sort_unstable();
    // At most a page of keys, fewer when its time is spent first.
    match open_in::fill(&tx)? {
        Filled::Through(key) => ensure!(key.as_str() <= keys[ROWS_PER_FILL - 1], "{key}"),
        other => anyhow::bail!("a part, not {other:?}"),
    }
    ensure!(!open_in::held(&tx)?.is_done());
    open_in::write_row(&tx, rows.last().context("rows")?)?;
    fill_all(&tx)?;
    ensure!(open_in::held(&tx)? == Filled::Done);
    let filled = open_in::entries(&tx)?;
    ensure!(filled == written && filled == table_of(&rows), "{filled:?}");
    Ok(())
}

/// (d) A part ends past the row whose records reach the bytes of a part,
/// and a row over them alone is a part of its own, never skipped. All rows
/// roll back.
pub fn ledger_open_parts_bounded_by_bytes<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_ledger_open_bytes")?;
    let mut events = Events::default();
    let pad = "x".repeat(FILL_BYTES + 1);
    for n in [1, 2] {
        events
            .claimed(n, true)
            .add(n, "ask_opened", json!({"ask_id": n, "note": pad}));
    }
    events.claimed(3, true);
    let rows: Vec<LedgerRow> = build(&events.0, T0_MS + 3_600_000, BUILT_AT)
        .into_iter()
        .filter(|row| row.kind == LedgerKind::Run)
        .collect();
    write_rows(&tx, &rows)?;
    as_before_the_table(&tx)?;
    ensure!(open_in::fill(&tx)? == Filled::Through(run_key(1)));
    ensure!(open_in::fill(&tx)? == Filled::Through(run_key(2)));
    ensure!(open_in::fill(&tx)? == Filled::Done);
    ensure!(open_in::entries(&tx)? == table_of(&rows));
    Ok(())
}

/// `open`'s store as the process `holder` after it took the write leader
/// over: its writes are of the new generation. The holder before it
/// releases first.
pub(super) fn taken_over<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    before: Option<(&str, i64)>,
    holder: &str,
) -> Result<(S, i64)> {
    let store = open()?;
    match before {
        Some((before, generation)) => ensure!(store.release_write_leader(before, generation)?),
        // The case before may hold the leader still: it is released first,
        // as a planned stop releases it.
        None => {
            let (held, now) = store.write_leader()?;
            if held.lease_until_ms > now
                && let Some(holder) = held.holder.as_deref()
            {
                ensure!(store.release_write_leader(holder, held.generation)?);
            }
        }
    }
    let generation = store
        .take_write_leader(holder, LEASE, false)?
        .with_context(|| format!("take over as {holder}"))?;
    let store = open()?.with_identity(WriteIdentity {
        holder: Some(holder.into()),
        generation,
    });
    Ok((store, generation))
}

/// (e) The mark is kept through the write leader's generations only while
/// each writer kept the table. A restart of a writer that keeps it carries
/// the mark on with its first write of the ledger; a generation with no
/// write of the ledger between has it filled again; a writer that keeps no
/// table (an older binary) and writes a run row alone, the marks
/// untouched, leaves the mark behind, and neither a reader nor this
/// binary's next write of another row takes it for done until the filling
/// took every row in again. The rows written are removed at the end and
/// the last holder releases the leader.
pub fn ledger_open_kept_through_generations<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let earlier = rows(3, false);
    let later = rows(3, true);
    let (first, a) = taken_over(open, None, "ledger-open-1")?;
    let tx = first.write_entry("contract_ledger_open_first")?;
    write_rows(&tx, &earlier)?;
    as_before_the_table(&tx)?;
    fill_all(&tx)?;
    S::commit(tx)?;
    let read = |store: &S| -> Result<(Filled, BTreeMap<String, BTreeSet<Left>>)> {
        let tx = store.read_entry("contract_ledger_open_read")?;
        Ok((open_in::held(&tx)?, open_in::entries(&tx)?))
    };
    let (held, kept) = read(&first)?;
    ensure!(held == Filled::Done && kept == table_of(&earlier));
    // A restart: not kept until its first write of the ledger, then on.
    let (second, b) = taken_over(open, Some(("ledger-open-1", a)), "ledger-open-2")?;
    ensure!(read(&second)?.0 == Filled::Not);
    let tx = second.write_entry("contract_ledger_open_restart")?;
    ensure!(open_in::fill(&tx)? == Filled::Done);
    S::commit(tx)?;
    ensure!(read(&second)? == (Filled::Done, kept.clone()));
    // A generation with no write of the ledger between: filled again, and
    // an entry the rows do not give goes with it.
    let (_, c) = taken_over(open, Some(("ledger-open-2", b)), "ledger-open-3")?;
    let (third, d) = taken_over(open, Some(("ledger-open-3", c)), "ledger-open-4")?;
    let tx = third.write_entry("contract_ledger_open_gap")?;
    tx.execute(
        "INSERT INTO ledger_open(kind, key, what) VALUES ('run', 'gone', 'slot')",
        sql_params![],
    )?;
    S::commit(tx)?;
    ensure!(read(&third)?.0 == Filled::Not);
    let tx = third.write_entry("contract_ledger_open_gap_fill")?;
    fill_all(&tx)?;
    S::commit(tx)?;
    ensure!(read(&third)? == (Filled::Done, kept));
    // An older binary writes run 7's row with the answer and stops.
    let (old, e) = taken_over(open, Some(("ledger-open-4", d)), "ledger-open-old")?;
    let seven = later
        .iter()
        .find(|row| row.key == run_key(7))
        .context("run 7")?;
    let tx = old.write_entry("contract_ledger_open_old")?;
    tx.execute(
        "UPDATE ledger_rows SET records=?1 WHERE kind='run' AND key=?2",
        sql_params![serde_json::to_string(&seven.records)?, run_key(7)],
    )?;
    S::commit(tx)?;
    ensure!(
        read(&old)?.1.contains_key(&run_key(7)),
        "the table is behind"
    );
    let (fifth, f) = taken_over(open, Some(("ledger-open-old", e)), "ledger-open-5")?;
    ensure!(read(&fifth)?.0 == Filled::Not);
    let tx = fifth.write_entry("contract_ledger_open_after_old")?;
    open_in::write_row(&tx, later.first().context("rows")?)?;
    S::commit(tx)?;
    ensure!(read(&fifth)?.0 == Filled::Not);
    let tx = fifth.write_entry("contract_ledger_open_refill")?;
    fill_all(&tx)?;
    S::commit(tx)?;
    let (held, filled) = read(&fifth)?;
    ensure!(
        held == Filled::Done && filled == table_of(&later),
        "{filled:?}"
    );
    ensure!(!filled.contains_key(&run_key(7)));
    let tx = fifth.write_entry("contract_ledger_open_cleanup")?;
    tx.execute("DELETE FROM ledger_rows WHERE kind='run'", sql_params![])?;
    as_before_the_table(&tx)?;
    S::commit(tx)?;
    ensure!(fifth.release_write_leader("ledger-open-5", f)?);
    Ok(())
}
