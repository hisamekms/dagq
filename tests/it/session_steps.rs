//! The SessionStepStore in queue.db (`session_steps`, ADR-t1662-2
//! decision 4, ADR-t1662-3 decisions 8, 10 and 12): one row per
//! `(session_id, seq)` with its gaps, the cursor of the order the rows
//! came in, each step with its target and read by it, no column for what
//! a session said, and the retention the ledger's pass runs; the
//! sessions' owners the control side registers and their resource samples
//! (`session_owners`, `session_resources`). The
//! decisions (the gaps, the check of a step, when the retention is due)
//! are the unit tests of `domain::session_step` and
//! `application::supervise::ledger`.
use dagq::application::{
    SessionOwners, SessionResourceStore, SessionStepStore,
    supervise::{Retain, build_pass},
};
use dagq::domain::RunId;
use dagq::domain::ledger::builder::Builder;
use dagq::domain::session_step::{
    RETENTION_MS, ResourceSample, SeqRange, SessionStep, Step, StepKind, StepTarget, StepsRecorded,
    TargetKind, retained_since,
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
        (now_ms, "2026-01-01T00:00:00.000Z"),
        retain,
        None,
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

/// A session is registered to one target: again to it changes nothing, to
/// another is refused, and so are another target's steps and samples of
/// it. Its samples are one row per seq with their target; the retention
/// removes the old samples with the steps, and the old owners whose
/// session keeps nothing.
#[test]
fn a_sessions_owner_and_samples_are_one_targets_and_the_retention_removes_them() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let (run, planner) = (run_target(), planner_target());
    let now_ms = NOW * 1000;
    let old = now_ms - RETENTION_MS - DAY_MS;
    queue.register_session(&run, "worker", old).unwrap();
    queue.register_session(&run, "worker", now_ms).unwrap();
    let owner = queue.session_owner("worker").unwrap().unwrap();
    assert_eq!((owner.target, owner.registered_at), (run.clone(), old));
    assert_eq!(queue.session_owner("nobody's").unwrap(), None);
    let another = |error: anyhow::Error| error.to_string().contains("another target");
    assert!(another(
        queue
            .register_session(&planner, "worker", now_ms)
            .unwrap_err()
    ));
    assert!(another(
        queue
            .record_steps(&planner, "worker", &steps(&[1]), now_ms)
            .unwrap_err()
    ));
    let mut sample = ResourceSample::new(1, 0, 30_000);
    sample.cpu_seconds = Some(2.5);
    sample.memory_max_bytes = Some(1 << 30);
    sample.node_id = Some("host".to_owned());
    assert!(another(
        queue
            .record_resources(&planner, "worker", &[sample.clone()], now_ms)
            .unwrap_err()
    ));
    // Steps that came in first hold the session for their target too.
    queue
        .record_steps(&planner, "planner", &steps(&[1]), now_ms)
        .unwrap();
    assert!(another(
        queue.register_session(&run, "planner", now_ms).unwrap_err()
    ));

    let second = ResourceSample::new(2, 30_000, 30_000);
    assert_eq!(
        queue
            .record_resources(&run, "worker", &[sample.clone(), second.clone()], old)
            .unwrap(),
        StepsRecorded {
            added: 2,
            duplicates: 0
        }
    );
    assert_eq!(
        queue
            .record_resources(&run, "worker", std::slice::from_ref(&second), now_ms)
            .unwrap(),
        StepsRecorded {
            added: 0,
            duplicates: 1
        }
    );
    let held = queue.target_resources(&run).unwrap();
    assert_eq!(
        held.iter()
            .map(|row| row.sample.clone())
            .collect::<Vec<_>>(),
        [sample, second]
    );
    assert!(
        held.iter()
            .all(|row| row.target == run && row.session_id == "worker")
    );
    assert_eq!(queue.target_resources(&planner).unwrap(), vec![]);
    let mut negative = ResourceSample::new(3, 0, 1);
    negative.read_bytes = Some(-1);
    assert!(
        queue
            .record_resources(&run, "worker", &[negative], now_ms)
            .is_err()
    );

    // The old samples go; the old owner of a session that keeps nothing
    // goes, one whose session keeps a step stays.
    queue.register_session(&run, "kept", old).unwrap();
    queue
        .record_steps(&run, "kept", &steps(&[1]), now_ms)
        .unwrap();
    queue.remove_steps(retained_since(now_ms)).unwrap();
    assert_eq!(queue.target_resources(&run).unwrap(), vec![]);
    assert_eq!(queue.session_owner("worker").unwrap(), None);
    assert!(queue.session_owner("kept").unwrap().is_some());
    // Nothing more is due.
    assert_eq!(queue.remove_steps(retained_since(now_ms)).unwrap(), 0);
}

