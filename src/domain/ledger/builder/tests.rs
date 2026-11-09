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
        resources: Default::default(),
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
    // The run's landing moves the landing slot's row, a queue row.
    assert!(builder.rows_marked());
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

/// The takes go round the marks: one starts at the first mark the last
/// did not take, or at the first one put back, and goes on to those
/// before it, marked again meanwhile.
#[test]
fn a_take_starts_where_the_last_pass_left_off_and_goes_round() {
    let task = |id| Target::Task(TaskId::new(id));
    let mut builder = Builder::default();
    builder.marks.extend((1..=5).map(task));
    assert_eq!(builder.take(2), [task(1), task(2)]);
    builder.keep(task(1));
    assert_eq!(builder.take(2), [task(3), task(4)]);
    builder.put_back(task(4), []);
    builder.keep(task(3));
    assert_eq!(builder.take(3), [task(4), task(5), task(1)]);
    builder.keep(task(5));
    assert_eq!(builder.take(10), [task(3), task(5)]);
    assert_eq!(builder.backlog(), 0);
    builder.keep(task(2));
    assert_eq!(builder.take(10), [task(2)]);
}

/// The marks holding R's events back are those of a run, task, queue or
/// node row with a position of the events, the earliest as many as asked
/// in the marks' order; a session's, one taken already and one with no position are
/// not, and naming them changes nothing.
#[test]
fn the_marks_holding_r_are_named_earliest_first() {
    let mut log = Log::default();
    for task in [3, 1, 2] {
        log.add(Some(task), None, "task_created", json!({}), task);
    }
    let mut builder = Builder::default();
    for event in &log.events {
        builder.mark_events(std::slice::from_ref(event));
    }
    let session = Target::Row(LedgerKind::Session, "s".to_owned());
    let at = builder.read_to();
    builder.marks.insert(session.clone());
    builder.dirtied.insert(
        session,
        Dirtied::events(ReadTo {
            event: EventId::new(0),
            ..at
        }),
    );
    let queue = Target::Row(LedgerKind::Queue, "q".to_owned());
    builder.marks.insert(queue);
    let before = builder.clone();
    let task = |id| Target::Task(TaskId::new(id));
    assert_eq!(builder.holding_r(10), [task(1), task(2), task(3)]);
    assert_eq!(builder.holding_r(2), [task(1), task(3)]);
    assert_eq!(builder.holding_r(0), []);
    assert_eq!(builder, before);
    // One taken and not looked at yet is not a mark left.
    builder.marks.remove(&task(3));
    assert_eq!(builder.holding_r(10), [task(1), task(2)]);
}

/// A take takes first the mark holding R, whatever rank the round goes on
/// from; kept without its row recording its input it holds R and is taken
/// first again, and once tracked R moves past it. The round's rank counts
/// only the round, a put back too.
#[test]
fn a_take_takes_the_mark_holding_r_first_whatever_the_rank() {
    let mut log = Log::default();
    log.add(Some(1), None, "task_created", json!({}), 1);
    let mut builder = Builder::default();
    let queue = |id: usize| Target::Row(LedgerKind::Queue, format!("q{id:02}"));
    builder.marks.extend((0..20).map(queue));
    let before = builder.events;
    builder.mark_events(&log.events);
    builder.resume_taking(10);
    let task = Target::Task(TaskId::new(1));
    // The round's rank 10 is of every mark, the task's (0) too.
    let taken = builder.take(3);
    assert_eq!(taken, [task.clone(), queue(9), queue(10)]);
    assert_eq!(builder.take_offset, 12);
    // Not looked at: it holds R, and is taken first again.
    builder.keep(task.clone());
    builder.keep(queue(9));
    builder.keep(queue(10));
    assert_eq!(builder.rows_written().unwrap().events, before);
    assert_eq!(builder.take(3), [task.clone(), queue(11), queue(12)]);
    builder.put_back(queue(11), [queue(12)]);
    assert_eq!(builder.take_offset, 12);
    builder.keep(task.clone());
    assert_eq!(builder.take(2), [task.clone(), queue(11)]);
    let row = build(&task, &inputs(&log, 2), &[]).unwrap();
    builder.track(&task, &row);
    builder.keep(task);
    assert_eq!(builder.rows_written().unwrap().events, log.last());
    assert_eq!(builder.take(2), [queue(12), queue(13)]);
}

/// Besides those of the events, the marks holding R are the one of the
/// earliest position of each other cursor (a run's steps, its windows
/// from outside or inside, a node's samples), the events' first when
/// fewer are asked, the earliest events after them; one whose write
/// failed or a session's is not. A take takes them first whatever the
/// round's rank; kept without its row recording its input such a mark
/// holds its cursor of R, and once tracked R moves past it.
#[test]
fn the_oldest_mark_of_each_cursor_holds_r_and_is_taken_first() {
    let mut log = Log::default();
    for task in [1, 2] {
        log.add(Some(task), None, "task_created", json!({}), task);
    }
    let start = node::bucket_of(1_900_000_000);
    let mut builder = Builder::default();
    let queue = |id: usize| Target::Row(LedgerKind::Queue, format!("q{id:02}"));
    builder.marks.extend((0..20).map(queue));
    builder.mark_samples(&[sample(1, start + 10)]);
    builder.mark_events(&log.events);
    let node = Target::Node(HOST_NODE.to_owned(), start);
    let run = |n| Target::Run(run_id(n));
    let task = |id| Target::Task(TaskId::new(id));
    for (target, dirtied) in [
        (
            run(1),
            Dirtied {
                steps: Some(5),
                ..Dirtied::default()
            },
        ),
        (
            run(2),
            Dirtied {
                steps: Some(3),
                ..Dirtied::default()
            },
        ),
        (
            run(3),
            Dirtied {
                resources: Some(1),
                ..Dirtied::default()
            },
        ),
        (
            run(4),
            Dirtied {
                inside_resources: Some(2),
                ..Dirtied::default()
            },
        ),
        (
            run(5),
            Dirtied {
                inside_resources: Some(9),
                ..Dirtied::default()
            },
        ),
        (
            run(6),
            Dirtied {
                steps: Some(0),
                ..Dirtied::default()
            },
        ),
        (
            Target::Row(LedgerKind::Session, "s".to_owned()),
            Dirtied {
                resources: Some(0),
                ..Dirtied::default()
            },
        ),
    ] {
        builder.marks.insert(target.clone());
        builder.dirtied.insert(target, dirtied);
    }
    builder.failed.insert(run(6));
    let before = builder.clone();
    assert_eq!(
        builder.holding_r(10),
        [run(2), run(3), run(4), task(1), task(2), node.clone()]
    );
    // Fewer asked: the events' earliest first, then the cursors in turn.
    assert_eq!(builder.holding_r(3), [run(2), task(1), node.clone()]);
    assert_eq!(builder.holding_r(1), [task(1)]);
    assert_eq!(builder, before);

    // With no event marked, the other cursors' are taken first anyway,
    // whatever the round's rank.
    let mut builder = Builder::default();
    builder.marks.extend((0..20).map(queue));
    builder.mark_samples(&[sample(1, start + 10)]);
    let at = builder.rows_written().unwrap();
    builder.mark_samples(&[sample(2, start + 20)]);
    builder.resume_taking(5);
    assert_eq!(builder.take(3), [node.clone(), queue(5), queue(6)]);
    builder.keep(node.clone());
    builder.keep(queue(5));
    builder.keep(queue(6));
    assert_eq!(builder.rows_written().unwrap().samples, at.samples);
    assert_eq!(builder.take(2)[0], node);
    let samples = [sample(1, start + 10), sample(2, start + 20)];
    let open = Inputs::new(&[], (start + 100) * 1000, BUILT, false);
    let row = build(&node, &open, &samples).unwrap();
    builder.track(&node, &row);
    builder.keep(node);
    assert_eq!(builder.rows_written().unwrap().samples, 2);
}

