//! The SessionStepStore in queue.db (`session_steps`, ADR-t1662-2
//! decision 4, ADR-t1662-3 decisions 8, 10 and 12): one row per
//! `(session_id, seq)` with its gaps, the cursor of the order the rows
//! came in, each step with its target and read by it, no column for what
//! a session said, and the retention the ledger's pass runs. The
//! decisions (the gaps, the check of a step, when the retention is due)
//! are the unit tests of `domain::session_step` and
//! `application::supervise::ledger`.
use dagq::application::{
    SessionStepStore,
    supervise::{Retain, build_pass},
};
use dagq::domain::RunId;
use dagq::domain::ledger::builder::Builder;
use dagq::domain::session_step::{
    RETENTION_MS, SeqRange, SessionStep, Step, StepKind, StepTarget, StepsRecorded, TargetKind,
    retained_since,
};
use dagq::infrastructure::sqlite::SqliteQueue;
use serde_json::json;

const RUN: &str = "11111111-1111-4111-8111-111111111111";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Noon of a day, in unix seconds.
const NOW: i64 = 20_500 * 86_400 + 43_200;

fn run_target() -> StepTarget {
    StepTarget::run(&RunId::new(RUN).unwrap())
}

fn planner_target() -> StepTarget {
    StepTarget {
        kind: TargetKind::Planner,
        id: "12".to_owned(),
    }
}

fn steps(seqs: &[i64]) -> Vec<Step> {
    seqs.iter()
        .map(|&seq| Step::new(seq, StepKind::Wait, seq * 100, 50))
        .collect()
}

fn seqs(steps: &[SessionStep]) -> Vec<(String, i64)> {
    steps
        .iter()
        .map(|step| (step.session_id.clone(), step.step.seq))
        .collect()
}

/// A step sent again is one row whatever it says now, the missing seqs
/// are the session's gaps, and the cursor reads the steps that came in
/// after a step's id, in that order and by the limit.
#[test]
fn a_session_keeps_one_row_per_seq_with_its_gaps_and_a_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let target = run_target();
    let mut turn = Step::new(1, StepKind::ModelTurn, 0, 1_200);
    turn.model = Some("claude-opus-5-5".to_owned());
    let mut call = Step::new(2, StepKind::ToolCall, 1_300, 4_000);
    call.tool = Some("Bash".to_owned());
    call.command_shape = Some(json!({"detail": "shape", "parts": [{"program": "cargo"}]}));
    call.exit_code = Some(0);
    call.background = Some(false);
    call.output_bytes = Some(2_048);
    let mut first = vec![turn.clone(), call.clone()];
    first.extend(steps(&[4]));
    assert_eq!(
        queue.record_steps(&target, "s-1", &first, 1_000).unwrap(),
        StepsRecorded {
            added: 3,
            duplicates: 0
        }
    );
    let mut again = Step::new(2, StepKind::Wait, 9, 9);
    again.tool = Some("Other".to_owned());
    let second = vec![again, Step::new(7, StepKind::Subagent, 9_000, 10)];
    assert_eq!(
        queue.record_steps(&target, "s-1", &second, 2_000).unwrap(),
        StepsRecorded {
            added: 1,
            duplicates: 1
        }
    );
    queue
        .record_steps(&target, "s-2", &steps(&[1]), 3_000)
        .unwrap();

    let held = queue.target_steps(&target).unwrap();
    assert_eq!(
        seqs(&held),
        [("s-1", 1), ("s-1", 2), ("s-1", 4), ("s-1", 7), ("s-2", 1)]
            .map(|(session, seq)| (session.to_owned(), seq))
    );
    // The step sent again kept what it said first.
    assert_eq!(held[0].step, turn);
    assert_eq!(held[1].step, call);
    assert_eq!(held[1].received_at, 1_000);
    assert_eq!(held[3].received_at, 2_000);
    assert_eq!(
        queue.session_gaps("s-1").unwrap(),
        vec![SeqRange { from: 3, to: 3 }, SeqRange { from: 5, to: 6 }]
    );
    assert_eq!(queue.session_gaps("s-2").unwrap(), vec![]);
    assert_eq!(queue.session_gaps("none").unwrap(), vec![]);

    // The cursor: the ids ascend in the order the rows came in.
    let all = queue.steps_after(0, 100).unwrap();
    assert_eq!(all, held);
    assert!(all.windows(2).all(|pair| pair[0].id < pair[1].id));
    let after = queue.steps_after(all[2].id, 1).unwrap();
    assert_eq!(seqs(&after), vec![("s-1".to_owned(), 7)]);
    assert_eq!(queue.steps_after(all[4].id, 100).unwrap(), vec![]);

    // Steps that do not hold together record none of the call.
    let error = queue
        .record_steps(&target, "s-3", &[steps(&[1]), steps(&[0])].concat(), 4_000)
        .unwrap_err();
    assert!(error.to_string().contains("before 1"), "{error:#}");
    assert_eq!(queue.steps_after(all[4].id, 100).unwrap(), vec![]);
}

