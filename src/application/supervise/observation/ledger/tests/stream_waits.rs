//! The queue rows of the supervisors' streams of the record of the
//! judgments of now (`live_alert_stream`) as their supervisors' lives end
//! after a pass wrote them, by a stop or by a silence that no event
//! records, whether or not the supervisor has a slots full open: after
//! each pass at the latest time the stored rows are the fold of every
//! event read so far.

use super::queue_waits::{stored_queue, whole_queue};
use super::waits::Lcg;
use super::*;

/// A stream whose supervisor falls silent with no slots full open,
/// written open by a pass before the silence, is ended by the first pass
/// past it though no event came in, as the fold of every event then ends
/// it, and settled by the first pass past its grace; a later event of its
/// supervisor that is no record of it opens it again.
#[test]
fn a_stream_its_supervisors_silence_ends_is_ended_and_settled() {
    use crate::domain::supervisor_life::SILENT_AFTER_MS;
    let silence = SILENT_AFTER_MS / 1000;
    let mut log = Log::default();
    log.add(
        None,
        None,
        "live_alert_baseline",
        json!({"supervisor": "s", "version": 1}),
        10,
    )
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s"}),
        20,
    );
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    drain(&memory, &mut builder, 30);
    assert_eq!(stored_queue(&memory), whole_queue(&log, 30));
    let start = log.events[0].id.to_string();
    assert!(memory.row(LedgerKind::Queue, &start).ended_at.is_none());
    let past = 20 + silence + 1;
    drain(&memory, &mut builder, past);
    let ended = memory.row(LedgerKind::Queue, &start);
    let mut expected = whole_queue(&log, past).remove(&start).unwrap();
    assert_eq!(ended, expected);
    assert_eq!(ended.ended_at, Some(ms(20)));
    assert_eq!(ended.outcome.as_deref(), Some("supervisor_gone"));
    assert!(!ended.is_final);
    assert!(!builder.marked());
    drain(&memory, &mut builder, 20 + SETTLE_GRACE_MS / 1000);
    expected.is_final = true;
    assert_eq!(memory.row(LedgerKind::Queue, &start), expected);
    // An event of its supervisor that is no record of it opens it again.
    let back = 20 + SETTLE_GRACE_MS / 1000 + 1;
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s"}),
        back,
    );
    memory.events.replace(log.events.clone());
    drain(&memory, &mut builder, back);
    let open = memory.row(LedgerKind::Queue, &start);
    assert_eq!(open, whole_queue(&log, back).remove(&start).unwrap());
    assert!(open.ended_at.is_none() && !open.is_final);
}

/// What a generated history keeps of a supervisor.
#[derive(Default)]
struct Supervisor {
    stopped: bool,
    slots_full: bool,
    version: u64,
    /// Its last event, in seconds after [`T0`].
    last: Option<i64>,
}

/// A pass at `secs` (not before the last one) over every event of `log`,
/// the last at `last_event`, until it leaves no mark: after it each queue
/// row the store holds is the fold of every event at the time it folds at
/// ([`Builder::folds_at`]), and it holds every one; one is settled once it
/// ended and its grace passed, and only then (`settled`: every one ended
/// is). The keys of the streams a supervisor's life ended.
fn checked_pass(
    memory: &Memory,
    builder: &mut Builder,
    log: &Log,
    (secs, last_event): (i64, i64),
    settled: bool,
    seed: u64,
) -> BTreeSet<String> {
    memory.events.replace(log.events.clone());
    drain(memory, builder, secs);
    assert!(!builder.marked(), "seed {seed}: marks left at {secs}");
    let now = secs.max(last_event);
    let stored = stored_queue(memory);
    let mut expected = whole_queue(log, now);
    for (key, row) in &mut expected {
        let past_grace = row
            .ended_at
            .is_some_and(|end| ms(now) - end >= SETTLE_GRACE_MS);
        let is_final = stored.get(key).is_some_and(|stored| stored.is_final);
        assert!(
            if settled {
                is_final == row.ended_at.is_some()
            } else {
                !is_final || past_grace
            },
            "seed {seed}: row {key} at {secs}, final {is_final}"
        );
        row.is_final = is_final;
    }
    assert_eq!(stored, expected, "seed {seed}: at {secs}");
    expected
        .into_iter()
        .filter(|(_, row)| {
            row.outcome.as_deref() == Some("supervisor_gone")
                && row
                    .subject
                    .as_ref()
                    .is_some_and(|subject| subject.kind == "live_alert_stream")
        })
        .map(|(key, _)| key)
        .collect()
}