/// What `sql` (a count) reads in the queue at `db`.
fn count(db: &std::path::Path, sql: &str) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

/// The seqs `session_step_removed` keeps of `session`, one by one: each
/// once, whatever transactions recorded them.
fn removed_seqs(db: &std::path::Path, session: &str) -> Vec<i64> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut statement = conn
        .prepare("SELECT from_seq, to_seq FROM session_step_removed WHERE session_id=?1")
        .unwrap();
    let mut seqs: Vec<i64> = statement
        .query_map([session], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })
        .unwrap()
        .flat_map(|range| {
            let (from, to) = range.unwrap();
            from..=to
        })
        .collect();
    seqs.sort_unstable();
    seqs
}

fn upto(last: i64) -> Vec<i64> {
    (1..=last).collect()
}

/// The rows each retention transaction wrote, by the WARN a queue opened
/// with no threshold ([`SqliteQueue::with_write_hold_warn`]) gives of every
/// write transaction as it ends, on the thread that sets it.
#[derive(Clone, Default)]
struct RetentionRows(std::sync::Arc<std::sync::Mutex<Vec<u64>>>);

impl RetentionRows {
    fn rows(&self) -> Vec<u64> {
        self.0.lock().unwrap().clone()
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RetentionRows {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        #[derive(Default)]
        struct Fields {
            transaction: String,
            rows: Option<u64>,
        }
        impl tracing::field::Visit for Fields {
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                if field.name() == "rows" {
                    self.rows = Some(value);
                }
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "transaction" {
                    self.transaction = value.to_owned();
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.transaction == "session_steps_retention" {
            self.0
                .lock()
                .unwrap()
                .push(fields.rows.expect("a long hold's WARN has rows"));
        }
    }
}

/// Watch the retention transactions of `queue` (opened with no threshold)
/// on this thread, until the guard drops.
fn retention_rows() -> (RetentionRows, tracing::subscriber::DefaultGuard) {
    use tracing_subscriber::layer::SubscriberExt;
    let rows = RetentionRows::default();
    let guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(rows.clone()));
    (rows, guard)
}

/// More rows due than one transaction holds: the retention commits them a
/// bounded number at a time, so one stopped part way has removed the
/// batches before it with their seqs, and the next finishes the rest. A
/// step another connection adds between the two to a session being
/// removed is kept and its seq not counted as removed; the session that
/// keeps it keeps every seq removed of it, recorded once. After it a
/// retention of the same day takes no write lock.
#[test]
fn the_steps_retention_commits_by_batches_resumes_and_keeps_a_step_added_between() {
    use dagq::infrastructure::runtime_store::ROWS_PER_TRANSACTION as BATCH;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db).unwrap();
    let target = run_target();
    let now_ms = NOW * 1000;
    let old = now_ms - RETENTION_MS - DAY_MS;
    let new = now_ms - DAY_MS;
    let before = retained_since(now_ms);
    let last = i64::try_from(BATCH).unwrap() + 20;
    queue
        .record_steps(&target, "a-kept", &steps(&upto(last)), old)
        .unwrap();
    queue
        .record_steps(&target, "a-kept", &steps(&[last + 1]), new)
        .unwrap();
    queue.register_session(&target, "b-whole", old).unwrap();
    queue
        .record_steps(&target, "b-whole", &steps(&upto(50)), old)
        .unwrap();
    queue
        .record_resources(&target, "b-whole", &[ResourceSample::new(1, 0, 1)], old)
        .unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(&format!(
        "CREATE TRIGGER stop BEFORE DELETE ON session_steps
         WHEN old.session_id = 'a-kept' AND old.seq = {}
         BEGIN SELECT RAISE(ABORT, 'stopped'); END",
        last - 5
    ))
    .unwrap();
    let error = queue.remove_steps(before).unwrap_err();
    assert!(format!("{error:#}").contains("stopped"), "{error:#}");
    // The first batch went with its seqs (its range row counts in it);
    // the rest is as it was.
    let first = i64::try_from(BATCH).unwrap() - 1;
    assert_eq!(removed_seqs(&db, "a-kept"), upto(first));
    assert_eq!(queue.session_gaps("a-kept").unwrap(), vec![]);
    assert_eq!(queue.target_steps(&target).unwrap().len(), 21 + 1 + 50);
    assert!(queue.session_owner("b-whole").unwrap().is_some());

    // Another connection adds a step to each session in between.
    let other = SqliteQueue::open(&db).unwrap();
    other
        .record_steps(&target, "a-kept", &steps(&[last + 10]), now_ms)
        .unwrap();
    other
        .record_steps(&target, "b-whole", &steps(&[60]), now_ms)
        .unwrap();
    conn.execute_batch("DROP TRIGGER stop").unwrap();
    assert_eq!(queue.remove_steps(before).unwrap(), 21 + 50);
    let kept = queue.target_steps(&target).unwrap();
    assert_eq!(
        seqs(&kept),
        [("a-kept", last + 1), ("a-kept", last + 10), ("b-whole", 60)]
            .map(|(session, seq)| (session.to_owned(), seq))
    );
    assert_eq!(removed_seqs(&db, "a-kept"), upto(last));
    assert_eq!(removed_seqs(&db, "b-whole"), upto(50));
    assert_eq!(
        queue.session_gaps("a-kept").unwrap(),
        vec![SeqRange {
            from: last + 2,
            to: last + 9
        }]
    );
    assert_eq!(
        queue.session_gaps("b-whole").unwrap(),
        vec![SeqRange { from: 51, to: 59 }]
    );
    assert_eq!(queue.target_resources(&target).unwrap(), vec![]);
    assert!(queue.session_owner("b-whole").unwrap().is_some());
    assert_eq!(
        queue
            .record_steps(&target, "a-kept", &steps(&[1, last - 5]), now_ms)
            .unwrap(),
        StepsRecorded {
            added: 0,
            duplicates: 2
        }
    );

    queue.set_busy_timeout(std::time::Duration::ZERO).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(queue.remove_steps(before).unwrap(), 0);
    conn.execute_batch("ROLLBACK").unwrap();
}

/// While the retention removes a session's many steps, another
/// connection's heartbeat gets the write lock between two of its
/// transactions, within its busy wait: it sees some steps removed and some
/// not yet. The session, its sample and its owner all go, with no seq
/// kept of it.
#[test]
fn a_heartbeat_writes_while_the_steps_retention_runs() {
    use crate::common::queue::{beat, beats, heart, until_committed};
    use dagq::infrastructure::runtime_store::ROWS_PER_TRANSACTION as BATCH;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db).unwrap();
    let target = run_target();
    let now_ms = NOW * 1000;
    let old = now_ms - RETENTION_MS - DAY_MS;
    let total = BATCH * 4 + 10;
    queue.register_session(&target, "s", old).unwrap();
    queue
        .record_steps(
            &target,
            "s",
            &steps(&upto(i64::try_from(total).unwrap())),
            old,
        )
        .unwrap();
    queue
        .record_resources(&target, "s", &[ResourceSample::new(1, 0, 1)], old)
        .unwrap();
    beats(&db);
    let rows = "SELECT count(*) FROM session_steps";
    let heart = heart(&db);
    let job = {
        let db = db.clone();
        std::thread::spawn(move || {
            SqliteQueue::open(&db)
                .unwrap()
                .remove_steps(retained_since(now_ms))
        })
    };
    let all = i64::try_from(total).unwrap();
    until_committed(&db, rows, Some(all));
    let (written, took, seen) = beat(&heart, &format!("SELECT ({rows})"));
    let _waiting = crate::common::within(crate::common::STEP_LIMIT, "the steps' retention");
    assert_eq!(job.join().unwrap().unwrap(), total);
    assert!(written, "the heartbeat was not written in {took:?}");
    assert!(took < dagq::application::QUEUE_BUSY_TIMEOUT, "{took:?}");
    assert!(0 < seen && seen < all, "{seen} of {all}");
    assert_eq!(count(&db, rows), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM session_step_removed"), 0);
    assert_eq!(queue.target_resources(&target).unwrap(), vec![]);
    assert_eq!(queue.session_owner("s").unwrap(), None);
}

/// Every transaction of the retention writes at most so many rows, the
/// range rows it writes and the ranges it removes among them: steps of
/// seqs apart take a range row each, so a transaction removes half as many
/// of them. A session that keeps no step loses its last steps in the
/// transaction whose one row voids all its ranges, which then go a bounded
/// number a transaction, the first of them with it. A retention stopped among them leaves the
/// session read as removed whole: a step another connection adds then
/// starts it anew (a seq removed before is a new step, the seqs below are
/// gaps), and the next retention removes the void ranges and keeps it.
#[test]
fn the_steps_retention_bounds_each_transaction_and_voids_a_sessions_ranges_at_once() {
    use dagq::infrastructure::runtime_store::ROWS_PER_TRANSACTION as BATCH;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db)
        .unwrap()
        .with_write_hold_warn(std::time::Duration::ZERO);
    let target = run_target();
    let now_ms = NOW * 1000;
    let old = now_ms - RETENTION_MS - DAY_MS;
    let before = retained_since(now_ms);
    let half = BATCH / 2;
    let odd: Vec<i64> = (0..i64::try_from(half * 3).unwrap())
        .map(|index| index * 2 + 1)
        .collect();
    for session in ["s", "t"] {
        queue.register_session(&target, session, old).unwrap();
        queue
            .record_steps(&target, session, &steps(&odd), old)
            .unwrap();
    }
    let (written, _watching) = retention_rows();
    let conn = rusqlite::Connection::open(&db).unwrap();
    // Stops the first transaction that removes a step past the first ones
    // it fits.
    conn.execute_batch(&format!(
        "CREATE TRIGGER stop BEFORE DELETE ON session_steps
         WHEN old.session_id = 's' AND old.seq = {}
         BEGIN SELECT RAISE(ABORT, 'stopped'); END",
        odd[half]
    ))
    .unwrap();
    assert!(queue.remove_steps(before).is_err());
    assert_eq!(removed_seqs(&db, "s"), odd[..half]);
    let evens: Vec<SeqRange> = (1..i64::try_from(half * 3).unwrap())
        .map(|index| SeqRange {
            from: index * 2,
            to: index * 2,
        })
        .collect();
    assert_eq!(queue.session_gaps("s").unwrap(), evens);
    conn.execute_batch("DROP TRIGGER stop").unwrap();
    // Stops the removal of the void ranges once half of them would be gone.
    conn.execute_batch(&format!(
        "CREATE TRIGGER stop BEFORE DELETE ON session_step_removed
         WHEN (SELECT count(*) FROM session_step_removed WHERE from_seq > 0) <= {half}
         BEGIN SELECT RAISE(ABORT, 'stopped'); END"
    ))
    .unwrap();
    assert!(queue.remove_steps(before).is_err());
    let count_of = |session: &str| {
        count(
            &db,
            &format!("SELECT count(*) FROM session_steps WHERE session_id='{session}'"),
        )
    };
    assert_eq!(count_of("s"), 0);
    // Ranges are left, void: the session reads as removed whole. The row
    // that voids them holds no seq, so a binary that does not know it
    // reads only the ranges left.
    assert!(
        count(
            &db,
            "SELECT count(*) FROM session_step_removed WHERE from_seq > 0"
        ) > 0
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM session_step_removed WHERE from_seq < 1 AND to_seq < from_seq"
        ),
        1
    );
    assert_eq!(queue.session_gaps("s").unwrap(), vec![]);
    assert!(queue.session_owner("s").unwrap().is_some());

    let other = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        other
            .record_steps(&target, "s", &steps(&[4, odd[0]]), now_ms)
            .unwrap(),
        StepsRecorded {
            added: 2,
            duplicates: 0
        }
    );
    conn.execute_batch("DROP TRIGGER stop").unwrap();
    assert_eq!(queue.remove_steps(before).unwrap(), half * 3);
    // The session started anew keeps its new steps and its owner, and no
    // seq of before.
    assert_eq!(count(&db, "SELECT count(*) FROM session_step_removed"), 0);
    let kept = queue.target_steps(&target).unwrap();
    assert_eq!(seqs(&kept), [4, odd[0]].map(|seq| ("s".to_owned(), seq)));
    assert_eq!(
        queue.session_gaps("s").unwrap(),
        vec![SeqRange { from: 2, to: 3 }]
    );
    assert!(queue.session_owner("s").unwrap().is_some());
    // The other went whole, its ranges more than a transaction holds.
    assert_eq!(count_of("t"), 0);
    assert_eq!(queue.session_owner("t").unwrap(), None);
    // Nothing more is due.
    assert_eq!(queue.remove_steps(before).unwrap(), 0);
    let rows = written.rows();
    assert!(rows.len() > 6, "{rows:?}");
    assert!(
        rows.iter()
            .all(|&rows| rows <= u64::try_from(BATCH).unwrap()),
        "{rows:?}"
    );
}