/// A run's steps and a target of no run's are each kept with their target
/// and read by it; a session's steps sent under another target are
/// refused whole.
#[test]
fn each_step_keeps_its_target_and_is_read_by_it() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let (run, planner) = (run_target(), planner_target());
    queue
        .record_steps(&run, "worker", &steps(&[1, 2]), 1_000)
        .unwrap();
    queue
        .record_steps(&planner, "planner", &steps(&[1]), 1_000)
        .unwrap();

    let of_run = queue.target_steps(&run).unwrap();
    assert_eq!(seqs(&of_run).len(), 2);
    assert!(of_run.iter().all(|step| step.target == run));
    assert_eq!(of_run[0].target.run_id(), Some(RunId::new(RUN).unwrap()));
    let of_planner = queue.target_steps(&planner).unwrap();
    assert_eq!(seqs(&of_planner), vec![("planner".to_owned(), 1)]);
    assert_eq!(of_planner[0].target.kind, TargetKind::Planner);
    assert_eq!(of_planner[0].target.id, "12");
    assert_eq!(of_planner[0].target.run_id(), None);
    let other_run = StepTarget::run(&RunId::new("22222222-2222-4222-8222-222222222222").unwrap());
    assert_eq!(queue.target_steps(&other_run).unwrap(), vec![]);

    let error = queue
        .record_steps(&planner, "worker", &steps(&[3]), 2_000)
        .unwrap_err();
    assert!(error.to_string().contains("another target"), "{error:#}");
    assert_eq!(queue.session_gaps("worker").unwrap(), vec![]);
    assert_eq!(queue.target_steps(&run).unwrap(), of_run);
}

/// The table has a column for what a step ran and none for a prompt, an
/// output, a command line or tokens (ADR-t1662-3 decision 8).
#[test]
fn the_table_has_no_column_for_what_a_session_said() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    SqliteQueue::init(&db).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    let mut statement = conn
        .prepare("SELECT name FROM pragma_table_info('session_steps') ORDER BY cid")
        .unwrap();
    let columns: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        columns,
        [
            "id",
            "target_kind",
            "target_id",
            "session_id",
            "seq",
            "kind",
            "offset_ms",
            "duration_ms",
            "model",
            "tool",
            "command_shape",
            "exit_code",
            "background",
            "output_bytes",
            "received_at",
        ]
    );
    for word in [
        "prompt", "text", "body", "line", "token", "input", "content",
    ] {
        assert!(
            columns.iter().all(|column| !column.contains(word)),
            "{word} in {columns:?}"
        );
    }
}

/// The ledger's pass with the retention due removes each step that came
/// in more than 90 days before and keeps the newer ones, also within one
/// session; the seqs it removed open no gap and a resend of one adds no
/// row; a later step's id is past every removed one's, so a cursor holds.
#[test]
fn the_ledgers_pass_removes_the_steps_past_ninety_days() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let now_ms = NOW * 1000;
    let target = run_target();
    let old = now_ms - RETENTION_MS - DAY_MS;
    let new = now_ms - DAY_MS;
    queue
        .record_steps(&target, "old", &steps(&[1, 2]), old)
        .unwrap();
    // A session with old steps and new ones keeps only the new ones.
    queue
        .record_steps(&target, "both", &steps(&[1, 2]), old)
        .unwrap();
    queue
        .record_steps(&target, "both", &steps(&[3, 5]), new)
        .unwrap();
    queue
        .record_steps(&planner_target(), "new", &steps(&[1]), new)
        .unwrap();
    let last = queue.steps_after(0, 100).unwrap().last().unwrap().id;

    let retain = Retain::due(NOW, NOW / 86_400, None, None);
    let mut builder = Builder::default();
    let built = build_pass(
        &queue,
        &mut builder,
        now_ms,
        "2026-01-01T00:00:00.000Z",
        retain,
    );
    assert_eq!(built.failure, None);
    assert_eq!(built.steps_removed, 4);
    let kept = queue.steps_after(0, 100).unwrap();
    assert_eq!(
        seqs(&kept),
        [("both", 3), ("both", 5), ("new", 1)].map(|(session, seq)| (session.to_owned(), seq))
    );
    assert!(kept.iter().all(|step| step.received_at == new));
    assert_eq!(queue.target_steps(&target).unwrap().len(), 2);
    // The removed seqs 1 and 2 are no gap; the missing 4 still is.
    assert_eq!(
        queue.session_gaps("both").unwrap(),
        vec![SeqRange { from: 4, to: 4 }]
    );
    assert_eq!(queue.session_gaps("old").unwrap(), vec![]);
    // A removed step sent again adds no row; a missing one fills its gap.
    assert_eq!(
        queue
            .record_steps(&target, "both", &steps(&[2, 4]), now_ms)
            .unwrap(),
        StepsRecorded {
            added: 1,
            duplicates: 1
        }
    );
    assert_eq!(queue.session_gaps("both").unwrap(), vec![]);

    // Nothing more is due: the next removal writes nothing.
    assert_eq!(queue.remove_steps(retain.steps.unwrap()).unwrap(), 0);
    queue
        .record_steps(&target, "after", &steps(&[1]), now_ms)
        .unwrap();
    let added = queue.steps_after(last, 100).unwrap();
    assert_eq!(
        seqs(&added),
        [("both", 4), ("after", 1)].map(|(session, seq)| (session.to_owned(), seq))
    );
    assert!(added.iter().all(|step| step.id > last));

    // A session the retention removed whole keeps none of its seqs: its
    // steps sent again are a new session's.
    queue
        .record_steps(&target, "old", &steps(&[1]), now_ms)
        .unwrap();
    assert_eq!(queue.session_gaps("old").unwrap(), vec![]);
    assert_eq!(
        seqs(&queue.steps_after(last, 100).unwrap()).last(),
        Some(&("old".to_owned(), 1))
    );
}