/// One history from `seed`: two supervisors (`s1`, `s2`) that record
/// baselines and changed inputs of their streams, their life, stops and
/// slots full (so a stream ends with a slots full open or with none), and
/// the passes of one builder: at the time of the last event, a few
/// seconds later, exactly at and one second past a supervisor's silence,
/// and past the grace, after which every row ended is settled. Whether a
/// pass found a stream its supervisor's life ended with no slots full
/// open, and whether a later one found one of those open again (an event
/// of its supervisor came; not one a baseline replaced).
fn a_history_of(seed: u64) -> (bool, bool) {
    use crate::domain::supervisor_life::SILENT_AFTER_MS;
    let silence = SILENT_AFTER_MS / 1000;
    let grace = SETTLE_GRACE_MS / 1000;
    let mut rng = Lcg(seed);
    let mut log = Log::default();
    let names = ["s1", "s2"];
    let mut supervisors = [Supervisor::default(), Supervisor::default()];
    let (mut t, mut last_event, mut clock) = (0, 0, 0);
    let memory = Memory::default();
    let mut builder = Builder::default();
    let (mut life_ended, mut reopened) = (false, false);
    let mut ended = BTreeSet::new();
    let mut check = |log: &Log, clock: i64, last_event: i64, settled: bool, slots: [bool; 2]| {
        let now_ended = checked_pass(
            &memory,
            &mut builder,
            log,
            (clock, last_event),
            settled,
            seed,
        );
        let rows = whole_queue(log, clock.max(last_event));
        // Open again, not replaced by a baseline: only an event naming its
        // supervisor that is no record of it makes the stored row dirty.
        reopened |= ended.iter().any(|key| rows[key].ended_at.is_none());
        life_ended |= now_ended.iter().any(|key| {
            let row = &rows[key];
            let about = row.subject.as_ref().map(|subject| subject.id.as_str());
            names
                .iter()
                .zip(slots)
                .any(|(name, slots_full)| Some(*name) == about && !slots_full)
        });
        ended = now_ended;
    };
    for _ in 0..40 {
        let s = usize::try_from(rng.below(2)).unwrap();
        let name = names[s];
        let events = log.events.len();
        match rng.below(9) {
            0 | 1 if !supervisors[s].stopped => {
                supervisors[s].version += 1;
                let kind = if rng.below(3) == 0 {
                    "live_alert_baseline"
                } else {
                    "live_alert_input_changed"
                };
                supervisors[s].last = Some(t);
                log.add(
                    None,
                    None,
                    kind,
                    json!({"supervisor": name, "version": supervisors[s].version}),
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
            3 if !supervisors[s].stopped && rng.below(3) == 0 => {
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
            5 => {
                clock = clock.max(t);
                let slots = supervisors.each_ref().map(|s| s.slots_full);
                check(&log, clock, last_event, false, slots);
            }
            6 => t += i64::try_from(1 + rng.below(12)).unwrap(),
            7 => {
                if let Some(last) = supervisors[s].last.filter(|_| !supervisors[s].stopped) {
                    t = t.max(last + silence + i64::try_from(rng.below(2)).unwrap());
                }
                clock = clock.max(t);
                let slots = supervisors.each_ref().map(|s| s.slots_full);
                check(&log, clock, last_event, false, slots);
            }
            _ => {}
        }
        if log.events.len() > events {
            last_event = t;
        }
    }
    let slots = supervisors.each_ref().map(|s| s.slots_full);
    for secs in [t, t + silence, t + silence + 1, t + silence + grace + 1] {
        clock = clock.max(secs);
        let settled = secs > t + silence + grace;
        check(&log, clock, last_event, settled, slots);
    }
    (life_ended, reopened)
}

/// The histories of many fixed seeds ([`a_history_of`]): two supervisors'
/// streams (baselines and changed inputs), life, stops and slots full, and
/// a builder's passes at the time of an event, at a supervisor's silence
/// and one second past it, and past the grace. Every pass leaves each
/// queue row the fold of every event read so far and no mark; some find a
/// stream its supervisor's life ended with no slots full open, and some
/// find one open again.
#[test]
fn every_history_leaves_the_stream_rows_the_fold_of_every_event() {
    let histories: Vec<(bool, bool)> = (0..400).map(a_history_of).collect();
    assert!(histories.iter().any(|(life_ended, _)| *life_ended));
    assert!(histories.iter().any(|(_, reopened)| *reopened));
}
