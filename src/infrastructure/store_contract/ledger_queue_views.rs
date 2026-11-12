//! The views of the queue rows ([`crate::domain::ledger::queue_views`])
//! through the store's functions on a transaction: written with each queue
//! row, the rows written before them taken in a part at a time up to the
//! mark, and the mark kept through the write leader's generations only
//! while each writer kept the views. What each view holds of the events is
//! a domain decision tested in `ledger::queue_views`; the cases compare the
//! store with it. Every case but the one of the generations rolls back;
//! that one removes its rows.
use super::ContractStore;
use crate::domain::ledger::open::{Filled, Left, ROWS_PER_FILL};
use crate::domain::ledger::queue_views::{
    self as views, MainSummary, asks_left_open, main_commits, snapshot_targets, summaries,
};
use crate::domain::ledger::session::written::{Marks, SessionsWritten};
use crate::domain::ledger::{LEDGER_V, LedgerKind, LedgerRow, build};
use crate::domain::marks::utc_text;
use crate::domain::stats::conflicts::{MainChange, MainCommit};
use crate::domain::stats::main_log::{commits_payloads, paths_payloads};
use crate::domain::stats::revert::RevertOf;
use crate::domain::{EventId, RunEvent, RunId};
use crate::infrastructure::runtime_store::ledger_views_in as views_in;
use crate::infrastructure::sql_exec::{SqlRead, SqlWrite, sql_params};
use crate::infrastructure::sqlite::WriteIdentity;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::Duration;

const LEASE: Duration = Duration::from_secs(86_400);
const T0_MS: i64 = 1_900_000_000_000;
const BUILT_AT: &str = "2030-01-01T00:00:00.000Z";

/// The run-less events of a queue, ascending id and time.
#[derive(Default, Clone)]
struct Queue(Vec<RunEvent>);

impl Queue {
    fn add(&mut self, kind: &str, payload: Value) -> EventId {
        let id = EventId::new(i64::try_from(self.0.len()).expect("few events") + 1);
        self.0.push(RunEvent {
            id,
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: utc_text(T0_MS + id.as_i64() * 1000),
            actor: None,
        });
        id
    }

    fn snapshot(&mut self, tasks: &[i64]) {
        let tasks: Vec<Value> = tasks.iter().map(|id| json!({"id": id})).collect();
        self.add(
            "forecast_recorded",
            json!({"supervisor": "s", "at_secs": 1, "tasks": tasks, "goals": []}),
        );
    }

    /// A commit `sha` after `after` that adds `added`, deleting `deleted`
    /// and reverting `reverts`.
    fn commit(&mut self, after: &str, sha: &str, added: &str, reverts: Option<&str>) {
        let commit = MainCommit {
            sha: sha.to_owned(),
            at: 1_900_000_000,
            changes: vec![MainChange {
                path: added.to_owned(),
                from: None,
                deleted: false,
            }],
            reverts: reverts.map_or(RevertOf::NotRevert, |sha| RevertOf::Commit(sha.into())),
        };
        for payload in commits_payloads(Some(after), &[commit]) {
            self.add("main_commits_recorded", payload);
        }
    }

    fn observed(&mut self, head: &str, at: i64) {
        self.add(
            "main_observed",
            json!({"head": head, "at": at, "since": 1_800_000_000, "reverts": true}),
        );
    }

    fn ci(&mut self, branch: &str, sha: &str) {
        self.add("ci_checked", json!({"branch": branch, "sha": sha}));
    }

    fn red(&mut self, sha: &str) {
        self.add("ci_turned_red", json!({"branch": "main", "sha": sha}));
    }

    fn ask(&mut self, kind: &str, id: i64) {
        self.add(kind, json!({"ask_id": id}));
    }

    fn rows(&self) -> Vec<LedgerRow> {
        let now = T0_MS + i64::try_from(self.0.len()).expect("few events") * 1000 + 60_000;
        let mut rows: Vec<LedgerRow> = build(&self.0, now, BUILT_AT)
            .into_iter()
            .filter(|row| row.kind == LedgerKind::Queue)
            .collect();
        rows.sort_by_key(|row| row.inputs_from);
        rows
    }
}

