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
    // A task on its way is not written, but its fold reads the waits:
    // count those it is given made for it, not read where they are kept.
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
    assert_eq!(build(&task, &inputs, &[]), None);
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
#[test]
fn a_pass_makes_as_much_of_the_same_new_inputs_whatever_the_history() {
    let (few, kept_few, closed_few) = one_pass(history(3));
    let (many, kept_many, closed_many) = one_pass(history(300));
    assert!(kept_many > kept_few * 50, "{kept_few} {kept_many}");
    // The history's, and the new inputs' own.
    assert_eq!((closed_few, closed_many), (3 + 1, 300 + 1));
    assert_eq!(few, many);
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