/// A run on its way is not written; one ended, one pending on its push and
/// a legacy one are, and only a legacy one or one ended past its grace is
/// settled. A task on its way is written, not settled.
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
    let on_its_way = build(&Target::Task(TaskId::new(1)), &inputs(&log, now), &[]).unwrap();
    assert!(on_its_way.ended_at.is_none() && !on_its_way.is_final);

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

/// A history of `n` of each kind the folds of the queue as a whole read,
/// each ended before what comes after it: runs landed (their attempts at
/// the landing slot and their pushes), slots full, holds and deferrals,
/// streams of supervisors that stopped, sessions and events of the queue
/// of their own; but for slots full whose supervisor lives past
/// [`LAST_PASS`] and falls silent before the next.
fn history(n: i64) -> Log {
    let mut log = Log::default();
    for k in 0..n {
        let at = k * 10;
        let supervisor = format!("h{k}");
        let task = 100 + k;
        log.landed(task, &format!("{k:08x}-2222-4222-8222-222222222222"), at)
            .add(
                None,
                None,
                "slots_full_started",
                json!({"supervisor": supervisor, "class": "heavy"}),
                at,
            )
            .add(
                None,
                None,
                "slots_full_ended",
                json!({"supervisor": supervisor, "class": "heavy"}),
                at + 1,
            )
            .add(None, None, "claim_held", json!({"supervisor": "h"}), at + 2)
            .add(
                None,
                None,
                "claim_resumed",
                json!({"supervisor": "h"}),
                at + 3,
            )
            .add(
                Some(task),
                None,
                "claim_deferred",
                json!({"supervisor": "h", "reason": "paths"}),
                at + 4,
            )
            .add(
                Some(task),
                None,
                "claim_deferral_ended",
                json!({"supervisor": "h", "why": "free"}),
                at + 5,
            )
            .add(
                None,
                None,
                "live_alert_baseline",
                json!({"supervisor": supervisor, "version": 1}),
                at + 6,
            )
            .add(
                None,
                None,
                "supervisor_stopped",
                json!({"supervisor": supervisor}),
                at + 7,
            )
            .add(
                None,
                None,
                "session_opened",
                json!({"kind": "runtime_planner", "planner_id": 1000 + k}),
                at + 8,
            )
            .add(
                None,
                None,
                "session_closed",
                json!({"kind": "runtime_planner", "planner_id": 1000 + k}),
                at + 9,
            )
            .add(None, None, "mark_added", json!({"mark": k}), at + 9)
            // Slots full no end of its own ends: its supervisor stops, or
            // falls silent.
            .add(
                None,
                None,
                "slots_full_started",
                json!({"supervisor": format!("g{k}"), "class": "light"}),
                at + 9,
            )
            .add(
                None,
                None,
                "supervisor_stopped",
                json!({"supervisor": format!("g{k}")}),
                at + 9,
            )
            .add(
                None,
                None,
                "slots_full_started",
                json!({"supervisor": format!("q{k}"), "class": "heavy"}),
                at + 9,
            );
    }
    for k in 0..n {
        log.add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": format!("q{k}")}),
            LAST_PASS - 100,
        );
    }
    log
}

/// When the pass before the one [`one_pass`] counts ran.
const LAST_PASS: i64 = 999_100;

/// What [`one_pass`] counts a pass made, in its order.
type Made = (usize, usize, usize, usize, usize, usize, usize);

/// What one pass makes and drops of a builder caught up on `log` when
/// the same new inputs come in: the rows it builds, the events it copies
/// into them, the landing slot's attempts among them, the waits before a
/// claim its due copies as it closes those whose supervisor's life ended
/// since the last pass, and the waits it makes for a task's fold (the
/// rest it reads where they are kept). Also what is kept, and how many
/// waits the due closed, to tell the histories apart.
fn one_pass(mut log: Log) -> (Made, usize, usize) {
    let now = 2_000_000;
    let mut builder = Builder::default();
    builder.mark_events(&log.events);
    builder.due(ms(LAST_PASS));
    let rows = builder.rows_to_build(builder.kept());
    let inputs = Inputs::of(builder.kept(), ms(LAST_PASS), BUILT, &rows);
    builder.mark_rows(&inputs.rows);
    builder.take(usize::MAX);
    drop(inputs);

    let before = log.last();
    let run = run_id(1);
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s"}),
        1_000_000,
    )
    .add(
        None,
        None,
        "slots_full_started",
        json!({"supervisor": "s", "class": "light"}),
        1_000_001,
    )
    .add(
        None,
        None,
        "claim_held",
        json!({"supervisor": "s"}),
        1_000_002,
    )
    .add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 1}),
        1_000_003,
    )
    .add(None, None, "mark_added", json!({"mark": "new"}), 1_000_004)
    .worked(1, &run, 1_000_005)
    .add(
        Some(1),
        Some(&run),
        "integration_started",
        json!({}),
        1_001_000,
    );
    builder.mark_events(&log.after(before));
    // The waits as the last pass read them: where each one's text is
    // kept, and whether it was open. One the due closes is moved, its text
    // where it was; a copy's is elsewhere, as both are at once.
    let before: std::collections::HashMap<EventId, (*const u8, bool)> = {
        let kept = builder.kept();
        kept.pre_claim
            .intervals(&kept.lives, ms(LAST_PASS))
            .filter_map(|wait| match wait {
                std::borrow::Cow::Borrowed(wait) => {
                    Some((wait.id, (wait.started_at.as_ptr(), wait.end.is_none())))
                }
                std::borrow::Cow::Owned(_) => None,
            })
            .collect()
    };
    builder.due(ms(now));
    let (mut copied_by_due, mut closed_by_due) = (0, 0);
    let kept = builder.kept();
    for wait in kept.pre_claim.intervals(&kept.lives, ms(now)) {
        match before.get(&wait.id) {
            Some(&(text, open)) if text == wait.started_at.as_ptr() => {
                if open && wait.end.is_some() {
                    closed_by_due += 1;
                }
            }
            _ => copied_by_due += 1,
        }
    }
    let rows = builder.rows_to_build(builder.kept());
    let mut inputs = Inputs::of(builder.kept(), ms(now), BUILT, &rows);
    builder.mark_rows(&inputs.rows);
    // The pushes and the waits before a claim are read where they are kept.
    assert!(Arc::ptr_eq(&inputs.kept, builder.kept()));
    let kept = Arc::clone(builder.kept());
    let task = Target::Task(TaskId::new(1));
    let events: Vec<RunEvent> = log
        .events
        .iter()
        .filter(|event| event.task_id == Some(TaskId::new(1)))
        .cloned()
        .collect();
    // A task on its way is written, not settled, and its fold reads the
    // waits: count those it is given made for it, not read where they are
    // kept.
    let (mut made_waits, mut read_waits) = (0, 0);
    let row = super::super::task_row(
        TaskId::new(1),
        &events,
        &kept.pushes,
        kept.pre_claim
            .intervals(&kept.lives, ms(now))
            .inspect(|wait| {
                read_waits += 1;
                if matches!(wait, std::borrow::Cow::Owned(_)) {
                    made_waits += 1;
                }
            }),
        ms(now),
        BUILT,
    );
    assert_eq!(row.task_id, Some(TaskId::new(1)));
    inputs.read(&task, events);
    let built = build(&task, &inputs, &[]).unwrap();
    assert!(built.ended_at.is_none() && !built.is_final);
    let copied = inputs.rows.values().map(|row| row.records.len()).sum();
    let attempts = inputs
        .rows
        .values()
        .filter(|row| {
            row.subject
                .as_ref()
                .is_some_and(|s| s.kind == "landing_slot")
        })
        .count();
    let marked = builder.take(usize::MAX).len();
    let kept_size = kept.queue.len() + kept.sessions.len() + kept.pushes.len() + read_waits;
    (
        (
            rows.len(),
            inputs.rows.len(),
            copied,
            attempts,
            marked,
            copied_by_due,
            made_waits,
        ),
        kept_size,
        closed_by_due,
    )
}