/// A queue whose views hold a snapshot naming three tasks and a later one
/// naming one of them again, main's base and its commits with a revert,
/// CI's checks of two branches and its turns to red of one sha and again
/// of another, an ask answered and one left open, ask 7 answered when
/// `answered`; and `settled` units outside a read's range, each a snapshot
/// of its own task, a commit (every other reverting the one before), a
/// reach, a check of `release`, a turn to red and an ask answered.
fn queue(settled: usize, answered: bool) -> Queue {
    let mut queue = Queue::default();
    queue.snapshot(&[1, 2, 3]);
    let paths: Vec<String> = ["a.rs", "b.rs", "c.rs"].map(str::to_owned).to_vec();
    for payload in paths_payloads("c0", 1_800_000_000, &paths) {
        queue.add("main_paths_recorded", payload);
    }
    queue.observed("c0", 1_900_000_000);
    queue.ci("refs/heads/main", "c0");
    queue.ci("release", "c0");
    queue.commit("c0", "c1", "d.rs", None);
    queue.red("c1");
    queue.commit("c1", "c2", "e.rs", Some("c1"));
    queue.red("c2");
    queue.snapshot(&[2]);
    queue.ask("ask_opened", 1);
    queue.ask("ask_answered", 1);
    queue.ask("ask_opened", 2);
    queue.ask("ask_opened", 7);
    let mut head = "c2".to_owned();
    for k in 0..settled {
        let n = 1000 + i64::try_from(k).expect("few units");
        queue.snapshot(&[n]);
        let sha = format!("u{k}");
        let reverts = (k % 2 == 1).then_some(head.as_str());
        queue.commit(&head.clone(), &sha, &format!("u{k}.rs"), reverts);
        queue.observed(&sha, 1_900_000_100 + n);
        queue.ci("release", &sha);
        queue.red(&sha);
        queue.ask("ask_opened", n);
        queue.ask("ask_answered", n);
        head = sha;
    }
    queue.ci("main", &head);
    if answered {
        queue.ask("ask_answered", 7);
    }
    queue
}

/// What the views of `rows` hold, as the domain makes them of their
/// events.
fn views_of(rows: &[LedgerRow]) -> views_in::Views {
    let mut events: Vec<RunEvent> = rows
        .iter()
        .filter(|row| views::takes(row))
        .flat_map(|row| row.records.clone())
        .collect();
    events.sort_by_key(|event| event.id);
    views_in::Views {
        targets: events
            .iter()
            .flat_map(|event| {
                snapshot_targets(event)
                    .into_iter()
                    .map(|task| (event.id.as_i64(), task))
            })
            .collect(),
        commits: events
            .iter()
            .flat_map(|event| {
                main_commits(event)
                    .into_iter()
                    .map(|commit| (event.id.as_i64(), commit.sha, commit.reverts))
            })
            .collect(),
        summaries: summaries(&events),
        asks: asks_left_open(rows),
    }
}

fn write_rows(tx: &impl SqlWrite, rows: &[LedgerRow]) -> Result<()> {
    for row in rows {
        views_in::write_row(tx, row)?;
    }
    Ok(())
}

/// Fill on `tx` until done: the parts it took, each before the last not
/// done for a reader.
fn fill_all(tx: &impl SqlWrite) -> Result<usize> {
    for parts in 1..10_000 {
        if views_in::fill(tx)?.is_done() {
            return Ok(parts);
        }
        ensure!(!views_in::held(tx)?.is_done(), "done before the last part");
    }
    anyhow::bail!("the filling never ended")
}

/// The views as the migration leaves them on a store whose rows were
/// written before them.
fn as_before_the_views(tx: &impl SqlWrite) -> Result<()> {
    for table in [
        "ledger_snapshot_targets",
        "ledger_main_commits",
        "ledger_main_summaries",
        "ledger_answered_asks",
        "ledger_queue_views_filled",
    ] {
        tx.execute(&format!("DELETE FROM {table}"), sql_params![])?;
    }
    tx.execute("DELETE FROM ledger_open WHERE kind='queue'", sql_params![])?;
    Ok(())
}