/// A transaction that removes as many samples as it has rows left does not
/// know whether more are due: the owner of a session of as many old
/// samples as a transaction holds goes in the next.
#[test]
fn the_steps_retention_leaves_an_owner_past_a_full_transaction_to_the_next() {
    use dagq::infrastructure::runtime_store::ROWS_PER_TRANSACTION as BATCH;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db).unwrap();
    let target = run_target();
    let now_ms = NOW * 1000;
    let old = now_ms - RETENTION_MS - DAY_MS;
    let before = retained_since(now_ms);
    queue.register_session(&target, "s", old).unwrap();
    let samples: Vec<ResourceSample> = (1..=i64::try_from(BATCH).unwrap())
        .map(|seq| ResourceSample::new(seq, 0, 1))
        .collect();
    queue.record_resources(&target, "s", &samples, old).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER stop BEFORE DELETE ON session_owners
         BEGIN SELECT RAISE(ABORT, 'stopped'); END",
    )
    .unwrap();
    assert!(queue.remove_steps(before).is_err());
    // The samples' transaction committed without the owner.
    assert_eq!(count(&db, "SELECT count(*) FROM session_resources"), 0);
    assert!(queue.session_owner("s").unwrap().is_some());
    conn.execute_batch("DROP TRIGGER stop").unwrap();
    assert_eq!(queue.remove_steps(before).unwrap(), 0);
    assert_eq!(queue.session_owner("s").unwrap(), None);
}

