use serde_json::json;

use super::*;
use crate::domain::host_metrics::{HOST_NODE, HostSample, SampleSource};
use crate::domain::ledger::testkit::{Log, ms, run_id};

const BUILT: &str = "2030-01-01T00:00:00.000Z";

fn inputs(log: &Log, now: i64) -> Inputs {
    Inputs::new(&log.events, ms(now), BUILT, true)
}

fn sample(id: i64, unix: i64) -> NodeSample {
    NodeSample {
        id,
        node_id: HOST_NODE.to_owned(),
        local_day: unix.div_euclid(86_400),
        source: SampleSource::Sampled,
        sample: HostSample::new(unix),
    }
}

/// An event marks its run and its task, a session's or the queue's event
/// the session and queue rows, and a push the runs left pending on theirs;
/// the cursor moves past what was read.
#[test]
fn an_event_marks_what_it_is_an_input_of() {
    let (a, b) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910);
    let mut builder = Builder::default();
    builder.mark_events(&log.events);
    assert_eq!(builder.events, log.last());
    assert_eq!(
        builder.take(10),
        [Target::Run(a.clone()), Target::Task(TaskId::new(1))]
    );
    assert!(!builder.rows_marked());
    let row = build(&Target::Run(a.clone()), &inputs(&log, 1000), &[]).unwrap();
    builder.track(&Target::Run(a.clone()), &row);

    let before = log.last();
    log.landed(2, &b, 2000).add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s"}),
        3000,
    );
    builder.mark_events(&log.after(before));
    assert!(builder.rows_marked());
    let marks = builder.take(10);
    // Run a too: b's push may close its pending one.
    for target in [
        Target::Run(a.clone()),
        Target::Run(b),
        Target::Task(TaskId::new(1)),
        Target::Task(TaskId::new(2)),
    ] {
        assert!(marks.contains(&target), "{target:?} in {marks:?}");
    }
}

/// A run on its way is not written; one ended, one pending on its push and
/// a legacy one are, and only a legacy one or one ended past its grace is
/// settled.
#[test]
fn only_an_ended_run_is_written_and_settled_after_its_grace() {
    let (a, b, c, d) = (run_id(1), run_id(2), run_id(3), run_id(4));
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .landed(2, &b, 0)
        .worked(3, &c, 0)
        .integrated(3, &c, 700)
        .push_failed(3, &c, 910)
        .legacy(4, &d, 0);
    let now = 930 + SETTLE_GRACE_MS / 1000;
    assert_eq!(build(&Target::Run(a), &inputs(&log, now), &[]), None);
    assert_eq!(
        build(&Target::Task(TaskId::new(1)), &inputs(&log, now), &[]),
        None
    );

    let early = build(&Target::Run(b.clone()), &inputs(&log, now - 1), &[]).unwrap();
    assert_eq!(early.ended_at, Some(ms(930)));
    assert!(!early.is_final);
    let settled = build(&Target::Run(b), &inputs(&log, now), &[]).unwrap();
    assert!(settled.is_final);
    let task = build(&Target::Task(TaskId::new(2)), &inputs(&log, now), &[]).unwrap();
    assert!(task.is_final, "{task:?}");

    let pending = build(&Target::Run(c), &inputs(&log, now * 10), &[]).unwrap();
    assert!(pending_push(&pending));
    assert!(!pending.is_final);
    let pending_task = build(&Target::Task(TaskId::new(3)), &inputs(&log, now * 10), &[]);
    assert!(!pending_task.unwrap().is_final);

    let legacy = build(&Target::Run(d), &inputs(&log, 1300), &[]).unwrap();
    assert!(legacy.is_legacy() && legacy.is_final);
}

/// The same inputs fold to the same row, which is not written again; a
/// later input, an older fold, settling and a missing row are written; a
/// legacy row's version is the store's and does not write it again.
#[test]
fn a_row_is_written_only_for_what_changed() {
    let b = run_id(2);
    let mut log = Log::default();
    log.landed(2, &b, 0);
    let target = Target::Run(b.clone());
    let row = build(&target, &inputs(&log, 1000), &[]).unwrap();
    assert!(writes(None, &row));
    let again = build(&target, &inputs(&log, 1000), &[]).unwrap();
    assert_eq!(again, row);
    assert!(!writes(Some(&row), &again));

    let mut old = row.clone();
    old.ledger_v = LEDGER_V - 1;
    assert!(writes(Some(&old), &again));

    let settled = build(&target, &inputs(&log, 930 + SETTLE_GRACE_MS / 1000), &[]).unwrap();
    assert!(writes(Some(&row), &settled));
    assert!(!writes(Some(&settled), &settled));

    // A later event of the run reaches a settled row: dirty.
    log.add(Some(2), Some(&b), "review_finished", json!({}), 5000);
    let later = build(&target, &inputs(&log, 6000), &[]).unwrap();
    assert!(writes(Some(&settled), &later));

    let mut legacy_log = Log::default();
    legacy_log.legacy(4, &run_id(4), 0);
    let legacy_target = Target::Run(run_id(4));
    let mut legacy = build(&legacy_target, &inputs(&legacy_log, 2000), &[]).unwrap();
    let fresh = legacy.clone();
    legacy.ledger_v = LEDGER_V - 1;
    assert!(!writes(Some(&legacy), &fresh));
    legacy_log.add(
        Some(4),
        Some(&run_id(4)),
        "review_finished",
        json!({}),
        3000,
    );
    let later = build(&legacy_target, &inputs(&legacy_log, 4000), &[]).unwrap();
    assert!(writes(Some(&legacy), &later));
}