/// W and R at the event `through`, as a pass that read up to it writes
/// them.
fn marks_at(through: EventId) -> Result<Marks> {
    let cursors =
        json!({"events": through, "samples": 0, "steps": 0, "resources": 0, "inside_resources": 0});
    Ok(Marks {
        sessions: SessionsWritten::at(through),
        rows: Some(serde_json::from_value(
            json!({"through": cursors, "ledger_v": LEDGER_V}),
        )?),
        omitted: None,
    })
}

/// The key of the run whose row a writer writes first in a generation:
/// a row the views are not made of.
const RUN_KEY: &str = "000000aa-6666-4666-8666-666666666666";

/// The row of run [`RUN_KEY`] claimed, of no Execution (so it adds no
/// first Execution the contract would leave behind).
fn run_row() -> Result<LedgerRow> {
    let event = RunEvent {
        id: EventId::new(1),
        task_id: None,
        goal_id: None,
        run_id: Some(RunId::new(RUN_KEY)?),
        kind: "run_claimed".into(),
        payload: json!({"from": "ready", "to": "in_progress"}),
        created_at: utc_text(T0_MS),
        actor: None,
    };
    let mut row = build(&[event], T0_MS + 60_000, BUILT_AT)
        .into_iter()
        .find(|row| row.kind == LedgerKind::Run)
        .context("a run's row")?;
    row.detail = json!({});
    Ok(row)
}

/// The mark's row as the store holds it.
fn mark_row(tx: &impl SqlRead) -> Result<Vec<(Option<String>, Option<i64>)>> {
    Ok(tx.query_map(
        "SELECT through, generation FROM ledger_queue_views_filled",
        sql_params![],
        |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<i64>>(1)?)),
    )?)
}

/// (a) Each write of a queue row holds its views in place of what they
/// held of it, the summary folded on the one before: the views of the
/// rows written in order are those the domain makes of their events, and
/// a later write of ask 7's answer takes its entry back. All rows roll
/// back.
pub fn ledger_queue_views_written_with_their_rows<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_ledger_queue_views_written")?;
    as_before_the_views(&tx)?;
    ensure!(fill_all(&tx)? == 1, "a store of no rows is filled at once");
    let earlier = queue(3, false).rows();
    write_rows(&tx, &earlier)?;
    let held = views_in::views(&tx)?;
    ensure!(held == views_of(&earlier), "{held:?}");
    ensure!(
        held.asks
            .values()
            .flatten()
            .any(|left| *left == Left::Ask(Some("7".into())))
    );
    ensure!(held.targets.iter().filter(|(_, task)| *task == 2).count() == 2);
    let later = queue(3, true).rows();
    write_rows(&tx, &later)?;
    let held = views_in::views(&tx)?;
    ensure!(held == views_of(&later), "{held:?}");
    // A row of main's record that comes after later ones (a row a pass
    // put back) has the summaries folded again, the mark still done.
    tx.execute("DELETE FROM ledger_rows WHERE kind='queue'", sql_params![])?;
    as_before_the_views(&tx)?;
    fill_all(&tx)?;
    let late = later
        .iter()
        .position(|row| {
            row.subject
                .as_ref()
                .is_some_and(|subject| subject.kind == "main_commits_recorded")
        })
        .context("a commit's row")?;
    for (_, row) in later.iter().enumerate().filter(|(at, _)| *at != late) {
        views_in::write_row(&tx, row)?;
    }
    views_in::write_row(&tx, &later[late])?;
    ensure!(views_in::held(&tx)? == Filled::Done);
    let held = views_in::views(&tx)?;
    ensure!(held == views_of(&later), "{held:?}");
    ensure!(
        held.asks
            .values()
            .flatten()
            .all(|left| *left != Left::Ask(Some("7".into()))),
        "{:?}",
        held.asks
    );
    ensure!(views_in::held(&tx)? == Filled::Done);
    Ok(())
}

/// What a read from a time looks up of the views: the snapshots naming a
/// task of its range, the commits recording a sha and those reverting it,
/// the asks left open, and the latest summary through a point.
fn looked_up(tx: &impl SqlRead, through: i64) -> Result<(usize, usize, usize, usize, bool)> {
    let (recorded, reverting) = views_in::commits_of(tx, "c1")?;
    Ok((
        views_in::snapshots_naming(tx, 2)?.len(),
        recorded.len(),
        reverting.len(),
        views_in::views(tx)?.asks.len(),
        views_in::summary_through(tx, through)?.is_some(),
    ))
}