/// A session removed whole in a transaction another session began loses
/// its last steps there with the row that voids its ranges, and as many of
/// them as are left of that transaction; the rest go in the next, and the
/// first commits within the bound.
#[test]
fn a_session_removed_whole_goes_on_from_what_is_left_of_a_transaction() {
    use dagq::infrastructure::runtime_store::ROWS_PER_TRANSACTION as BATCH;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db)
        .unwrap()
        .with_write_hold_warn(std::time::Duration::ZERO);
    let target = run_target();
    let now_ms = NOW * 1000;
    let older = now_ms - RETENTION_MS - 3 * DAY_MS;
    let old = now_ms - RETENTION_MS - DAY_MS;
    let odd: Vec<i64> = (0..20).map(|index| index * 2 + 1).collect();
    queue
        .record_steps(&target, "b", &steps(&odd), older)
        .unwrap();
    queue
        .record_steps(&target, "b", &steps(&(41..=50).collect::<Vec<_>>()), old)
        .unwrap();
    // An earlier retention left "b" 20 ranges and 10 steps.
    assert_eq!(queue.remove_steps(older + 1).unwrap(), 20);
    assert_eq!(removed_seqs(&db, "b"), odd);
    // "a" leaves 25 rows of the transaction: the 10 steps, the row that
    // voids the ranges and 14 of them.
    let samples: Vec<ResourceSample> = (1..=i64::try_from(BATCH).unwrap() - 25)
        .map(|seq| ResourceSample::new(seq, 0, 1))
        .collect();
    queue.record_resources(&target, "a", &samples, old).unwrap();
    let (written, _watching) = retention_rows();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER stop BEFORE DELETE ON session_step_removed
         WHEN (SELECT count(*) FROM session_step_removed WHERE from_seq > 0) <= 6
         BEGIN SELECT RAISE(ABORT, 'stopped'); END",
    )
    .unwrap();
    let before = retained_since(now_ms);
    assert!(queue.remove_steps(before).is_err());
    assert_eq!(count(&db, "SELECT count(*) FROM session_resources"), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM session_steps"), 0);
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM session_step_removed WHERE from_seq > 0"
        ),
        6
    );
    assert_eq!(queue.session_gaps("b").unwrap(), vec![]);
    assert_eq!(written.rows()[0], u64::try_from(BATCH).unwrap());
    conn.execute_batch("DROP TRIGGER stop").unwrap();
    assert_eq!(queue.remove_steps(before).unwrap(), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM session_step_removed"), 0);
    assert!(
        written
            .rows()
            .iter()
            .all(|&rows| rows <= u64::try_from(BATCH).unwrap())
    );
}