/// A pass at a time before the last pass's, as a clock set back gives it,
/// folds a slots full whose supervisor's silence the last pass found as
/// the fold of every event then: open again in its queue row and in the
/// waits of a task's, and ended again by the pass after.
#[test]
fn a_pass_at_a_clock_set_back_folds_the_waits_as_every_event_then() {
    use crate::domain::{HEARTBEAT_TIMEOUT_SECS, supervisor_life::SUPERVISOR_ALIVE_INTERVAL_SECS};
    let run = run_id(1);
    let mut log = Log::default();
    log.add(Some(1), None, "task_created", json!({}), 0)
        .add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s", "class": "heavy"}),
            10,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            20,
        )
        .landed(1, &run, 1000);
    // The last time s's life goes on.
    let bound = ms(20) + (SUPERVISOR_ALIVE_INTERVAL_SECS + HEARTBEAT_TIMEOUT_SECS) * 1000;
    let task = Target::Task(TaskId::new(1));
    let slots_full = Target::Row(LedgerKind::Queue, "2".to_owned());
    let whole = |now: i64| {
        let inputs = Inputs::new(&log.events, now, BUILT, true);
        (build(&task, &inputs, &[]), build(&slots_full, &inputs, &[]))
    };
    assert_ne!(whole(bound), whole(bound + 1));
    let mut builder = Builder::default();
    builder.mark_events(&log.events);
    for now in [bound + 1, bound, bound + 1, bound] {
        builder.due(now);
        let rows = BTreeSet::from([(LedgerKind::Queue, "2".to_owned())]);
        let mut inputs = Inputs::of(builder.kept(), now, BUILT, &rows);
        inputs.read(
            &task,
            log.events
                .iter()
                .filter(|event| event.task_id == Some(TaskId::new(1)))
                .cloned()
                .collect(),
        );
        assert_eq!(
            (build(&task, &inputs, &[]), build(&slots_full, &inputs, &[])),
            whole(now),
            "at {now}"
        );
    }
}

/// A pass folds the same new inputs into as many rows, copies as many
/// events, makes as many attempts, marks as many targets, and copies no
/// wait before a claim as its due closes those whose supervisor fell
/// silent since the last pass and makes none for a task's fold (slots
/// full its supervisor's stop or silence ended among them), however long
/// the history before them: nothing it makes grows with the events kept.
/// Besides, it marks and folds the queue row of each wait its due closes,
/// whose end that is (one record each).
#[test]
fn a_pass_makes_as_much_of_the_same_new_inputs_whatever_the_history() {
    let (few, kept_few, closed_few) = one_pass(history(3));
    let (many, kept_many, closed_many) = one_pass(history(300));
    assert!(kept_many > kept_few * 50, "{kept_few} {kept_many}");
    // The history's, and the new inputs' own.
    assert_eq!((closed_few, closed_many), (3 + 1, 300 + 1));
    let but_closed = |made: Made, closed: usize| {
        let (rows, built, copied, attempts, marked, copied_by_due, made_waits) = made;
        (
            rows - closed,
            built - closed,
            copied - closed,
            attempts,
            marked - closed,
            copied_by_due,
            made_waits,
        )
    };
    assert_eq!(but_closed(few, closed_few), but_closed(many, closed_many));
    let (rows, built, copied, attempts, _, copied_by_due, made_waits) = few;
    assert_eq!((copied_by_due, made_waits), (0, 0));
    assert_eq!(rows, built);
    assert!(built > 0 && copied > 0, "{few:?}");
    assert_eq!(attempts, 1);
}

/// A step marks its run only, a step of a target of no run nothing, and
/// the step cursor moves past both.
#[test]
fn a_step_marks_its_run_and_one_of_no_run_nothing() {
    use crate::domain::RunId;
    use crate::domain::session_step::{Step, StepKind, StepTarget, TargetKind};
    let a = run_id(1);
    let step = |id, target: StepTarget| SessionStep {
        id,
        target,
        session_id: "s".to_owned(),
        received_at: 0,
        step: Step::new(id, StepKind::Wait, 0, 1),
    };
    let planner = StepTarget {
        kind: TargetKind::Planner,
        id: "p".to_owned(),
    };
    let mut builder = Builder::default();
    builder.mark_steps(&[step(3, planner.clone())]);
    assert_eq!((builder.steps, builder.marked()), (3, false));
    builder.mark_steps(&[
        step(4, StepTarget::run(&RunId::new(&a).unwrap())),
        step(5, planner),
    ]);
    assert_eq!(builder.steps, 5);
    assert_eq!(builder.take(10), [Target::Run(a)]);
}

/// The token of a run is revoked when none was issued or the last issue
/// or revocation recorded is a revocation; a stored row a later step
/// reaches is written again, one that read the same steps is not.
#[test]
fn a_runs_token_is_revoked_after_its_last_issue_and_a_later_step_writes_its_row() {
    let a = run_id(1);
    let mut log = Log::default();
    log.worked(1, &a, 0);
    assert!(telemetry_revoked(&log.events));
    log.add(Some(1), Some(&a), "telemetry_token_issued", json!({}), 1);
    assert!(!telemetry_revoked(&log.events));
    log.add(Some(1), Some(&a), "telemetry_token_revoked", json!({}), 2);
    assert!(telemetry_revoked(&log.events));
    log.add(Some(1), Some(&a), "telemetry_token_issued", json!({}), 3)
        .add(Some(1), Some(&a), "review_finished", json!({}), 4);
    assert!(!telemetry_revoked(&log.events));

    log.integrated(1, &a, 700).pushed(1, &a, 930);
    let mut inputs = inputs(&log, 1000);
    let stored = build(&Target::Run(a.clone()), &inputs, &[]).unwrap();
    assert_eq!(stored.inputs_through.steps, None);
    let mut later = stored.clone();
    later.inputs_through.steps = Some(7);
    assert!(writes(Some(&stored), &later));
    assert!(!writes(Some(&later), &later));
    assert!(!writes(Some(&later), &stored));
    // A run's steps read into its inputs fold into its row, which goes on
    // from what the stored row read; forgotten with its events once folded.
    let target = Target::Run(a.clone());
    inputs.read_steps(&a, RunSteps::new(Vec::new(), Some(&later)));
    assert_eq!(
        build(&target, &inputs, &[]).unwrap().inputs_through.steps,
        Some(7)
    );
    inputs.forget(&target);
    assert_eq!(build(&target, &inputs, &[]), None);
}

