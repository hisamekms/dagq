//! The rows of tasks with time ready, as the waits before a claim of the
//! queue as a whole move after a pass wrote them (by an event of no task,
//! or a time past a supervisor's silence): after each pass the stored row
//! is the fold of every event read so far.

use super::*;

/// A task ready and not ended, whose row a pass wrote, reads the waits
/// before a claim of the queue as a whole that come in later (a claim
/// hold, a supervisor's slots full and its life's end) though no event of
/// the task does: after each pass its row is the fold of every event read
/// so far, whatever many events a pass reads.
#[test]
fn a_ready_tasks_row_follows_the_waits_of_the_queue_that_come_in_later() {
    let mut log = Log::default();
    log.add(Some(8), None, "task_created", json!({"status": "draft"}), 0)
        .add(
            Some(8),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "ready"}),
            10,
        )
        .add(None, None, "claim_held", json!({"supervisor": "s1"}), 20)
        .add(None, None, "claim_resumed", json!({"supervisor": "s1"}), 30)
        .add(
            None,
            None,
            "supervisor_started",
            json!({"supervisor": "s2"}),
            40,
        )
        .add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s2", "class": "heavy"}),
            50,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s2"}),
            60,
        )
        .add(
            None,
            None,
            "supervisor_stopped",
            json!({"supervisor": "s2"}),
            70,
        );
    let all = log.events.len();
    for step in [1, 2, all] {
        let memory = Memory::default();
        let mut builder = Builder::default();
        let mut read = 0;
        while read < all {
            read = (read + step).min(all);
            let events = &log.events[..read];
            memory.events.replace(events.to_vec());
            drain(&memory, &mut builder, LATER);
            let whole = crate::domain::ledger::build(events, ms(LATER), BUILT);
            let mut expected = whole
                .into_iter()
                .find(|row| row.kind == LedgerKind::Task && row.key == "8")
                .unwrap();
            let stored = memory.row(LedgerKind::Task, "8");
            expected.is_final = stored.is_final;
            assert_eq!(stored, expected, "{step} a pass, {read} read");
        }
        let row = memory.row(LedgerKind::Task, "8");
        let kinds: Vec<&str> = row.detail["overlays"]
            .as_array()
            .unwrap()
            .iter()
            .map(|overlay| overlay["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["claim_hold", "slots_full"], "{step} a pass");
        assert!(row.ended_at.is_none() && !row.is_final);
    }
}

/// A task ready and not ended, whose row a pass wrote while a supervisor's
/// slots full lay open on it, is folded and written again by the first
/// pass past that supervisor's silence, though no event came in: its
/// overlay ends at the supervisor's last event as the fold of every event
/// then ends it. A pass before the silence leaves the row as it was.
#[test]
fn a_ready_tasks_row_follows_a_slots_full_its_supervisors_silence_ends() {
    use crate::domain::{HEARTBEAT_TIMEOUT_SECS, supervisor_life::SUPERVISOR_ALIVE_INTERVAL_SECS};
    let mut log = Log::default();
    log.add(Some(8), None, "task_created", json!({"status": "draft"}), 0)
        .add(
            Some(8),
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
    // The last second s's life goes on: the next one is past its silence.
    let bound = 20 + SUPERVISOR_ALIVE_INTERVAL_SECS + HEARTBEAT_TIMEOUT_SECS;
    let whole = |secs: i64| {
        crate::domain::ledger::build(&log.events, ms(secs), BUILT)
            .into_iter()
            .find(|row| row.kind == LedgerKind::Task && row.key == "8")
            .unwrap()
    };
    let slots_full = |row: &LedgerRow| {
        row.detail["overlays"]
            .as_array()
            .unwrap()
            .iter()
            .find(|overlay| overlay["kind"] == "slots_full")
            .map(|overlay| (overlay["start"].as_i64(), overlay["end"].as_i64()))
    };
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    pass(&memory, &mut builder, 30);
    let open = memory.row(LedgerKind::Task, "8");
    assert_eq!(open, whole(30));
    assert_eq!(slots_full(&open), Some((Some(ms(10)), Some(ms(30)))));
    pass(&memory, &mut builder, bound);
    assert_eq!(memory.row(LedgerKind::Task, "8"), open);
    let writes = memory.writes.get();
    pass(&memory, &mut builder, bound + 1);
    let ended = memory.row(LedgerKind::Task, "8");
    assert_eq!(ended, whole(bound + 1));
    assert_eq!(slots_full(&ended), Some((Some(ms(10)), Some(ms(20)))));
    assert!(ended.ended_at.is_none() && !ended.is_final);
    assert!(memory.writes.get() > writes);
    // The same inputs at the same time leave it as it is.
    let writes = memory.writes.get();
    pass(&memory, &mut builder, bound + 1);
    assert_eq!(memory.writes.get(), writes);
}

/// A task ready and not ended, with the slots full of two supervisors on
/// it: s2's ended by its stop, s1's open until a pass past s1's silence,
/// whose last event is older than s2's stop. That pass folds and writes the
/// row again as the fold of every event then, though no event came in,
/// whether s1's life ends on the task's time ready or before it began.
#[test]
fn a_ready_tasks_row_follows_one_supervisors_silence_among_others_ended() {
    use crate::domain::supervisor_life::SILENT_AFTER_MS;
    for ready in [5, 22] {
        let mut log = Log::default();
        log.add(Some(8), None, "task_created", json!({"status": "draft"}), 0)
            .add(
                None,
                None,
                "slots_full_started",
                json!({"supervisor": "s1", "class": "heavy"}),
                10,
            )
            .add(
                None,
                None,
                "supervisor_alive",
                json!({"supervisor": "s1"}),
                20,
            )
            .add(
                Some(8),
                None,
                "task_status_changed",
                json!({"from": "draft", "to": "ready"}),
                ready,
            )
            .add(
                None,
                None,
                "slots_full_started",
                json!({"supervisor": "s2", "class": "heavy"}),
                25,
            )
            .add(
                None,
                None,
                "supervisor_stopped",
                json!({"supervisor": "s2"}),
                30,
            );
        // The last second s1's life goes on.
        let bound = 20 + SILENT_AFTER_MS / 1000;
        let whole = |secs: i64| {
            crate::domain::ledger::build(&log.events, ms(secs), BUILT)
                .into_iter()
                .find(|row| row.kind == LedgerKind::Task && row.key == "8")
                .unwrap()
        };
        let of = |row: &LedgerRow, supervisor: &str| {
            row.detail["overlays"]
                .as_array()
                .unwrap()
                .iter()
                .find(|overlay| overlay["supervisor"] == supervisor)
                .map(|overlay| (overlay["start"].as_i64(), overlay["end"].as_i64()))
        };
        let memory = Memory::of(&log);
        let mut builder = Builder::default();
        pass(&memory, &mut builder, 40);
        let open = memory.row(LedgerKind::Task, "8");
        assert_eq!(open, whole(40), "ready at {ready}");
        assert_eq!(
            of(&open, "s1"),
            Some((Some(ms(10.max(ready))), Some(ms(40)))),
            "ready at {ready}"
        );
        assert_eq!(of(&open, "s2"), Some((Some(ms(25)), Some(ms(30)))));
        pass(&memory, &mut builder, bound);
        assert_eq!(memory.row(LedgerKind::Task, "8"), open, "ready at {ready}");
        pass(&memory, &mut builder, bound + 1);
        let ended = memory.row(LedgerKind::Task, "8");
        assert_eq!(ended, whole(bound + 1), "ready at {ready}");
        let s1 = (ready < 20).then_some((Some(ms(10.max(ready))), Some(ms(20))));
        assert_eq!(of(&ended, "s1"), s1, "ready at {ready}");
        assert_eq!(of(&ended, "s2"), Some((Some(ms(25)), Some(ms(30)))));
        let writes = memory.writes.get();
        pass(&memory, &mut builder, bound + 1);
        assert_eq!(memory.writes.get(), writes, "ready at {ready}");
    }
}

/// A task a pass folds at the very time it became ready (no time ready
/// yet, so no interval of it) is ready all the same: a wait of the queue
/// as a whole that comes in later, read by a pass at its own time or after
/// it, folds and writes its row again as the fold of every event then, and
/// so does that wait's end.
#[test]
fn a_task_folded_as_it_became_ready_follows_the_waits_after() {
    for at_the_hold in [true, false] {
        let mut log = Log::default();
        log.add(Some(8), None, "task_created", json!({"status": "draft"}), 0)
            .add(
                Some(8),
                None,
                "task_status_changed",
                json!({"from": "draft", "to": "ready"}),
                10,
            );
        let task = |memory: &Memory| memory.row(LedgerKind::Task, "8");
        let whole = |log: &Log, secs: i64| {
            crate::domain::ledger::build(&log.events, ms(secs), BUILT)
                .into_iter()
                .find(|row| row.kind == LedgerKind::Task && row.key == "8")
                .unwrap()
        };
        let memory = Memory::of(&log);
        let mut builder = Builder::default();
        pass(&memory, &mut builder, 10);
        let ready = task(&memory);
        assert_eq!(ready, whole(&log, 10));
        assert!(ready.intervals.iter().all(|i| i.phase != "ready"));
        assert_eq!(ready.detail["ready_since"], json!(ms(10)));

        log.add(None, None, "claim_held", json!({"supervisor": "s1"}), 20);
        memory.events.replace(log.events.clone());
        let held = if at_the_hold { 20 } else { 30 };
        pass(&memory, &mut builder, held);
        assert_eq!(task(&memory), whole(&log, held), "read at {held}");

        log.add(None, None, "claim_resumed", json!({"supervisor": "s1"}), 25);
        memory.events.replace(log.events.clone());
        pass(&memory, &mut builder, 40);
        let resumed = task(&memory);
        assert_eq!(resumed, whole(&log, 40), "read at {held}");
        let holds: Vec<(Option<i64>, Option<i64>)> = resumed.detail["overlays"]
            .as_array()
            .unwrap()
            .iter()
            .map(|overlay| (overlay["start"].as_i64(), overlay["end"].as_i64()))
            .collect();
        assert_eq!(holds, [(Some(ms(20)), Some(ms(25)))], "read at {held}");
    }
}

/// The row of task 8 that [`crate::domain::ledger::build`] folds of `log`
/// at `secs` after [`T0`].
fn whole(log: &Log, task: i64, secs: i64) -> LedgerRow {
    crate::domain::ledger::build(&log.events, ms(secs), BUILT)
        .into_iter()
        .find(|row| row.kind == LedgerKind::Task && row.key == task.to_string())
        .unwrap()
}

/// A task that left its time ready (back to a draft) while a supervisor's
/// slots full lay open on it, whose row a pass wrote then, is folded and
/// written again by the first pass past that supervisor's silence, though
/// no event came in: the overlay on its time ready past ends at the
/// supervisor's last event as the fold of every event then ends it.
#[test]
fn a_task_back_to_a_draft_follows_a_slots_full_its_supervisors_silence_ends() {
    use crate::domain::supervisor_life::SILENT_AFTER_MS;
    let mut log = Log::default();
    log.add(Some(8), None, "task_created", json!({"status": "draft"}), 0)
        .add(
            Some(8),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "ready"}),
            5,
        )
        .add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s2", "class": "heavy"}),
            10,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s2"}),
            20,
        )
        .add(
            Some(8),
            None,
            "task_status_changed",
            json!({"from": "ready", "to": "draft"}),
            30,
        );
    let bound = 20 + SILENT_AFTER_MS / 1000;
    let overlay = |row: &LedgerRow| {
        row.detail["overlays"]
            .as_array()
            .unwrap()
            .iter()
            .map(|overlay| (overlay["start"].as_i64(), overlay["end"].as_i64()))
            .collect::<Vec<_>>()
    };
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    pass(&memory, &mut builder, 40);
    let draft = memory.row(LedgerKind::Task, "8");
    assert_eq!(draft, whole(&log, 8, 40));
    assert_eq!(overlay(&draft), [(Some(ms(10)), Some(ms(30)))]);
    pass(&memory, &mut builder, bound);
    assert_eq!(memory.row(LedgerKind::Task, "8"), draft);
    pass(&memory, &mut builder, bound + 1);
    let silent = memory.row(LedgerKind::Task, "8");
    assert_eq!(silent, whole(&log, 8, bound + 1));
    assert_eq!(overlay(&silent), [(Some(ms(10)), Some(ms(20)))]);
}

