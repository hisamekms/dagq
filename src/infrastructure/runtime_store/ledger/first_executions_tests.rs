//! The first-Execution view is written with rows, backfilled by migration,
//! and has the same coverage meaning as the omitted rows' GROUP BY.
use super::*;
use crate::domain::ledger;
use crate::domain::ledger::testkit::{Log, ms, run_id};
use crate::domain::stats::executions;
use serde_json::json;

type Firsts = Vec<(String, String, i64)>;

fn held(queue: &SqliteQueue) -> Firsts {
    queue
        .conn
        .prepare("SELECT actor, provider, at FROM ledger_first_executions ORDER BY actor, provider")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn rows() -> Vec<LedgerRow> {
    let mut log = Log::default();
    // The second run is kept by its later end, even though the clock went
    // backwards for its Execution. Minima lie on both sides of RowsFrom.
    for (n, start, turns) in [
        (
            1,
            0,
            vec![
                ("turn_finished", "claude", 50),
                ("turn_finished", "codex", 20),
            ],
        ),
        (
            2,
            1000,
            vec![
                ("turn_finished", "claude", 10),
                ("turn_finished", "codex", 1100),
                ("review_finished", "codex", 1050),
            ],
        ),
    ] {
        let run = run_id(n);
        log.landed(i64::from(n), &run, start);
        for (kind, provider, at) in turns {
            log.add(
                Some(i64::from(n)),
                Some(&run),
                kind,
                json!({"turn": 1, "provider": provider, "tokens": {"input": 1},
                       "tokens_source": "model_usage", "tokens_by_model": []}),
                at,
            );
        }
    }
    ledger::build(&log.events, ms(3000), "t")
        .into_iter()
        .filter(|row| row.kind == LedgerKind::Run)
        .collect()
}

/// Kept rows supply their own minima to all, including the pair whose
/// minimum precedes the omitted pair's. A kept-only pair is not lost.
#[test]
fn the_view_and_the_omitted_group_by_give_the_same_window_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let rows = rows();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        queue.write_ledger_row(row).unwrap();
    }
    let from = RowsFrom {
        ended_before_ms: ms(950),
        inputs_through: EventId::new(10000),
    };
    let (omitted, kept): (Vec<_>, Vec<_>) = rows.iter().partition(|row| from.leaves_out(row));
    assert_eq!((omitted.len(), kept.len()), (1, 1));
    let all = executions::executions(
        &kept
            .iter()
            .flat_map(|row| row.records.iter().cloned())
            .collect::<Vec<_>>(),
    );
    // Guard the contract: every minimum of a kept detail's pair is also
    // represented by the Execution records execution_tokens reads as all.
    for row in kept {
        for first in ledger::builder::first_executions(row) {
            assert_eq!(
                all.iter()
                    .filter(|e| e.actor == first.actor && e.provider == first.provider)
                    .map(|e| e.at)
                    .min(),
                Some(first.at)
            );
        }
    }
    let tx = queue.read_entry("first_executions_test").unwrap();
    let old = earlier_in(&tx, &from).unwrap().executions;
    let view: Vec<_> = held(&queue)
        .into_iter()
        .map(|(actor, provider, at)| FirstExecution::of(&actor, provider, at).unwrap())
        .collect();
    assert_ne!(view, old);
    assert_eq!(
        held(&queue),
        vec![
            ("review".to_owned(), "codex".to_owned(), ms(1050)),
            ("worker".to_owned(), "claude".to_owned(), ms(10)),
            ("worker".to_owned(), "codex".to_owned(), ms(20)),
        ]
    );
    for start in [
        None,
        Some(ms(5)),
        Some(ms(15)),
        Some(ms(30)),
        Some(ms(1000)),
    ] {
        // Compare the whole answer, which includes recorded_from and
        // coverage globally, by actor, and by provider under each actor.
        assert_eq!(
            executions::window_after(&all, &view, start, ms(3000), |_| true),
            executions::window_after(&all, &old, start, ms(3000), |_| true)
        );
    }
}

/// Migration backfills existing rows before publishing its ready mark;
/// replaying events into an empty view produces precisely the same table.
#[test]
fn the_migration_backfills_the_view_and_publishes_readiness_afterwards() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    for row in rows().iter().rev() {
        queue.write_ledger_row(row).unwrap();
    }
    let rebuilt = held(&queue);
    assert_eq!(rebuilt.len(), 3);
    queue
        .conn
        .execute_batch(
            "DROP TABLE ledger_first_executions; DROP TABLE ledger_first_executions_ready;",
        )
        .unwrap();
    let migration = crate::infrastructure::schema::MIGRATIONS
        .iter()
        .find(|sql| sql.contains("CREATE TABLE ledger_first_executions ("))
        .unwrap();
    let (backfill, mark) = migration
        .split_once("INSERT INTO ledger_first_executions_ready")
        .unwrap();
    queue.conn.execute_batch(backfill).unwrap();
    let ready = || {
        queue
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM ledger_first_executions_ready WHERE singleton=1)",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
    };
    assert!(!ready());
    assert_eq!(held(&queue), rebuilt);
    queue
        .conn
        .execute_batch(&format!("INSERT INTO ledger_first_executions_ready{mark}"))
        .unwrap();
    assert!(ready());
    queue
        .conn
        .execute_batch(
            crate::infrastructure::schema::MIGRATIONS
                .iter()
                .find(|sql| {
                    sql.contains("ALTER TABLE ledger_first_executions_ready ADD COLUMN generation")
                })
                .unwrap(),
        )
        .unwrap();
    // A later writer with a clock further back replaces the minimum, and
    // writing the same or a later minimum cannot move it forwards.
    let mut row = rows().remove(0);
    row.detail["executions"][0]["at"] = json!(ms(-50));
    queue.write_ledger_row(&row).unwrap();
    assert!(held(&queue).iter().any(|(_, _, at)| *at == ms(-50)));
    for row in rows() {
        queue.write_ledger_row(&row).unwrap();
    }
    assert!(held(&queue).iter().any(|(_, _, at)| *at == ms(-50)));
}

/// The table grows with pairs alone, however many settled run rows the
/// time range omits. Repeated writes are idempotent too.
#[test]
fn thirty_and_three_thousand_omitted_runs_keep_only_the_pairs() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let mut row = rows().remove(0);
    row.is_final = true;
    let from = RowsFrom {
        ended_before_ms: ms(950),
        inputs_through: EventId::new(10000),
    };
    assert!(from.leaves_out(&row));
    let tx = queue.write_entry("first_executions_test").unwrap();
    for n in 0..3000 {
        row.key = format!("old-{n}");
        upsert(&tx, &row).unwrap();
        if n == 29 || n == 2999 {
            let count: i64 = SqlRead::query_row(
                &tx,
                "SELECT COUNT(*) FROM ledger_first_executions",
                &[],
                |r| r.get(0),
            )
            .unwrap();
            assert_eq!(count, 2);
            let runs: i64 = SqlRead::query_row(
                &tx,
                "SELECT COUNT(*) FROM ledger_rows WHERE kind='run'",
                &[],
                |r| r.get(0),
            )
            .unwrap();
            assert_eq!(runs, n + 1);
        }
    }
    tx.commit().unwrap();
    assert_eq!(held(&queue).len(), 2);
}
