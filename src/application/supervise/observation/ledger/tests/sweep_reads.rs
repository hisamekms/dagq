//! A builder from its cursors' start sweeps every settled session and
//! queue row the store holds: it reads their stored rows a share at a time
//! ([`LedgerStore::ledger_rows_of`]), so that the requests it sends grow
//! with its shares, not with the rows it sweeps, and it sweeps and writes
//! as a builder that reads them one at a time does.

use super::*;

/// What a builder from the cursors' start did over a store of
/// [`settled_sessions`] that an earlier builder filled.
struct FromTheStart {
    events: usize,
    /// The requests `ledger.fold_rows` sent for stored rows, one by one
    /// and in shares together.
    requests: usize,
    /// The stored rows its sweeps read ([`SweepCounts::reads`]).
    swept: usize,
    sweeps: Vec<SweepCounts>,
    rows: BTreeMap<(LedgerKind, String), LedgerRow>,
    written: Vec<(LedgerKind, String)>,
}

/// Passes until one leaves no mark, at most `passes`, each pass's sweep
/// kept in `sweeps`.
fn catch_up(memory: &Memory, builder: &mut Builder, passes: usize, sweeps: &mut Vec<SweepCounts>) {
    for _ in 0..passes {
        let built = pass(memory, builder, LATER);
        assert_eq!(built.failure, None);
        sweeps.push(built.sweep);
        if built.backlog == 0 {
            return;
        }
    }
    panic!("marks left after {passes} passes");
}

fn from_the_start(settled: i64, one_by_one: bool) -> FromTheStart {
    let log = settled_sessions(settled);
    let memory = Memory::of(&log);
    memory.rows_one_by_one.set(one_by_one);
    let mut sweeps = Vec::new();
    // An empty store is written a bounded number of rows a pass.
    catch_up(&memory, &mut Builder::default(), 1_000, &mut sweeps);
    memory.callers.borrow_mut().clear();
    let first = sweeps.len();
    catch_up(&memory, &mut Builder::default(), DRAIN_PASSES, &mut sweeps);
    let requests = memory
        .callers
        .borrow()
        .iter()
        .filter(|(op, caller)| {
            matches!(*op, "ledger_row" | "ledger_rows_of") && *caller == Some("ledger.fold_rows")
        })
        .count();
    let swept = sweeps[first..].iter().map(|sweep| sweep.reads).sum();
    FromTheStart {
        events: log.events.len(),
        requests,
        swept,
        sweeps,
        rows: memory.rows.borrow().clone(),
        written: memory.written_keys.borrow().clone(),
    }
}

/// From the cursors' start over 5,000 and 20,000 events, the requests of
/// the sweep stay within a few a page of events read
/// ([`EVENTS_PER_READ`]), each share of [`CHECKS_PER_PASS`] rows one, and
/// at the larger at most one in a hundred of the rows swept; read one by
/// one they were one a row.
#[test]
fn a_sweep_from_the_cursors_start_sends_a_request_a_share_not_a_row() {
    for settled in [1_667, 6_667] {
        let start = from_the_start(settled, false);
        let pages = start.events.div_ceil(EVENTS_PER_READ);
        let shares_a_page = (2 * usize::try_from(settled).unwrap()).div_ceil(CHECKS_PER_PASS);
        assert!(
            start.swept >= 2 * usize::try_from(settled).unwrap(),
            "{}",
            start.swept
        );
        assert!(start.requests > 0);
        assert!(
            start.requests <= (pages + 1) * shares_a_page,
            "{} requests, {pages} pages",
            start.requests
        );
        assert!(
            start.requests * 100 <= start.swept,
            "{} requests for {} rows",
            start.requests,
            start.swept
        );
    }
    let one_by_one = from_the_start(1_667, true);
    assert_eq!(one_by_one.requests, one_by_one.swept);
}

/// Read in shares or one by one, the sweeps judge the rows alike, the
/// builders write the same rows in the same order, and the store holds the
/// same rows after (kind, key, records, ledger_v and the rest).
#[test]
fn the_rows_read_in_shares_sweep_and_write_as_the_rows_read_one_by_one() {
    let (shares, one_by_one) = (from_the_start(1_667, false), from_the_start(1_667, true));
    assert_eq!(shares.sweeps, one_by_one.sweeps);
    assert_eq!(shares.written, one_by_one.written);
    assert!(!shares.written.is_empty());
    assert_eq!(shares.rows, one_by_one.rows);
}