/// Two builders, one behind the other on the events. A task left its time
/// ready under a supervisor's slots full (started on that time, or at its
/// very end) whose last record came by then, and the supervisor records
/// again past its silence. The builder that read that record writes the
/// row with the slots full open (to the end of the time ready, or of no
/// length on it). The other, folding the task in a pass past the silence
/// over the events before that record, ends the slots full at the record
/// before: on the time ready, it read less of the wait, so it does not
/// write over the row ([`crate::domain::ledger::builder::behind`]) and
/// keeps the task marked; at its end, its fold is the same row. Once it
/// reads that record, it folds the same row.
#[test]
fn a_builder_behind_on_a_wait_does_not_write_over_the_task_row_of_one_ahead() {
    let (held, behind) = two_builders_on_a_slots_full_from(10);
    assert_eq!(held.detail["overlays"][0]["end"], json!(ms(30)));
    assert!(held.inputs_through.life_ended.is_empty());
    assert!(behind.backlog > 0, "the task stays marked");
    let (held, _) = two_builders_on_a_slots_full_from(30);
    assert_eq!(held.detail["overlays"], json!([]));
    assert!(held.inputs_through.life_ended.is_empty());
}

/// [`a_builder_behind_on_a_wait_does_not_write_over_the_task_row_of_one_ahead`]
/// with the slots full started at `start` (seconds after [`T0`], by 30, when
/// the task leaves its time ready): the row the store holds after the pass
/// of the builder behind, the fold of every event, and that pass. The
/// builder behind folds it again after reading the later record and
/// leaves it as it is.
fn two_builders_on_a_slots_full_from(start: i64) -> (LedgerRow, Built) {
    use crate::domain::supervisor_life::SILENT_AFTER_MS;
    let slots_full = |log: &mut Log| {
        log.add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s", "class": "heavy"}),
            start,
        );
    };
    let mut log = Log::default();
    log.add(Some(8), None, "task_created", json!({"status": "draft"}), 0)
        .add(
            Some(8),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "ready"}),
            5,
        );
    if start < 20 {
        slots_full(&mut log);
    }
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s"}),
        20,
    )
    .add(
        Some(8),
        None,
        "task_status_changed",
        json!({"from": "ready", "to": "draft"}),
        30,
    );
    if start >= 20 {
        slots_full(&mut log);
    }
    let silent = start.max(20) + SILENT_AFTER_MS / 1000 + 1;
    let memory = Memory::of(&log);
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s"}),
        silent,
    );
    let later = log.events.clone();
    memory
        .before_steps
        .replace(Some(Box::new(move |memory: &Memory| {
            memory.events.replace(later);
            let ahead = build_pass(
                memory,
                &mut Builder::default(),
                ms(silent),
                BUILT,
                Retain::default(),
            );
            assert_eq!(ahead.failure, None);
        })));
    let mut behind = Builder::default();
    let built = pass(&memory, &mut behind, silent);
    assert_eq!(built.failure, None, "{start}");
    assert!(memory.before_steps.borrow().is_none(), "the other pass ran");
    let held = memory.row(LedgerKind::Task, "8");
    assert_eq!(held, whole(&log, 8, silent), "{start}");
    pass(&memory, &mut behind, silent);
    assert_eq!(memory.row(LedgerKind::Task, "8"), held, "{start}");
    (held, built)
}