/// A task's fold over the events the stored row read that lacks a wait
/// the stored row read ended by its supervisor's silence was folded at an
/// earlier time: it would write (its waits differ) but is behind, and the
/// fold past the silence is not behind the earlier one. A fold that read a
/// later event of the task is not behind either.
#[test]
fn a_task_fold_before_a_silence_over_the_same_events_is_behind_the_one_past_it() {
    use crate::domain::supervisor_life::SILENT_AFTER_MS;
    let task_row = |log: &Log, secs: i64| {
        super::super::build(&log.events, ms(secs), BUILT)
            .into_iter()
            .find(|row| row.kind == LedgerKind::Task)
            .unwrap()
    };
    let mut log = Log::default();
    log.add(Some(1), None, "task_created", json!({"status": "draft"}), 0)
        .add(
            Some(1),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "ready"}),
            5,
        )
        .add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s", "class": "heavy"}),
            10,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            20,
        );
    let bound = 20 + SILENT_AFTER_MS / 1000;
    let (before, past) = (task_row(&log, bound), task_row(&log, bound + 1));
    assert_eq!(before.inputs_through.events, past.inputs_through.events);
    assert!(before.inputs_through.life_ended.is_empty());
    assert!(!past.inputs_through.life_ended.is_empty());
    assert!(writes(Some(&past), &before) && behind(&past, &before));
    assert!(writes(Some(&before), &past) && !behind(&before, &past));
    log.add(
        Some(1),
        None,
        "task_status_changed",
        json!({"from": "ready", "to": "draft"}),
        bound,
    );
    let later = task_row(&log, bound);
    assert!(later.inputs_through.events > past.inputs_through.events);
    assert!(writes(Some(&past), &later) && !behind(&past, &later));
}

/// Passes at `now_ms` on `builder` after the events `new` came in, as the
/// supervisor's would on a store that holds every row as it is folded:
/// each folds the session and queue rows it builds, gives back the records
/// of those dropped from those of the rows held (`held`), and notes what
/// it holds of each. Two passes: the first notes the rows settled, the
/// second's due drops their records.
fn settle(
    builder: &mut Builder,
    held: &mut BTreeMap<Target, LedgerRow>,
    new: &[RunEvent],
    now_ms: i64,
) {
    builder.mark_events(new);
    for _ in 0..2 {
        builder.due(now_ms);
        let rows = builder.rows_to_build(builder.kept());
        for (kind, key) in builder.to_restore(&rows) {
            let records = held[&Target::Row(kind, key.clone())].records.clone();
            assert_eq!(builder.restore(kind, &key, records), Vec::new());
        }
        let inputs = Inputs::of(builder.kept(), now_ms, BUILT, &rows);
        builder.mark_rows(&inputs.rows);
        for target in builder.take(usize::MAX) {
            if let Some(row) = build(&target, &inputs, &[]) {
                builder.track(&target, &row);
                held.insert(target, row);
            }
        }
    }
}

/// `n` session rows and queue rows that end and settle: a planner's
/// session, a hold on the claims, a mark; and a landed run, whose push
/// closes no pending push.
fn settled_history(n: i64) -> Log {
    let mut log = Log::default();
    for k in 0..n {
        let at = k * 10;
        log.add(
            None,
            None,
            "session_opened",
            json!({"kind": "runtime_planner", "planner_id": 1000 + k}),
            at,
        );
        let opened = log.last().as_i64();
        log.add(
            None,
            None,
            "session_closed",
            json!({"kind": "runtime_planner", "planner_id": 1000 + k, "opened_event_id": opened}),
            at + 1,
        )
        .add(None, None, "claim_held", json!({"supervisor": "h"}), at + 2)
        .add(
            None,
            None,
            "claim_resumed",
            json!({"supervisor": "h"}),
            at + 3,
        )
        .add(None, None, "mark_added", json!({"mark": k}), at + 4)
        .landed(100 + k, &format!("{k:08x}-2222-4222-8222-222222222222"), at);
    }
    log
}

/// The records of the session and queue rows settled and the pushes that
/// closed no pending push are not kept: a history of more of them keeps
/// as many events as one of fewer, the session and queue rows open and
/// the books aside.
#[test]
fn the_events_of_settled_rows_are_not_kept_however_many() {
    let kept = |n: i64| {
        let mut log = settled_history(n);
        let before = log.last();
        log.add(
            None,
            None,
            "session_opened",
            json!({"kind": "runtime_planner", "planner_id": 1}),
            900_000,
        )
        .add(
            None,
            None,
            "claim_held",
            json!({"supervisor": "s"}),
            900_001,
        );
        let mut builder = Builder::default();
        let mut held = BTreeMap::new();
        settle(
            &mut builder,
            &mut held,
            &log.events[..log.events.iter().position(|e| e.id > before).unwrap()],
            ms(500_000),
        );
        settle(&mut builder, &mut held, &log.after(before), ms(900_002));
        builder.kept().size()
    };
    let (few, many) = (kept(3), kept(300));
    // The open session and the open hold.
    assert_eq!((few.sessions, few.queue, few.pushes), (1, 1, 0));
    assert_eq!(few.events(), many.events());
    assert_eq!(many.dropped_rows, 300 * 3);
    assert!(many.session_rows > few.session_rows && many.bytes > few.bytes);
    assert!(many.lives > 0 && many.pre_claim > few.pre_claim && many.queue_rows > 0);
}

/// A push that closes a pending push is kept, and a run's and a task's
/// fold reads it where it is kept as from every push.
#[test]
fn a_push_that_closes_a_pending_one_is_kept() {
    let (a, b, c) = (run_id(1), run_id(2), run_id(3));
    let mut log = Log::default();
    log.landed(3, &c, 0)
        .worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910)
        .landed(2, &b, 2000);
    let kept = Kept::of(&log.events);
    assert_eq!(kept.size().pushes, 1);
    let whole: Vec<RunEvent> = log
        .events
        .iter()
        .filter(|event| event.kind == EventKind::PushFinished)
        .cloned()
        .collect();
    assert_eq!(whole.len(), 2);
    let run_events: Vec<RunEvent> = log
        .events
        .iter()
        .filter(|event| event.run_id.as_ref().map(|run| run.as_str()) == Some(a.as_str()))
        .cloned()
        .collect();
    let task_events: Vec<RunEvent> = log
        .events
        .iter()
        .filter(|event| event.task_id == Some(TaskId::new(1)))
        .cloned()
        .collect();
    let now = ms(9000);
    let row = super::super::run_row(&a, &run_events, &kept.pushes, now, BUILT);
    assert!(row.ended_at.is_some());
    assert_eq!(
        row,
        super::super::run_row(&a, &run_events, &whole, now, BUILT)
    );
    let waits = Vec::<crate::domain::pre_claim::PreClaimInterval>::new();
    assert_eq!(
        super::super::task_row(
            TaskId::new(1),
            &task_events,
            &kept.pushes,
            &waits,
            now,
            BUILT
        ),
        super::super::task_row(TaskId::new(1), &task_events, &whole, &waits, now, BUILT)
    );
}

/// A later event that reaches a settled row whose records were dropped
/// (a late turn of a closed session, a later event of the supervisor whose
/// stop ended a stream) folds it from its records given back from the row
/// held: the session and queue rows are those of the fold of every event.
#[test]
fn a_dropped_row_a_later_event_reaches_folds_as_every_event() {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 7}),
        0,
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": 7, "opened_event_id": 1}),
        10,
    )
    .add(
        None,
        None,
        "live_alert_baseline",
        json!({"supervisor": "s", "version": 1}),
        20,
    )
    .add(
        None,
        None,
        "live_alert_input_changed",
        json!({"supervisor": "s", "version": 2}),
        30,
    )
    .add(
        None,
        None,
        "supervisor_stopped",
        json!({"supervisor": "s"}),
        40,
    );
    let mut builder = Builder::default();
    let mut held = BTreeMap::new();
    settle(&mut builder, &mut held, &log.events, ms(100_000));
    assert_eq!(builder.kept().size().events(), 0);
    let dropped = builder.kept().size().dropped_rows;
    assert!(dropped >= 2, "{dropped}");

    let before = log.last();
    log.add(
        None,
        None,
        "session_turns",
        json!({"kind": "runtime_planner", "planner_id": 7, "opened_event_id": 1}),
        200_000,
    )
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s"}),
        200_001,
    );
    let now = ms(200_002);
    builder.mark_events(&log.after(before));
    builder.due(now);
    let rows = builder.rows_to_build(builder.kept());
    let restored = builder.to_restore(&rows);
    assert!(
        restored
            .iter()
            .any(|(kind, _)| *kind == LedgerKind::Session),
        "{restored:?}"
    );
    for (kind, key) in restored {
        let records = held[&Target::Row(kind, key.clone())].records.clone();
        assert_eq!(builder.restore(kind, &key, records), Vec::new());
    }
    let inputs = Inputs::of(builder.kept(), now, BUILT, &rows);
    let whole = Inputs::new(&log.events, now, BUILT, true);
    assert!(!inputs.rows.is_empty());
    for (key, row) in &inputs.rows {
        assert_eq!(Some(row), whole.rows.get(key), "{key:?}");
    }
}

