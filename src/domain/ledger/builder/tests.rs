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