/// Two builders over the same events, one's pass at a later time than the
/// other's. A supervisor's slots full lies on a task's time ready (ready
/// now, or past once the task went back to a draft) and the supervisor
/// records no more. The builder whose pass is past the silence closes that
/// wait and writes the task's row with it ended. The other, whose pass
/// began before the silence and writes after, folds it open over the same
/// events: folded at an earlier time, it does not write over the row
/// ([`crate::domain::ledger::builder::behind`]) and keeps the task marked,
/// as the builder ahead, which closed the wait, marks the task no more.
/// Once its own time passes the silence, it folds the same row.
#[test]
fn a_builder_at_an_earlier_time_does_not_write_over_the_task_row_of_one_past_a_silence() {
    use crate::domain::supervisor_life::SILENT_AFTER_MS;
    for back_to_draft in [false, true] {
        let mut log = Log::default();
        log.add(Some(8), None, "task_created", json!({"status": "draft"}), 0)
            .add(
                Some(8),
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
        if back_to_draft {
            log.add(
                Some(8),
                None,
                "task_status_changed",
                json!({"from": "ready", "to": "draft"}),
                30,
            );
        }
        // The last second s's life goes on.
        let bound = 20 + SILENT_AFTER_MS / 1000;
        let memory = Memory::of(&log);
        let mut ahead = Builder::default();
        assert_eq!(pass(&memory, &mut ahead, bound + 1).failure, None);
        let held = memory.row(LedgerKind::Task, "8");
        assert_eq!(held, whole(&log, 8, bound + 1), "{back_to_draft}");
        assert!(!held.inputs_through.life_ended.is_empty());
        let open = whole(&log, 8, bound);
        assert_eq!(open.inputs_through.events, held.inputs_through.events);
        assert!(open.inputs_through.life_ended.is_empty());
        let writes = memory.writes.get();
        let mut earlier = Builder::default();
        let built = pass(&memory, &mut earlier, bound);
        assert_eq!(built.failure, None);
        assert_eq!(memory.row(LedgerKind::Task, "8"), held, "{back_to_draft}");
        assert!(built.backlog > 0, "the task stays marked");
        // The builder ahead closed the wait: it marks the task no more.
        pass(&memory, &mut ahead, bound + 2);
        assert_eq!(ahead.backlog(), 0);
        let built = pass(&memory, &mut earlier, bound + 1);
        assert_eq!(built.backlog, 0);
        assert_eq!(memory.writes.get(), writes, "{back_to_draft}");
        assert_eq!(memory.row(LedgerKind::Task, "8"), held);
    }
}

/// A pass that took its time before it read the events: a supervisor's
/// slots full ended by its stop, and a task made ready after that stop,
/// both after the pass's clock. The pass folds at the time of the last
/// event it read ([`Builder::folds_at`]), so the task's time ready starts
/// at the move and the stop, before it, bears on none of it: the row is
/// the fold of every event then, and another builder's later pass folds
/// the same row, writes nothing and keeps no mark.
#[test]
fn a_pass_whose_clock_is_before_the_events_it_reads_folds_at_the_last() {
    let mut log = Log::default();
    log.add(Some(8), None, "task_created", json!({"status": "draft"}), 0)
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
            "supervisor_stopped",
            json!({"supervisor": "s"}),
            20,
        )
        .add(
            Some(8),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "ready"}),
            21,
        );
    let memory = Memory::of(&log);
    assert_eq!(pass(&memory, &mut Builder::default(), 19).failure, None);
    let held = memory.row(LedgerKind::Task, "8");
    assert_eq!(held, whole(&log, 8, 21));
    assert!(held.inputs_through.life_ended.is_empty());
    let writes = memory.writes.get();
    let built = pass(&memory, &mut Builder::default(), 22);
    assert_eq!(built.backlog, 0);
    assert_eq!(memory.writes.get(), writes);
    assert_eq!(memory.row(LedgerKind::Task, "8"), held);
}