/// (b) What a read looks up by what its range names counts those rows,
/// not the settled history beside them: 30 and 3000 settled units give
/// as many snapshots naming task 2, commits of c1 and asks left open, and
/// one latest summary. All rows roll back.
pub fn ledger_queue_views_count_what_a_read_names<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    let mut counts = Vec::new();
    for settled in [30, 3000] {
        let tx = store.write_entry("contract_ledger_queue_views_counts")?;
        as_before_the_views(&tx)?;
        fill_all(&tx)?;
        let queue = queue(settled, false);
        let rows = queue.rows();
        ensure!(rows.len() > 5 * settled);
        write_rows(&tx, &rows)?;
        let last = queue.0.last().context("events")?.id.as_i64();
        let summary = views_in::summary_through(&tx, last)?.context("a summary")?;
        ensure!(summary.ci.contains_key("main") && summary.ci.contains_key("release"));
        counts.push(looked_up(&tx, last)?);
        drop(tx);
    }
    ensure!(counts[0] == counts[1], "{counts:?}");
    ensure!(counts[0] == (2, 1, 1, 2, true), "{counts:?}");
    Ok(())
}

/// (c) The queue rows written before the views are taken in a part at a
/// time up to the mark: a reader takes no part for the whole, a row
/// written meanwhile past the mark is taken by the filling in the order of
/// the events, and the views filled are those the writes of the rows make.
/// All rows roll back.
pub fn ledger_queue_views_filled_in_parts<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_ledger_queue_views_filled")?;
    as_before_the_views(&tx)?;
    fill_all(&tx)?;
    let rows = queue(ROWS_PER_FILL / 2, true).rows();
    ensure!(rows.len() > 2 * ROWS_PER_FILL);
    write_rows(&tx, &rows)?;
    let written = views_in::views(&tx)?;
    ensure!(written == views_of(&rows), "{written:?}");
    tx.execute("DELETE FROM ledger_rows WHERE kind='queue'", sql_params![])?;
    let (before, after) = rows.split_at(rows.len() - 3);
    for row in before {
        views_in::write_row_alone(&tx, row)?;
    }
    as_before_the_views(&tx)?;
    ensure!(views_in::held(&tx)? == Filled::Not);
    let taken: Vec<&LedgerRow> = before.iter().filter(|row| views::takes(row)).collect();
    match views_in::fill(&tx)? {
        Filled::Through(through) => {
            ensure!(through.parse::<i64>()? <= taken[ROWS_PER_FILL - 1].inputs_from.as_i64());
        }
        other => anyhow::bail!("a part, not {other:?}"),
    }
    ensure!(!views_in::held(&tx)?.is_done());
    write_rows(&tx, after)?;
    fill_all(&tx)?;
    ensure!(views_in::held(&tx)? == Filled::Done);
    let filled = views_in::views(&tx)?;
    ensure!(filled == written, "{filled:?}");
    Ok(())
}