/// The fold at `now_ms` of the run or task `target` as a pass makes it on
/// `builder`: its events of `log`, and the pushes the builder no longer
/// keeps read back by id.
fn fold_of(builder: &Builder, target: &Target, log: &Log, now_ms: i64) -> Option<LedgerRow> {
    let mut inputs = Inputs::of(builder.kept(), now_ms, BUILT, &BTreeSet::new());
    let events: Vec<RunEvent> = log
        .events
        .iter()
        .filter(|event| match target {
            Target::Run(run) => event.run_id.as_ref().map(|id| id.as_str()) == Some(run.as_str()),
            Target::Task(task) => event.task_id == Some(*task),
            _ => false,
        })
        .cloned()
        .collect();
    let ids = builder.pushes_to_read(&events);
    let pushes = log
        .events
        .iter()
        .filter(|event| ids.contains(&event.id))
        .cloned()
        .collect();
    inputs.read_pushes(target, pushes);
    inputs.read(target, events);
    build(target, &inputs, &[])
}

/// A push that closed a pending push is dropped once the rows of the run
/// it closed and of its task are settled; a later event of the run folds
/// it and its task again on the push read back by id, as the fold of
/// every event.
#[test]
fn a_closing_push_goes_once_its_run_and_task_settle_and_comes_back_by_id() {
    let (a, b) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910)
        .add(
            Some(1),
            None,
            "task_status_changed",
            json!({"from": "in_progress", "to": "completed"}),
            910,
        )
        .landed(2, &b, 2000);
    let mut builder = Builder::default();
    builder.mark_events(&log.events);
    builder.take(usize::MAX);
    let targets = [Target::Run(a.clone()), Target::Task(TaskId::new(1))];
    let now = ms(100_000);
    assert_eq!(builder.kept().size().pushes, 1);
    // The run settles: the task still reads the push.
    let run = fold_of(&builder, &targets[0], &log, now).unwrap();
    assert!(run.is_final && run.ended_at.is_some());
    builder.track(&targets[0], &run);
    builder.due(now);
    assert_eq!(builder.kept().size().pushes, 1);
    let task = fold_of(&builder, &targets[1], &log, now).unwrap();
    assert!(task.is_final);
    builder.track(&targets[1], &task);
    builder.due(now);
    assert_eq!(builder.kept().size().pushes, 0);
    assert_eq!(builder.kept().size().push_runs, 2);

    let before = log.last();
    log.add(Some(1), Some(&a), "note_added", json!({}), 200_000);
    builder.mark_events(&log.after(before));
    let later = ms(200_001);
    let whole = Inputs::new(&log.events, later, BUILT, false);
    for target in &targets {
        let row = fold_of(&builder, target, &log, later);
        assert!(row.is_some_and(|row| row.ended_at.is_some()));
        assert_eq!(
            fold_of(&builder, target, &log, later),
            build(target, &whole, &[]),
            "{target:?}"
        );
    }
}

/// The bytes count the room of the kept events' vectors, used or not, and
/// the texts and payloads of the events in them: a drop of the events of
/// the rows settled gives back the room the events read at the start took,
/// and spare room is counted as it is.
#[test]
fn the_bytes_count_the_room_of_the_events_and_a_drop_gives_it_back() {
    let room = |kept: &Kept| {
        (kept.sessions.capacity() + kept.queue.capacity() + kept.pushes.capacity())
            * size_of::<RunEvent>()
    };
    let heap = |kept: &Kept| {
        kept.sessions
            .iter()
            .chain(&kept.queue)
            .chain(&kept.pushes)
            .map(event_heap_bytes)
            .sum::<usize>()
    };
    let mut log = settled_history(300);
    let before = log.last();
    log.add(None, None, "mark_added", json!({"mark": "open"}), 900_000)
        .add(
            None,
            None,
            "session_opened",
            json!({"kind": "runtime_planner", "planner_id": 1}),
            900_001,
        );
    let mut builder = Builder::default();
    builder.mark_events(&log.events[..log.events.iter().position(|e| e.id > before).unwrap()]);
    let read = builder.kept().size();
    let read_room = room(builder.kept());
    assert!(read_room >= read.events() * size_of::<RunEvent>());
    assert_eq!(builder.kept().event_heap, heap(builder.kept()));

    let mut held = BTreeMap::new();
    settle(&mut builder, &mut held, &[], ms(500_000));
    settle(&mut builder, &mut held, &log.after(before), ms(900_002));
    let kept = builder.kept();
    let dropped = kept.size();
    assert_eq!((dropped.sessions, dropped.queue, dropped.pushes), (1, 1, 0));
    assert_eq!(kept.event_heap, heap(kept));
    for events in [&kept.sessions, &kept.queue, &kept.pushes] {
        assert!(
            // One the new events grew from empty holds the least room a vector takes.
            events.capacity() <= (2 * events.len()).max(4),
            "{} of {}",
            events.capacity(),
            events.len()
        );
    }
    assert!(read.bytes - dropped.bytes >= read_room - room(kept) && read_room > room(kept));

    // Room not used is counted as much as room used.
    let mut spare = (**kept).clone();
    let (bytes, capacity) = (spare.size().bytes, spare.queue.capacity());
    spare.queue.reserve_exact(1000);
    assert_eq!(
        spare.size().bytes - bytes,
        (spare.queue.capacity() - capacity) * size_of::<RunEvent>()
    );
}

/// R holds each cursor's position before the first input that made a mark
/// still left: an event's for a task, a sample's for its hour, a step's
/// for its run; a later input of a mark left keeps the earlier position.
/// Once each is looked at, R is the cursors again. A mark that came with
/// time (a grace over) holds back none.
#[test]
fn r_stops_before_the_first_input_of_each_mark_left() {
    use crate::domain::RunId;
    use crate::domain::session_step::{Step, StepKind, StepTarget};
    let a = run_id(1);
    let mut log = Log::default();
    for task in 1..=3 {
        log.add(Some(task), None, "task_created", json!({}), task);
    }
    let mut builder = Builder::default();
    assert_eq!(builder.rows_written(), Some(builder.cursors()));
    builder.mark_events(&log.events[..1]);
    let first = log.events[0].id;
    // Task 1 looked at: the next marks start after it.
    assert_eq!(builder.take(1), [Target::Task(TaskId::new(1))]);
    builder.mark_events(&log.events[1..]);
    let r = builder.rows_written().unwrap();
    assert_eq!(r.events, first);
    // Task 2 marked again later keeps its first position.
    let before = log.last();
    log.add(Some(2), None, "task_created", json!({}), 10);
    builder.mark_events(&log.after(before));
    assert_eq!(builder.rows_written().unwrap().events, first);

    let start = node::bucket_of(1_900_000_000);
    builder.mark_samples(&[sample(4, start + 10), sample(5, start + 20)]);
    let step = SessionStep {
        id: 9,
        target: StepTarget::run(&RunId::new(&a).unwrap()),
        session_id: "s".to_owned(),
        received_at: 0,
        step: Step::new(9, StepKind::Wait, 0, 1),
    };
    builder.steps = 8;
    builder.rows_through.steps = 8;
    builder.mark_steps(&[step]);
    let r = builder.rows_written().unwrap();
    assert_eq!((r.samples, r.steps, r.events), (0, 8, first));
    assert_eq!((builder.samples, builder.steps), (5, 9));

    // Every mark looked at: R is the cursors.
    builder.take(usize::MAX);
    builder.due(0);
    assert_eq!(builder.rows_written(), Some(builder.cursors()));

    // A run whose grace is over is marked with no position.
    let b = run_id(2);
    let mut landed = Log::default();
    landed.landed(2, &b, 0);
    let row = build(&Target::Run(b.clone()), &inputs(&landed, 1000), &[]).unwrap();
    builder.track(&Target::Run(b.clone()), &row);
    builder.due(i64::MAX);
    assert!(builder.marked());
    assert_eq!(builder.rows_written(), Some(builder.cursors()));
}