/// Steps that come in out of the order of their seqs: the retention
/// removes an older step with a larger seq, and the seqs below it that
/// never came in stay gaps and are recorded when they come in, while the
/// removed seq is no gap and a resend of it adds no row.
#[test]
fn the_retention_tells_a_removed_seq_from_one_never_received() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let now_ms = NOW * 1000;
    let target = run_target();
    let old = now_ms - RETENTION_MS - DAY_MS;
    let new = now_ms - DAY_MS;
    queue
        .record_steps(&target, "s", &steps(&[5, 8, 9]), old)
        .unwrap();
    queue
        .record_steps(&target, "s", &steps(&[1, 6]), new)
        .unwrap();
    assert_eq!(queue.remove_steps(retained_since(now_ms)).unwrap(), 3);
    let kept = queue.target_steps(&target).unwrap();
    assert_eq!(
        seqs(&kept),
        [("s", 1), ("s", 6)].map(|(session, seq)| (session.to_owned(), seq))
    );
    // 2 to 4 and 7 never came in; 5, 8 and 9 were removed.
    assert_eq!(
        queue.session_gaps("s").unwrap(),
        vec![SeqRange { from: 2, to: 4 }, SeqRange { from: 7, to: 7 }]
    );
    assert_eq!(
        queue
            .record_steps(&target, "s", &steps(&[2, 5, 7, 9]), now_ms)
            .unwrap(),
        StepsRecorded {
            added: 2,
            duplicates: 2
        }
    );
    assert_eq!(
        queue.session_gaps("s").unwrap(),
        vec![SeqRange { from: 3, to: 4 }]
    );
    let kept = queue.target_steps(&target).unwrap();
    assert_eq!(
        seqs(&kept),
        [("s", 1), ("s", 6), ("s", 2), ("s", 7)].map(|(session, seq)| (session.to_owned(), seq))
    );

    // A later retention, removing 1 and 6, keeps the seqs removed before
    // with its own.
    assert_eq!(queue.remove_steps(now_ms).unwrap(), 2);
    assert_eq!(
        queue.session_gaps("s").unwrap(),
        vec![SeqRange { from: 3, to: 4 }]
    );

    // Removing its last steps removes the session whole with the seqs kept
    // of it: a resend of a seq removed before is a new step again.
    assert_eq!(queue.remove_steps(now_ms + 1).unwrap(), 2);
    assert_eq!(queue.target_steps(&target).unwrap(), vec![]);
    assert_eq!(queue.session_gaps("s").unwrap(), vec![]);
    assert_eq!(
        queue
            .record_steps(&target, "s", &steps(&[5]), now_ms + 1)
            .unwrap(),
        StepsRecorded {
            added: 1,
            duplicates: 0
        }
    );
    assert_eq!(
        queue.session_gaps("s").unwrap(),
        vec![SeqRange { from: 1, to: 4 }]
    );
}

/// A session's largest seq reads its gaps without overflowing.
#[test]
fn a_session_at_the_largest_seq_reads_its_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    queue
        .record_steps(&run_target(), "s", &steps(&[1, i64::MAX / 100]), 1_000)
        .unwrap();
    let last = Step::new(i64::MAX, StepKind::Wait, 0, 0);
    queue
        .record_steps(&run_target(), "s", &[last], 1_000)
        .unwrap();
    assert_eq!(
        queue.session_gaps("s").unwrap(),
        vec![
            SeqRange {
                from: 2,
                to: i64::MAX / 100 - 1
            },
            SeqRange {
                from: i64::MAX / 100 + 1,
                to: i64::MAX - 1
            },
        ]
    );
}