/// A deterministic generator: a linear congruential one from a fixed seed.
pub(super) struct Lcg(pub(super) u64);

impl Lcg {
    /// The next of `0..n`.
    pub(super) fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

/// `row`, folded at a later time, as of the time `cut` (unix ms): its
/// intervals and overlays cut there, and their sums made again. A row not
/// ended whose inputs did not move since a pass at `cut` folds to this at
/// any later time: what grows is the time up to now only.
fn as_of(row: &LedgerRow, cut: i64) -> LedgerRow {
    let mut cut_row = row.clone();
    cut_row.intervals = row
        .intervals
        .iter()
        .filter(|interval| interval.start < cut)
        .map(|interval| {
            let mut interval = interval.clone();
            interval.end = interval.end.min(cut);
            interval
        })
        .collect();
    cut_row.totals = crate::domain::ledger::Totals::of(&cut_row.intervals);
    let overlays: Vec<Value> = row.detail["overlays"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|overlay| {
            let mut overlay = overlay.clone();
            let end = overlay["end"].as_i64()?.min(cut);
            (end > overlay["start"].as_i64()?).then(|| {
                overlay["end"] = json!(end);
                overlay
            })
        })
        .collect();
    let mut covered: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
    for overlay in &overlays {
        covered
            .entry(overlay["kind"].as_str().unwrap().to_owned())
            .or_default()
            .push((
                overlay["start"].as_i64().unwrap(),
                overlay["end"].as_i64().unwrap(),
            ));
    }
    let overlay_ms: BTreeMap<String, i64> = covered
        .into_iter()
        .map(|(kind, mut spans)| {
            spans.sort_unstable();
            let (mut total, mut reach) = (0, i64::MIN);
            for (start, end) in spans {
                total += (end - start.max(reach)).max(0);
                reach = reach.max(end);
            }
            (kind, total)
        })
        .collect();
    cut_row.detail["overlays"] = json!(overlays);
    cut_row.detail["overlay_ms"] = json!(overlay_ms);
    cut_row
}

/// What a generated history keeps of a supervisor.
#[derive(Default)]
struct Supervisor {
    stopped: bool,
    slots_full: bool,
    /// Its last event, in seconds after [`T0`].
    last: Option<i64>,
}

/// The passes of a generated history: two builders over one store, each
/// on its own clock, and what the checks keep of the passes before.
#[derive(Default)]
struct Passes {
    memory: Memory,
    builders: [Builder; 2],
    /// Each builder's last pass, in seconds after [`T0`].
    clocks: [i64; 2],
    /// The latest pass of either.
    latest: i64,
    /// Each task's stored row as last checked, and the time (unix ms) of
    /// the pass that wrote it.
    written: BTreeMap<i64, (LedgerRow, i64)>,
    /// The passes that found a row written as of an earlier pass.
    as_of_earlier: usize,
    /// The passes at an earlier time that found a row folded at a later
    /// one and kept its task marked.
    behind_in_time: usize,
}

impl Passes {
    /// A pass of builder `b` at `secs` (not before its last one) over
    /// every event of `log`, the last at `last_event`, and the checks of
    /// each task's stored row. It folds at no earlier time than that event
    /// ([`Builder::folds_at`]). At the latest time, its passes leave no
    /// mark ([`passes_to_drain`]), and the row is the fold of every event
    /// as of the pass that last wrote it ([`as_of`]): one at the pass's
    /// time when it wrote it, and one no
    /// input since it was written moved (the events, the waits' ends, a
    /// supervisor's silence) otherwise. At a time before the latest pass's
    /// (one that began before it and writes after it), the pass leaves the
    /// row as it was or writes the fold of every event at its time, and
    /// takes back none of the waits a silence ended over the same events.
    fn at(&mut self, log: &Log, b: usize, secs: i64, last_event: i64, seed: u64) {
        assert!(secs >= self.clocks[b]);
        self.clocks[b] = secs;
        self.memory.events.replace(log.events.clone());
        let tasks = [8, 9];
        let folds_at = secs.max(last_event);
        if folds_at >= self.latest {
            self.latest = folds_at;
            let built = passes_to_drain(&self.memory, &mut self.builders[b], secs);
            assert_eq!(
                (built.backlog, &built.failure),
                (0, &None),
                "seed {seed}: marks left at {secs}"
            );
            let secs = folds_at;
            for task in tasks {
                let stored = self.memory.row(LedgerKind::Task, &task.to_string());
                let cut = match self.written.get(&task) {
                    Some((before, at)) if *before == stored => *at,
                    _ => ms(secs),
                };
                if cut < ms(secs) {
                    self.as_of_earlier += 1;
                }
                let expected = as_of(&whole(log, task, secs), cut);
                assert_eq!(stored, expected, "seed {seed}: task {task} at {secs}");
                self.written.insert(task, (stored, cut));
            }
            return;
        }
        let before = tasks.map(|task| self.memory.row(LedgerKind::Task, &task.to_string()));
        // A task whose row was folded later stays marked each pass.
        let mut backlog = 0;
        for _ in 0..3 {
            backlog = pass(&self.memory, &mut self.builders[b], secs).backlog;
            if backlog == 0 {
                break;
            }
        }
        let secs = folds_at;
        for (task, before) in tasks.into_iter().zip(before) {
            let stored = self.memory.row(LedgerKind::Task, &task.to_string());
            let (held, read) = (&before.inputs_through, &stored.inputs_through);
            if stored == before {
                let later = whole(log, task, secs).inputs_through;
                if backlog > 0 && later != *held && later.events == held.events {
                    self.behind_in_time += 1;
                }
                continue;
            }
            let at = format!("seed {seed}: task {task} at {secs}, before {}", self.latest);
            assert_eq!(stored, whole(log, task, secs), "{at}");
            assert!(
                read.events > held.events
                    || held
                        .life_ended
                        .iter()
                        .all(|wait| read.life_ended.contains(wait)),
                "{at}: {held:?} taken back to {read:?}"
            );
            self.written.insert(task, (stored, ms(secs)));
        }
    }
}

/// One history from `seed`: two tasks (8 and 9) made drafts at 0 and
/// moved between `draft` and `ready`, two supervisors (`s1`, `s2`) whose
/// slots full start and end, that record their life and stop, holds on
/// the claims by either, and the passes of two builders over one store
/// ([`Passes::at`]): at the time of the last event, a few seconds later,
/// or exactly at and one second past a supervisor's silence, and those of
/// the builder behind in time at a time before the latest pass's, up to a
/// silence before (before the last event too, as a pass that took its time
/// before it read it). Returns how many passes found a
/// row written as of an earlier pass, and how many at an earlier time found
/// one folded at a later time.
fn a_history_of(seed: u64) -> (usize, usize) {
    use crate::domain::supervisor_life::SILENT_AFTER_MS;
    let silence = SILENT_AFTER_MS / 1000;
    let mut rng = Lcg(seed);
    let mut log = Log::default();
    log.add(Some(8), None, "task_created", json!({"status": "draft"}), 0)
        .add(Some(9), None, "task_created", json!({"status": "draft"}), 0);
    let mut ready = [false, false];
    let names = ["s1", "s2"];
    let mut supervisors = [Supervisor::default(), Supervisor::default()];
    let mut held = false;
    let mut t = 0;
    let mut last_event = 0;
    let mut passes = Passes::default();
    for _ in 0..40 {
        let s = usize::try_from(rng.below(2)).unwrap();
        let name = names[s];
        let b = usize::try_from(rng.below(2)).unwrap();
        let events = log.events.len();
        match rng.below(9) {
            0 => {
                let task = usize::try_from(rng.below(2)).unwrap();
                let (from, to) = if ready[task] {
                    ("ready", "draft")
                } else {
                    ("draft", "ready")
                };
                ready[task] = !ready[task];
                let id = i64::try_from(8 + task).unwrap();
                log.add(
                    Some(id),
                    None,
                    "task_status_changed",
                    json!({"from": from, "to": to}),
                    t,
                );
            }
            1 if !supervisors[s].stopped => {
                let kind = if supervisors[s].slots_full {
                    "slots_full_ended"
                } else {
                    "slots_full_started"
                };
                supervisors[s].slots_full = !supervisors[s].slots_full;
                supervisors[s].last = Some(t);
                log.add(
                    None,
                    None,
                    kind,
                    json!({"supervisor": name, "class": "heavy"}),
                    t,
                );
            }
            2 if !supervisors[s].stopped => {
                supervisors[s].last = Some(t);
                log.add(
                    None,
                    None,
                    "supervisor_alive",
                    json!({"supervisor": name}),
                    t,
                );
            }
            3 if !supervisors[s].stopped => {
                supervisors[s].stopped = true;
                supervisors[s].last = Some(t);
                log.add(
                    None,
                    None,
                    "supervisor_stopped",
                    json!({"supervisor": name}),
                    t,
                );
            }
            4 if !supervisors[s].stopped => {
                let kind = if held { "claim_resumed" } else { "claim_held" };
                held = !held;
                supervisors[s].last = Some(t);
                log.add(None, None, kind, json!({"supervisor": name}), t);
            }
            5 => passes.at(&log, b, t, last_event, seed),
            6 => t += i64::try_from(1 + rng.below(12)).unwrap(),
            7 => {
                if let Some(last) = supervisors[s].last.filter(|_| !supervisors[s].stopped) {
                    t = t.max(last + silence + i64::try_from(rng.below(2)).unwrap());
                }
                passes.at(&log, b, t, last_event, seed);
            }
            8 => {
                // The builder behind in time: its pass began before the
                // latest one and writes after it.
                let b = usize::from(passes.clocks[1] < passes.clocks[0]);
                let back =
                    i64::try_from(1 + rng.below(u64::try_from(silence + 1).unwrap())).unwrap();
                let secs = (passes.latest - back).max(passes.clocks[b]);
                passes.at(&log, b, secs, last_event, seed);
            }
            _ => {}
        }
        if log.events.len() > events {
            last_event = t;
        }
    }
    for b in [0, 1] {
        passes.at(&log, b, t, last_event, seed);
    }
    for b in [0, 1] {
        passes.at(&log, b, t + silence + 1, last_event, seed);
    }
    (passes.as_of_earlier, passes.behind_in_time)
}

/// The histories of many fixed seeds ([`a_history_of`]): tasks moving
/// between `draft` and `ready` under two supervisors' slots full (started
/// and ended), life, stops and holds, and two builders' passes at the time
/// of an event and at a supervisor's silence, its bound included, and at a
/// time before the latest pass's. Every pass at the latest time leaves each
/// task's row the fold of every event read so far, as of the pass that
/// last wrote it, and none at an earlier time takes a row back; some passes
/// find a row as of an earlier one, which the check reads through, and
/// some at an earlier time find one folded at a later time.
#[test]
fn every_history_leaves_the_tasks_rows_the_fold_of_every_event() {
    let (as_of_earlier, behind_in_time) = (0..400)
        .map(a_history_of)
        .fold((0, 0), |(a, b), (c, d)| (a + c, b + d));
    assert!(as_of_earlier > 0);
    assert!(behind_in_time > 0);
}