/// A builder whose cursor was set past its marks gives no R; one set at
/// the R of the store's mark ([`Builder::go_on_from_rows`]) gives R from
/// there, and follows the inputs it reads after it.
#[test]
fn only_a_builder_that_follows_every_input_gives_r() {
    let mut log = Log::default();
    log.add(Some(1), None, "task_created", json!({}), 1).add(
        Some(2),
        None,
        "task_created",
        json!({}),
        2,
    );
    let mut set = Builder {
        events: log.events[0].id,
        ..Builder::default()
    };
    set.mark_events(&log.events[1..]);
    assert_eq!(set.rows_written(), None);
    set.samples = 3;
    assert_eq!(set.rows_written(), None);

    let from = Cursors {
        events: log.events[0].id,
        samples: 3,
        steps: 4,
        resources: 5,
        inside_resources: 6,
    };
    let mut gone_on = Builder::default();
    gone_on.go_on_from_rows(from);
    assert_eq!(gone_on.rows_written(), Some(from));
    gone_on.mark_events(&log.events[1..]);
    let r = gone_on.rows_written().unwrap();
    assert_eq!(r, from, "task 2 is left");
    gone_on.take(usize::MAX);
    gone_on.due(0);
    assert_eq!(
        gone_on.rows_written(),
        Some(Cursors {
            events: log.last(),
            ..from
        })
    );
}

/// A row written after an input marked it holds back no R when its grace
/// marks it again: the position of that input went with the write.
#[test]
fn a_row_written_and_marked_again_by_its_grace_holds_no_position() {
    let b = run_id(2);
    let mut log = Log::default();
    log.landed(2, &b, 0);
    let target = Target::Run(b);
    let mut builder = Builder::default();
    builder.mark_events(&log.events);
    assert!(builder.rows_written().unwrap().events < builder.events);
    builder.take(usize::MAX);
    let row = build(&target, &inputs(&log, 1000), &[]).unwrap();
    assert!(!row.is_final);
    builder.track(&target, &row);
    builder.due(i64::MAX);
    assert!(builder.marked());
    assert_eq!(builder.rows_written(), Some(builder.cursors()));
}

/// R holds the latest time read up to the position a mark left cuts it
/// back to: not its cursor's, nor the time of the event at it. With no mark
/// left it holds its cursor's.
#[test]
fn r_holds_the_time_read_at_the_position_it_is_cut_back_to() {
    let mut log = Log::default();
    log.add(Some(1), None, "task_created", json!({}), 200)
        .add(Some(2), None, "task_created", json!({}), 50)
        .add(Some(3), None, "task_created", json!({}), 300);
    let mut builder = Builder::default();
    builder.mark_events(&log.events);
    builder.take(usize::MAX);
    builder.keep(Target::Task(TaskId::new(3)));
    let mark = builder.sessions_mark().unwrap().rows.unwrap();
    assert_eq!(mark.through.events, log.events[1].id);
    assert_eq!(mark.read_ms, Some(ms(200)));

    builder.take(usize::MAX);
    builder.due(ms(300));
    let mark = builder.sessions_mark().unwrap().rows.unwrap();
    assert_eq!(mark.through.events, log.last());
    assert_eq!(mark.read_ms, Some(ms(300)));
}

/// A queue row's mark, made as the rows are named, holds the time read at
/// the event its input starts after: an event of no run or task, the one
/// after an event timed before an earlier one.
#[test]
fn a_rows_mark_holds_the_time_read_at_the_event_before_its_input() {
    let mut log = Log::default();
    log.add(Some(1), None, "task_created", json!({}), 200)
        .add(Some(2), None, "task_created", json!({}), 50)
        .add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s", "class": "heavy"}),
            100,
        )
        .add(Some(3), None, "task_created", json!({}), 300);
    let mut builder = Builder::default();
    builder.mark_events(&log.events);
    let keys = builder.rows_to_build(builder.kept());
    let inputs = Inputs::of(builder.kept(), ms(300), BUILT, &keys);
    builder.mark_rows(&inputs.rows);
    drop(inputs);
    let tasks: Vec<Target> = (1..=3)
        .map(|task| Target::Task(TaskId::new(task)))
        .collect();
    for task in &tasks {
        builder.marks.remove(task);
        builder.dirtied.remove(task);
    }
    let mark = builder.sessions_mark().unwrap().rows.unwrap();
    assert_eq!(mark.through.events, log.events[1].id);
    assert_eq!(mark.read_ms, Some(ms(200)));
}

/// The mark of a row is the one [`Target::row_key`] gives its key back from,
/// and a key of no such row has none.
#[test]
fn a_rows_kind_and_key_give_its_mark() {
    for target in [
        Target::Run(run_id(1)),
        Target::Task(TaskId::new(7)),
        Target::Row(LedgerKind::Session, "plan_review/9/session/1".to_owned()),
        Target::Row(LedgerKind::Queue, "12".to_owned()),
        Target::Node("host/a".to_owned(), 3600),
    ] {
        let (kind, key) = target.row_key();
        assert_eq!(Target::of_row(kind, &key), Some(target));
    }
    assert_eq!(Target::of_row(LedgerKind::Task, "x"), None);
    assert_eq!(Target::of_row(LedgerKind::Node, "x"), None);
}

/// A builder that walks the rows of an older version reads as many as fill
/// a pass's writes with the marks waiting, after the last it read, none
/// while the marks fill them, and is done at a page short of its limit; one
/// that does not walk reads none. Until then it counts as a mark left.
#[test]
fn the_rows_of_an_older_version_are_read_a_share_at_a_time() {
    let task = |n: usize| (LedgerKind::Task, n.to_string());
    let mut builder = Builder::default();
    assert_eq!(builder.older_rows_to_read(), None);
    assert_eq!(builder.backlog(), 0);

    builder.walk_older_rows();
    assert_eq!(builder.backlog(), 1);
    assert_eq!(
        builder.older_rows_to_read(),
        Some((None, OLDER_ROWS_PER_PASS))
    );

    let page: Vec<_> = (1..=OLDER_ROWS_PER_PASS).map(task).collect();
    let last = page.last().cloned().unwrap();
    builder.mark_older_rows(page, OLDER_ROWS_PER_PASS);
    assert_eq!(builder.backlog(), OLDER_ROWS_PER_PASS + 1);
    assert_eq!(builder.older_rows_to_read(), None);
    // Such a mark holds no position.
    assert_eq!(builder.rows_written(), Some(builder.cursors()));

    let taken = builder.take(OLDER_ROWS_PER_PASS - 3);
    assert_eq!(taken.len(), OLDER_ROWS_PER_PASS - 3);
    assert_eq!(
        builder.older_rows_to_read(),
        Some((Some(&last), OLDER_ROWS_PER_PASS - 3))
    );

    builder.mark_older_rows(vec![task(1_000)], OLDER_ROWS_PER_PASS - 3);
    assert_eq!(builder.older_rows_to_read(), None);
    assert_eq!(builder.backlog(), 4);
    builder.take(4);
    assert_eq!(builder.older_rows_to_read(), None);
    assert_eq!(builder.backlog(), 0);
}

