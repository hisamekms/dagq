use serde_json::json;

use super::*;
use crate::domain::ledger::Totals;
use crate::domain::ledger::compare::RowState;
use crate::domain::ledger::fold::{Interval, UNATTRIBUTED};
use crate::domain::run_phase::{Blocker, Holds};

fn interval(blocker: Option<Blocker>, holds: Option<Holds>, start: i64, end: i64) -> Interval {
    Interval {
        phase: if blocker.is_some() {
            "work".to_owned()
        } else {
            UNATTRIBUTED.to_owned()
        },
        blocker,
        holds,
        attempt: None,
        start,
        end,
        cause: None,
        blocked_by: None,
        run_id: None,
    }
}

/// A row of `kind` and `key` whose totals are of `intervals`.
fn row(kind: LedgerKind, key: &str, intervals: Vec<Interval>) -> LedgerRow {
    let mut row = LedgerRow::new(kind, key.to_owned(), None, "2030-01-01T00:00:00.000Z");
    row.totals = Totals::of(&intervals);
    row.intervals = intervals;
    row
}

fn state(kind: LedgerKind, key: &str, is_final: bool, up_to_date: bool) -> RowState {
    RowState {
        kind,
        key: key.to_owned(),
        row: true,
        is_final,
        push_pending: false,
        up_to_date,
        reach: 1,
        folded: Some(1),
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// Only the run and task rows the reconciliation finds settled and up to
/// date are summed, with their milliseconds kept; a dirty row, one not
/// final, one pending on its push, one missing and one outside the window
/// (not among the reconciliation's rows) are not, and a legacy row is
/// counted apart. Each sum by blocker and by holds, with the unattributed
/// time, is the whole duration.
#[test]
fn only_settled_up_to_date_rows_are_summed_and_the_tags_add_up() {
    use {Blocker as B, Holds as H, LedgerKind::Run, LedgerKind::Task};
    let worked = || {
        vec![
            interval(Some(B::Ai), Some(H::WorkerSlot), 0, 1_500),
            interval(None, None, 1_500, 1_833),
            interval(Some(B::Queue), Some(H::None), 1_833, 4_083),
            interval(Some(B::Compute), Some(H::LandingSlot), 4_083, 4_084),
        ]
    };
    let mut legacy = row(Run, "legacy", Vec::new());
    legacy.legacy = Some(json!({"work": 10}));
    let stored = vec![
        row(Run, "a", worked()),
        row(Run, "dirty", worked()),
        row(Run, "open", worked()),
        row(Run, "pending", worked()),
        row(Run, "outside", worked()),
        legacy,
        row(
            Task,
            "1",
            vec![
                interval(Some(B::Human), Some(H::None), 0, 2_001),
                interval(None, None, 2_001, 2_002),
            ],
        ),
        row(Task, "2", worked()),
    ];
    let pending = RowState {
        push_pending: true,
        is_final: false,
        ..state(Run, "pending", false, true)
    };
    let missing = RowState {
        row: false,
        ..state(Run, "missing", false, false)
    };
    let reconciled = Reconciliation {
        from_ms: None,
        until_ms: 10_000,
        kinds: Vec::new(),
        rows: vec![
            state(Run, "a", true, true),
            state(Run, "dirty", true, false),
            state(Run, "open", false, true),
            pending,
            missing,
            state(Run, "legacy", true, true),
            state(Task, "1", true, true),
            state(Task, "2", true, false),
        ],
    };

    let totals = totals(&reconciled, &stored);
    let run = &totals.run;
    assert_eq!((run.rows, run.legacy), (1, 1));
    assert!(close(run.duration_secs, 4.084), "{run:?}");
    assert!(close(run.slot_secs, 1.501), "{run:?}");
    assert!(close(run.unattributed_secs, 0.333), "{run:?}");
    assert!(close(run.unattributed_ratio.unwrap(), 333.0 / 4084.0));
    assert_eq!(
        run.by_blocker["ai"],
        Seconds {
            duration_secs: 1.5,
            slot_secs: 1.5
        }
    );
    assert!(close(run.by_blocker["queue"].duration_secs, 2.25));
    assert_eq!(run.by_blocker["queue"].slot_secs, 0.0);
    assert!(close(run.by_holds["landing_slot"].slot_secs, 0.001));
    // Every blocker and holds is listed, a zero one too.
    assert_eq!(run.by_blocker.len(), 7);
    assert_eq!(run.by_holds.len(), 3);
    assert_eq!(run.by_blocker["infra"], Seconds::default());

    let task = &totals.task;
    assert_eq!((task.rows, task.legacy), (1, 0));
    assert!(close(task.duration_secs, 2.002));
    assert!(close(task.unattributed_secs, 0.001));
    assert!(close(task.by_blocker["human"].duration_secs, 2.001));
    assert_eq!(task.slot_secs, 0.0);

    for kind in [run, task] {
        let sum = |by: &BTreeMap<String, Seconds>| {
            by.values().map(|s| s.duration_secs).sum::<f64>() + kind.unattributed_secs
        };
        assert!(close(sum(&kind.by_blocker), kind.duration_secs), "{kind:?}");
        assert!(close(sum(&kind.by_holds), kind.duration_secs), "{kind:?}");
        let slots = |by: &BTreeMap<String, Seconds>| by.values().map(|s| s.slot_secs).sum::<f64>();
        assert!(close(slots(&kind.by_blocker), kind.slot_secs));
        assert!(close(slots(&kind.by_holds), kind.slot_secs));
    }

    // Nothing taken: no ratio.
    let none = super::totals(
        &Reconciliation {
            rows: Vec::new(),
            ..reconciled
        },
        &stored,
    );
    assert_eq!((none.run.rows, none.run.unattributed_ratio), (0, None));
}