/// `open`'s store as the process `holder` after it took the write leader
/// over: its writes are of the new generation. The holder before it
/// releases first.
fn taken_over<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    before: Option<(&str, i64)>,
    holder: &str,
) -> Result<(S, i64)> {
    let store = open()?;
    match before {
        Some((before, generation)) => ensure!(store.release_write_leader(before, generation)?),
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

/// (d) The mark is kept through the write leader's generations only while
/// each writer kept the views. A restart of a writer that keeps them
/// carries the mark on with its first write of the ledger, whether that is
/// R alone, a row of no view (a run's) or a row of the views, and fills
/// nothing again. A writer that keeps no views (an older binary) writes
/// queue rows alone, a snapshot naming a task of the range, a commit and a
/// turn to red of CI on it and a run-less ask with no answer, R and the
/// mark's row untouched: the mark's row stays as it was, yet a reader
/// takes the views for none, and this binary's next write in the next
/// generation removes the mark and the filling takes every row in again,
/// to the views the rows make. A part of the queue's rows written in the
/// middle of a pass (no marks, R untouched) keeps the views with them and
/// the mark done. The rows written are removed at the end and the last
/// holder releases the leader.
pub fn ledger_queue_views_kept_through_generations<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let earlier = queue(3, false);
    let mut older = earlier.clone();
    older.snapshot(&[1]);
    older.commit("u2", "o1", "o.rs", Some("c2"));
    older.red("o1");
    older.ask("ask_opened", 9);
    let mut chunk = older.clone();
    chunk.snapshot(&[3]);
    chunk.commit("o1", "n1", "n.rs", None);
    chunk.ci("main", "n1");
    chunk.ask("ask_opened", 10);
    let read = |store: &S| -> Result<(Filled, views_in::Views)> {
        let tx = store.read_entry("contract_ledger_queue_views_read")?;
        Ok((views_in::held(&tx)?, views_in::views(&tx)?))
    };
    let (first, a) = taken_over(open, None, "ledger-views-1")?;
    let tx = first.write_entry("contract_ledger_queue_views_first")?;
    as_before_the_views(&tx)?;
    write_rows(&tx, &earlier.rows())?;
    fill_all(&tx)?;
    let r = marks_at(earlier.0.last().context("events")?.id)?;
    ensure!(views_in::write_marks(&tx, &r)?);
    S::commit(tx)?;
    let marks = |store: &S| -> Result<Option<Marks>> {
        views_in::marks(&store.read_entry("contract_ledger_queue_views_marks")?)
    };
    ensure!(marks(&first)? == Some(r.clone()), "R written");
    let (held, kept) = read(&first)?;
    ensure!(
        held == Filled::Done && kept == views_of(&earlier.rows()),
        "{kept:?}"
    );
    // Restarts: not kept until the first write of the ledger, then on,
    // whatever that write is and without filling again (the entry the rows
    // do not give stays): R alone, a run's row, then a row of the views.
    let tx = first.write_entry("contract_ledger_queue_views_mark")?;
    tx.execute(
        "INSERT INTO ledger_snapshot_targets(snapshot_event_id, task_id) VALUES (0, 0)",
        sql_params![],
    )?;
    S::commit(tx)?;
    let (second, b) = taken_over(open, Some(("ledger-views-1", a)), "ledger-views-2")?;
    ensure!(read(&second)?.0 == Filled::Not);
    let tx = second.write_entry("contract_ledger_queue_views_restart_marks")?;
    ensure!(views_in::write_marks(&tx, &r)?);
    S::commit(tx)?;
    ensure!(read(&second)?.0 == Filled::Done, "carried on by R alone");
    let (run, b) = taken_over(open, Some(("ledger-views-2", b)), "ledger-views-run")?;
    ensure!(read(&run)?.0 == Filled::Not);
    let tx = run.write_entry("contract_ledger_queue_views_restart_run")?;
    views_in::write_row(&tx, &run_row()?)?;
    S::commit(tx)?;
    ensure!(read(&run)?.0 == Filled::Done, "carried on by a run's row");
    let (views, b) = taken_over(open, Some(("ledger-views-run", b)), "ledger-views-queue")?;
    ensure!(read(&views)?.0 == Filled::Not);
    let tx = views.write_entry("contract_ledger_queue_views_restart_queue")?;
    write_rows(&tx, &earlier.rows()[..1])?;
    ensure!(views_in::fill(&tx)? == Filled::Done);
    S::commit(tx)?;
    let (held, carried) = read(&views)?;
    ensure!(held == Filled::Done && carried.targets.contains(&(0, 0)));
    let tx = views.write_entry("contract_ledger_queue_views_unmark")?;
    tx.execute(
        "DELETE FROM ledger_snapshot_targets WHERE snapshot_event_id = 0",
        sql_params![],
    )?;
    S::commit(tx)?;
    // An older binary writes the new rows alone and stops before R.
    let (old, c) = taken_over(open, Some(("ledger-views-queue", b)), "ledger-views-old")?;
    let tx = old.read_entry("contract_ledger_queue_views_old_mark")?;
    let mark = mark_row(&tx)?;
    drop(tx);
    let tx = old.write_entry("contract_ledger_queue_views_old")?;
    let written: BTreeSet<String> = earlier.rows().into_iter().map(|row| row.key).collect();
    for row in older
        .rows()
        .iter()
        .filter(|row| !written.contains(&row.key))
    {
        views_in::write_row_alone(&tx, row)?;
    }
    S::commit(tx)?;
    let tx = old.read_entry("contract_ledger_queue_views_old_after")?;
    ensure!(mark_row(&tx)? == mark, "the mark's row is as it was");
    drop(tx);
    ensure!(marks(&old)? == Some(r.clone()), "R is as it was");
    let (held, behind) = read(&old)?;
    ensure!(held == Filled::Not, "{held:?}");
    ensure!(behind != views_of(&older.rows()), "the views are behind");
    // This binary in the next generation: the mark goes and the filling
    // takes every row in again.
    let (third, d) = taken_over(open, Some(("ledger-views-old", c)), "ledger-views-3")?;
    ensure!(read(&third)?.0 == Filled::Not);
    let tx = third.write_entry("contract_ledger_queue_views_after_old")?;
    write_rows(&tx, &older.rows()[..1])?;
    ensure!(views_in::held(&tx)? == Filled::Not);
    fill_all(&tx)?;
    S::commit(tx)?;
    let (held, refilled) = read(&third)?;
    ensure!(
        held == Filled::Done && refilled == views_of(&older.rows()),
        "{refilled:?}"
    );
    // A part of a pass written with no marks: the views with it, still done.
    let tx = third.write_entry("contract_ledger_queue_views_chunk")?;
    let new: Vec<LedgerRow> = chunk
        .rows()
        .into_iter()
        .filter(|row| !older.rows().iter().any(|old| old.key == row.key))
        .collect();
    ensure!(views_in::write_chunk(&tx, &new)?);
    S::commit(tx)?;
    ensure!(
        marks(&third)? == Some(r.clone()),
        "a chunk leaves R as it was"
    );
    let (held, chunked) = read(&third)?;
    ensure!(
        held == Filled::Done && chunked == views_of(&chunk.rows()),
        "{chunked:?}"
    );
    let summary: &MainSummary = chunked.summaries.last().context("a summary")?;
    ensure!(summary.last_commit() == Some("n1"));
    ensure!(
        chunked
            .asks
            .values()
            .flatten()
            .any(|left| *left == Left::Ask(Some("10".into())))
    );
    let tx = third.write_entry("contract_ledger_queue_views_cleanup")?;
    tx.execute("DELETE FROM ledger_rows WHERE kind='queue'", sql_params![])?;
    for table in ["ledger_rows", "ledger_open"] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE kind='run' AND key=?1"),
            sql_params![RUN_KEY],
        )?;
    }
    as_before_the_views(&tx)?;
    S::commit(tx)?;
    ensure!(third.release_write_leader("ledger-views-3", d)?);
    Ok(())
}