/// A batch of the retention reads its session's rows under its lock by
/// the searches the migrations leave on queue.db, each finding a row by
/// its key or walking only rows it takes or stops at, and the walk for its
/// candidates with no lock goes by the session's index in its order: no
/// walk of a table and no sort of the session's rows.
#[test]
fn a_retention_batch_reads_its_sessions_rows_in_the_order_of_their_index() {
    use dagq::infrastructure::runtime_store::{RETENTION_BATCH_READS, RETENTION_CANDIDATE_READS};
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    SqliteQueue::init(&db).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    for (sql, search) in RETENTION_BATCH_READS
        .iter()
        .chain(&RETENTION_CANDIDATE_READS)
    {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
        let steps: Vec<String> = statement
            .query_map(rusqlite::params_from_iter(nulls), |r| r.get("detail"))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            steps
                .iter()
                .any(|line| line.starts_with("SEARCH") && line.ends_with(search)),
            "{sql}: {steps:#?}"
        );
        assert!(
            !steps
                .iter()
                .any(|line| line.contains("TEMP B-TREE") || line.starts_with("SCAN")),
            "{sql}: {steps:#?}"
        );
    }
}

/// A session that keeps many rows at its first seqs, with old ones among
/// them and after them: the retention removes the old ones and keeps the
/// rest, every transaction within the bound, though the rows the session
/// keeps are many times more. Stopped part way, a step another connection
/// adds before the next is kept and its seq not counted as removed.
#[test]
fn the_steps_retention_removes_the_old_rows_after_many_a_session_keeps() {
    use dagq::infrastructure::runtime_store::ROWS_PER_TRANSACTION as BATCH;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db)
        .unwrap()
        .with_write_hold_warn(std::time::Duration::ZERO);
    let target = run_target();
    let now_ms = NOW * 1000;
    let old = now_ms - RETENTION_MS - DAY_MS;
    let new = now_ms - DAY_MS;
    let before = retained_since(now_ms);
    let kept_up_to = 10_000;
    let (due, kept): (Vec<i64>, Vec<i64>) =
        (1..=kept_up_to + 300).partition(|seq| *seq > kept_up_to || seq % 1_000 == 0);
    queue.register_session(&target, "s", old).unwrap();
    queue
        .record_steps(&target, "s", &steps(&kept), new)
        .unwrap();
    queue.record_steps(&target, "s", &steps(&due), old).unwrap();
    let samples = |seqs: std::ops::RangeInclusive<i64>| -> Vec<ResourceSample> {
        seqs.map(|seq| ResourceSample::new(seq, 0, 1)).collect()
    };
    queue
        .record_resources(&target, "s", &samples(1..=kept_up_to), new)
        .unwrap();
    queue
        .record_resources(
            &target,
            "s",
            &samples(kept_up_to + 1..=kept_up_to + 50),
            old,
        )
        .unwrap();
    let (written, _watching) = retention_rows();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(&format!(
        "CREATE TRIGGER stop BEFORE DELETE ON session_steps
         WHEN old.session_id = 's' AND old.seq = {}
         BEGIN SELECT RAISE(ABORT, 'stopped'); END",
        kept_up_to + 250
    ))
    .unwrap();
    assert!(queue.remove_steps(before).is_err());
    let other = SqliteQueue::open(&db).unwrap();
    other
        .record_steps(&target, "s", &steps(&[kept_up_to + 400]), now_ms)
        .unwrap();
    conn.execute_batch("DROP TRIGGER stop").unwrap();
    let first = due.len() - removed_seqs(&db, "s").len();
    assert_eq!(queue.remove_steps(before).unwrap(), first);
    assert_eq!(removed_seqs(&db, "s"), due);
    let mut left = kept.clone();
    left.push(kept_up_to + 400);
    assert_eq!(
        seqs(&queue.target_steps(&target).unwrap()),
        left.iter()
            .map(|&seq| ("s".to_owned(), seq))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        queue.session_gaps("s").unwrap(),
        vec![SeqRange {
            from: kept_up_to + 301,
            to: kept_up_to + 399
        }]
    );
    assert_eq!(
        queue.target_resources(&target).unwrap().len(),
        usize::try_from(kept_up_to).unwrap()
    );
    assert!(queue.session_owner("s").unwrap().is_some());
    let rows = written.rows();
    assert!(rows.len() > 2, "{rows:?}");
    assert!(
        rows.iter()
            .all(|&rows| rows <= u64::try_from(BATCH).unwrap()),
        "{rows:?}"
    );
}