/// Reading the summary does not move the round-robin take or any cursor;
/// only a non-session event position chooses the single oldest blocker.
#[test]
fn mark_observation_is_read_only_and_ignores_session_positions() {
    let mut builder = Builder::default();
    let empty = builder.observe_marks();
    assert_eq!(empty.rows.unwrap().through, builder.cursors());
    assert!(empty.oldest.is_none());
    assert_eq!(
        builder.sessions_mark_from(empty.rows),
        builder.sessions_mark()
    );
    let targets = [
        Target::Run(run_id(1)),
        Target::Task(TaskId::new(2)),
        Target::Task(TaskId::new(3)),
        Target::Row(LedgerKind::Session, "s".to_owned()),
        Target::Row(LedgerKind::Queue, "q".to_owned()),
        Target::Node("node".to_owned(), 0),
    ];
    builder.go_on_from_rows(Cursors {
        events: EventId::new(20),
        samples: 10,
        ..Cursors::default()
    });
    builder.marks.extend(targets.iter().cloned());
    for (index, event) in [(0, 8), (1, 5), (3, 0)] {
        builder.dirtied.insert(
            targets[index].clone(),
            Dirtied::events(ReadTo {
                event: EventId::new(event),
                read_ms: event * 1000,
            }),
        );
    }
    builder.dirtied.insert(
        targets[5].clone(),
        Dirtied {
            samples: Some(4),
            ..Dirtied::default()
        },
    );
    builder.next = Some(targets[1].clone());
    builder.take_offset = 3;
    builder.defer_time(ms(1000));
    let before = (
        builder.marks.clone(),
        builder.dirtied.clone(),
        builder.cursors(),
        builder.next.clone(),
    );
    let observed = builder.observe_marks();
    assert_eq!(observed.oldest.as_ref().unwrap().0, targets[1]);
    assert_eq!(observed.rows, builder.rows_mark());
    assert_eq!(observed.rows.unwrap().take_offset, 3);
    assert_eq!(observed.rows.unwrap().through.events, EventId::new(5));
    assert_eq!(observed.rows.unwrap().through.samples, 4);
    assert_eq!(observed.counts[&LedgerKind::Run].positioned, 1);
    assert_eq!(
        observed.counts[&LedgerKind::Task],
        MarkCounts {
            positioned: 1,
            unpositioned: 1
        }
    );
    assert_eq!(observed.counts[&LedgerKind::Session].unpositioned, 1);
    assert_eq!(observed.counts[&LedgerKind::Queue].unpositioned, 1);
    assert_eq!(observed.counts[&LedgerKind::Node].positioned, 1);
    assert_eq!(
        before,
        (
            builder.marks.clone(),
            builder.dirtied.clone(),
            builder.cursors(),
            builder.next.clone()
        )
    );
    assert_eq!(builder.take(1), [targets[1].clone()]);
    builder
        .marks
        .retain(|t| matches!(t, Target::Row(LedgerKind::Session, _)));
    assert!(builder.observe_marks().oldest.is_none());
    assert_eq!(
        builder.observe_marks().rows.unwrap().through,
        builder.cursors()
    );
    let mut session_only = builder.observe_marks();
    builder.take_sessions();
    builder.refresh_session_observation(&mut session_only);
    assert_eq!(session_only, builder.observe_marks());
}