/// (e) A run-less ask whose answer the store wrote first is closed by it,
/// whether the writes or the filling take the rows, and an ask no answer
/// names stays open: the store keeps the answers' keys as the domain makes
/// them ([`asks_left_open`]; which keys pair is a domain decision tested
/// in `ledger::queue_views`). All rows roll back.
pub fn ledger_queue_views_pair_asks_whichever_comes_first<
    S: ContractStore,
    F: Fn() -> Result<S>,
>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_ledger_queue_views_asks")?;
    tx.execute("DELETE FROM ledger_rows WHERE kind='queue'", sql_params![])?;
    as_before_the_views(&tx)?;
    fill_all(&tx)?;
    let mut queue = Queue::default();
    queue.add("ask_answered", json!({"ask_id": {"b": 1, "a": [1, "x"]}}));
    queue.add("ask_opened", json!({"ask_id": {"b": 1, "a": [1, "x"]}}));
    queue.add("ask_opened", json!({"ask_id": 2}));
    let rows = queue.rows();
    let left = asks_left_open(&rows);
    ensure!(
        left == [(rows[2].key.clone(), [Left::Ask(Some("2".into()))].into())].into(),
        "{left:?}"
    );
    write_rows(&tx, &rows)?;
    let held = views_in::views(&tx)?.asks;
    ensure!(held == left, "{held:?}");
    as_before_the_views(&tx)?;
    fill_all(&tx)?;
    let filled = views_in::views(&tx)?.asks;
    ensure!(filled == left, "filled: {filled:?}");
    Ok(())
}