/// Another run's push closes a pending push: the pending run's fold reads
/// it, past what its row held, and is written again ended.
#[test]
fn another_runs_push_makes_a_pending_row_dirty() {
    let (a, b) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910);
    let target = Target::Run(a);
    let pending = build(&target, &inputs(&log, 1000), &[]).unwrap();
    log.landed(2, &b, 2000);
    let closed = build(&target, &inputs(&log, 4000), &[]).unwrap();
    assert!(closed.ended_at.is_some());
    assert!(writes(Some(&pending), &closed));
}

/// A row ended and not settled is marked again once its grace is over,
/// not before; a settled one is not waited on.
#[test]
fn an_unsettled_row_is_marked_again_when_its_grace_is_over() {
    let b = run_id(2);
    let mut log = Log::default();
    log.landed(2, &b, 0);
    let target = Target::Run(b);
    let row = build(&target, &inputs(&log, 1000), &[]).unwrap();
    let mut builder = Builder::default();
    builder.track(&target, &row);
    builder.due(ms(930) + SETTLE_GRACE_MS - 1);
    assert!(!builder.marked());
    builder.due(ms(930) + SETTLE_GRACE_MS);
    assert_eq!(builder.take(10), std::slice::from_ref(&target));
    let mut settled = row;
    settled.is_final = true;
    builder.track(&target, &settled);
    builder.due(i64::MAX);
    assert!(!builder.marked());
}

/// The session and queue rows are looked at only for those that read an
/// input after the event their marks started at.
#[test]
fn only_the_session_and_queue_rows_with_a_new_input_are_marked() {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s1"}),
        0,
    )
    .add(None, None, "claim_held", json!({"supervisor": "s1"}), 10);
    let mut builder = Builder::default();
    builder.mark_events(&log.events);
    let all = inputs(&log, 100).rows;
    assert!(!all.is_empty());
    builder.mark_rows(&all);
    assert_eq!(builder.backlog(), all.len());
    builder.take(usize::MAX);

    let before = log.last();
    log.add(None, None, "claim_held", json!({"supervisor": "s2"}), 50);
    builder.mark_events(&log.after(before));
    let rows = inputs(&log, 100).rows;
    builder.mark_rows(&rows);
    let marked = builder.take(usize::MAX);
    assert!(!marked.is_empty());
    assert!(marked.len() < rows.len(), "{marked:?}");
    for target in &marked {
        let (kind, key) = target.row_key();
        assert!(rows[&(kind, key)].inputs_through.last() > before);
    }
}

/// A sample marks its node's hour; the hour's row is built of its samples
/// and settled once the hour and its grace passed.
#[test]
fn a_sample_marks_its_hour_which_settles_after_it() {
    let start = node::bucket_of(1_900_000_000);
    let samples = [sample(1, start + 10), sample(2, start + 70)];
    let mut builder = Builder::default();
    builder.mark_samples(&samples);
    assert_eq!(builder.samples, 2);
    let target = Target::Node(HOST_NODE.to_owned(), start);
    assert_eq!(builder.take(10), std::slice::from_ref(&target));
    let open = Inputs::new(&[], (start + 100) * 1000, BUILT, false);
    let row = build(&target, &open, &samples).unwrap();
    assert_eq!(row.inputs_through.node_samples, Some(2));
    assert!(!row.is_final);
    let late = Inputs::new(
        &[],
        (start + BUCKET_SECS) * 1000 + SETTLE_GRACE_MS,
        BUILT,
        false,
    );
    assert!(build(&target, &late, &samples).unwrap().is_final);
    let more = [
        samples[0].clone(),
        samples[1].clone(),
        sample(3, start + 90),
    ];
    let again = build(&target, &late, &more).unwrap();
    assert!(writes(Some(&row), &again));
    assert_eq!(build(&target, &late, &[]), None);
}