/// Every sweep outcome counts the decision actually used to retain or
/// remove the mark; observing it leaves the writer's predicate unchanged.
#[test]
fn sweep_counts_retention_reasons_and_removal() {
    let (log, _) = crate::domain::ledger::testkit::sessions_across();
    let mut row = crate::domain::ledger::session::session_rows(&log.events, BUILT).remove(0);
    row.is_final = true;
    let target = Target::Row(row.kind, row.key.clone());
    let mut builder = Builder::default();
    builder.keep(target.clone());
    let mut counts = SweepCounts::default();
    counts.record(builder.sweep(&target, None, None));
    counts.record(builder.sweep(&target, None, Some(&row)));
    let mut open = row.clone();
    open.is_final = false;
    counts.record(builder.sweep(&target, Some(&open), Some(&row)));
    let mut older = row.clone();
    older.ledger_v = LEDGER_V - 1;
    counts.record(builder.sweep(&target, Some(&older), Some(&row)));
    let mut later = row.clone();
    later.inputs_through.events = EventId::new(i64::MAX);
    counts.record(builder.sweep(&target, Some(&row), Some(&later)));
    let mut ended = row.clone();
    ended
        .inputs_through
        .life_ended
        .push((EventId::new(100), EventId::new(101)));
    counts.record(builder.sweep(&target, Some(&row), Some(&ended)));
    assert!(builder.marks.contains(&target));
    counts.record(builder.sweep(&target, Some(&row), Some(&row)));
    assert!(!builder.marks.contains(&target));
    assert_eq!(
        (
            counts.no_build,
            counts.no_stored,
            counts.not_final,
            counts.removed
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(
        counts.writes,
        BTreeMap::from([("older_version", 1), ("later_event", 1), ("life_ended", 1)])
    );
}

/// The comparison is of each input, independent of the row's version or
/// finality. No missing input may disappear behind an advanced R.
#[test]
fn recorded_inputs_ignore_version_and_finality_but_require_every_input() {
    let mut log = Log::default();
    log.add(None, None, "authorization_denied", json!({}), 1);
    let built = crate::domain::ledger::build(&log.events, ms(2), BUILT).remove(0);
    let mut stored = built.clone();
    stored.ledger_v = LEDGER_V - 1;
    stored.is_final = false;
    assert!(Builder::inputs_recorded(&stored, &built));
    for field in 0..7 {
        let mut missing = stored.clone();
        let mut needed = built.clone();
        match field {
            0 => missing.inputs_through.events = EventId::new(0),
            1 => needed.inputs_through.supervisor = Some(EventId::new(10)),
            2 => needed.inputs_through.node_samples = Some(10),
            3 => needed.inputs_through.steps = Some(10),
            4 => needed.inputs_through.resources = Some(10),
            5 => needed.inputs_through.inside_resources = Some(10),
            _ => needed
                .inputs_through
                .life_ended
                .push((EventId::new(1), EventId::new(2))),
        }
        assert!(
            !Builder::inputs_recorded(&missing, &needed),
            "field {field}"
        );
    }
    let target = Target::Row(built.kind, built.key.clone());
    let mut builder = Builder::default();
    builder.mark_events(&log.events);
    let rows = BTreeMap::from([((built.kind, built.key.clone()), built.clone())]);
    builder.mark_rows(&rows);
    assert_eq!(builder.rows_written().unwrap().events, EventId::new(0));
    let mut missing = stored.clone();
    missing.inputs_through.events = EventId::new(0);
    assert!(matches!(
        builder.sweep(&target, Some(&missing), Some(&built)),
        SweepOutcome::NotFinal | SweepOutcome::Writes(_)
    ));
    assert_eq!(builder.rows_written().unwrap().events, EventId::new(0));
    assert_eq!(
        builder.sweep(&target, Some(&stored), Some(&built)),
        SweepOutcome::Writes("older_version")
    );
    assert_eq!(builder.rows_written().unwrap().events, log.last());
    stored.ledger_v = LEDGER_V;
    assert_eq!(
        builder.sweep(&target, Some(&stored), Some(&built)),
        SweepOutcome::Removed
    );
    // Tracking the stored, unsettled row still schedules its grace.
    builder.due(ms(3600));
    assert!(builder.take(CHECKS_PER_PASS).contains(&target));
}

/// With an unchanged set, persisted ranks cover even the rows after the
/// check bound when each process lives for only one pass.
#[test]
fn persisted_take_rank_visits_marks_across_restarts() {
    let count = CHECKS_PER_PASS * 3 + 17;
    let mut seen = BTreeSet::new();
    let mut offset = 0;
    for _ in 0..4 {
        let mut builder = Builder::default();
        builder
            .marks
            .extend((1..=count).map(|id| Target::Task(TaskId::new(id as i64))));
        builder.resume_taking(offset);
        seen.extend(builder.take(CHECKS_PER_PASS));
        offset = builder.rows_mark().unwrap().take_offset;
    }
    assert_eq!(seen.len(), count);
}

/// The bound is the latest time read, including out-of-order event times,
/// until the snapshot is caught up; then the wall clock may advance it.
#[test]
fn snapshot_time_waits_for_unread_events() {
    let mut builder = Builder::default();
    assert_eq!(
        builder.folds_at_snapshot(ms(1000), EventId::new(1)),
        i64::MIN
    );
    let mut log = Log::default();
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s"}),
        20,
    )
    .add(None, None, "mark_added", json!({}), 10);
    builder.mark_events(&log.events);
    assert_eq!(builder.folds_at_snapshot(ms(1000), EventId::new(3)), ms(20));
    assert_eq!(builder.folds_at_snapshot(ms(1000), log.last()), ms(1000));
    assert_eq!(builder.folds_at_snapshot(ms(1), log.last()), ms(20));
}

/// Open rows written at a bounded prefix retain time work, while settled,
/// legacy and clock-ahead rows never acquire a backward clock write.
#[test]
fn open_row_time_only_advances() {
    let mut log = Log::default();
    log.add(Some(1), None, "task_created", json!({"status": "draft"}), 1);
    let target = Target::Task(TaskId::new(1));
    let row = build(&target, &inputs(&log, 2), &[]).unwrap();
    let later = build(&target, &inputs(&log, 1000), &[]).unwrap();
    let mut builder = Builder::default();
    assert!(!builder.time_advanced(&row, &later));
    builder.defer_time(ms(1000));
    assert!(builder.time_advanced(&row, &later));
    assert!(!builder.time_advanced(&row, &row));
    assert!(!builder.time_advanced(&later, &row));
    let even_later = build(&target, &inputs(&log, 2000), &[]).unwrap();
    assert!(!builder.time_advanced(&later, &even_later));
    let mut newer = row.clone();
    newer.ledger_v = LEDGER_V + 1;
    assert!(!builder.time_advanced(&newer, &later));
    let mut final_row = row.clone();
    final_row.is_final = true;
    assert!(!builder.time_advanced(&final_row, &later));
    let mut legacy = row;
    legacy.legacy = Some(json!({}));
    assert!(!builder.time_advanced(&legacy, &later));
}

/// Each certificate stops before its own unrecorded input. Queue work
/// cannot hold core inputs back, nor can a core write hold queue back.
#[test]
fn queue_and_core_certificates_cover_only_their_own_recorded_inputs() {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "supervisor_started",
        json!({"supervisor": "s"}),
        1,
    );
    log.add(Some(1), None, "task_created", json!({}), 2);
    let mut builder = Builder::default();
    builder.mark_events(&log.events);
    let input = inputs(&log, 100);
    builder.mark_rows(&input.rows);
    let task = Target::Task(TaskId::new(1));
    let core_before = builder.rows_mark().unwrap().position(false).unwrap();
    let queue_before = builder.rows_mark().unwrap().position(true).unwrap();
    assert!(core_before.through.events < log.last());
    assert!(queue_before.through.events < log.last());
    builder.track(&task, &build(&task, &input, &[]).unwrap());
    let mark = builder.rows_mark().unwrap();
    assert_eq!(mark.position(false).unwrap().through.events, log.last());
    assert_eq!(mark.position(true), Some(queue_before));
    assert_eq!(builder.observe_marks().rows, Some(mark));
    for (key, row) in &input.rows {
        builder.track(&Target::Row(key.0, key.1.clone()), row);
    }
    assert_eq!(builder.rows_written(), Some(builder.cursors()));

    // A core-only input cannot lower Q. Include a non-event input as well.
    builder.dirtied.insert(
        task.clone(),
        Dirtied::events(ReadTo {
            event: EventId::new(1),
            read_ms: ms(1),
        }),
    );
    builder.marks.insert(task);
    let node = Target::Node("n".into(), 0);
    builder.samples = 5;
    builder.rows_through.samples = 5;
    builder.marks.insert(node.clone());
    builder.dirtied.insert(
        node,
        Dirtied {
            samples: Some(3),
            ..Dirtied::default()
        },
    );
    let mark = builder.rows_mark().unwrap();
    assert_eq!(
        mark.position(false).unwrap().through.events,
        EventId::new(1)
    );
    assert_eq!(mark.position(false).unwrap().through.samples, 3);
    assert_eq!(mark.position(true).unwrap().through, builder.cursors());
}

#[test]
fn both_kinds_get_a_turn_among_the_marks_holding_r() {
    let mut builder = Builder::default();
    for (kind, before) in [
        (LedgerKind::Queue, 1),
        (LedgerKind::Queue, 2),
        (LedgerKind::Task, 10),
    ] {
        let target = if kind == LedgerKind::Task {
            Target::Task(TaskId::new(1))
        } else {
            Target::Row(kind, before.to_string())
        };
        builder.marks.insert(target.clone());
        builder.dirtied.insert(
            target,
            Dirtied::events(ReadTo {
                event: EventId::new(before),
                read_ms: before,
            }),
        );
    }
    let taken = builder.holding_r(2);
    assert!(taken.contains(&Target::Task(TaskId::new(1))));
    assert!(taken.contains(&Target::Row(LedgerKind::Queue, "1".into())));
}

/// Coverage validates queue R, independently of core R and of the event
/// time needed only for restarting the fold of a legacy certificate.
#[test]
fn coverage_checks_queue_r_even_when_a_legacy_mark_has_no_event_time() {
    use crate::domain::ledger::session::written::RowPosition;
    let kinds = ["authorization_denied".to_owned()];
    for (queue, core, legacy, stopped) in [
        (7, 7, true, true),
        (7, 2, false, true),
        (2, 7, false, false),
    ] {
        let mut builder = Builder::default();
        builder.set_omission(omitted::Omission::of(kinds.iter().map(String::as_str), 2));
        let position = |event| RowPosition {
            through: Cursors {
                events: EventId::new(event),
                ..Cursors::default()
            },
            read_ms: event,
        };
        let mut rows = RowsWritten::at(position(queue.min(core)).through, 0);
        if legacy {
            rows.read_ms = None;
        } else {
            rows.queue = Some(position(queue));
            rows.core = Some(position(core));
        }
        let mut coverage = omitted::Coverage::start(&kinds, EventId::new(0));
        coverage.through = EventId::new(5);
        builder.begin_coverage(Some(&Marks {
            sessions: SessionsWritten::at(EventId::new(10)),
            rows: Some(rows),
            omitted: Some(omitted::Omitted {
                coverage,
                points: Vec::new(),
            }),
        }));
        assert_eq!(
            builder.omission_stopped(),
            stopped,
            "queue={queue}, core={core}, legacy={legacy}"
        );
    }
}

/// A pass has taken its time at [`PASS_TIME`], not before.
#[test]
fn a_pass_is_spent_at_its_time_and_not_before() {
    assert!(!pass_spent(std::time::Duration::ZERO));
    assert!(!pass_spent(PASS_TIME - std::time::Duration::from_nanos(1)));
    assert!(pass_spent(PASS_TIME));
    assert!(pass_spent(PASS_TIME * 2));
}
