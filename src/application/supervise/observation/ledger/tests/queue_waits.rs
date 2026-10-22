//! The queue rows of the waits before a claim (a supervisor's slots full,
//! a hold on the claims) as their supervisors' lives end after a pass
//! wrote them, by a stop or by a silence that no event records: after each
//! pass at the latest time the stored rows are the fold of every event
//! read so far.

use super::waits::Lcg;
use super::*;

/// The queue rows [`crate::domain::ledger::build`] folds of `log` at
/// `secs` after [`T0`], by key.
fn whole_queue(log: &Log, secs: i64) -> BTreeMap<String, LedgerRow> {
    crate::domain::ledger::build(&log.events, ms(secs), BUILT)
        .into_iter()
        .filter(|row| row.kind == LedgerKind::Queue)
        .map(|row| (row.key.clone(), row))
        .collect()
}

/// The queue rows the store holds, by key.
fn stored_queue(memory: &Memory) -> BTreeMap<String, LedgerRow> {
    memory
        .rows
        .borrow()
        .iter()
        .filter(|((kind, _), _)| *kind == LedgerKind::Queue)
        .map(|((_, key), row)| (key.clone(), row.clone()))
        .collect()
}

/// A slots full whose supervisor falls silent, written open by a pass
/// before the silence, is ended by the first pass past it though no event
/// came in, as the fold of every event then ends it, and settled by the
/// first pass past its grace.
#[test]
fn a_slots_full_its_supervisors_silence_ends_is_ended_and_settled() {
    use crate::domain::supervisor_life::SILENT_AFTER_MS;
    let silence = SILENT_AFTER_MS / 1000;
    let mut log = Log::default();
    log.add(
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
    assert!(!ended.is_final);
    drain(&memory, &mut builder, 20 + SETTLE_GRACE_MS / 1000);
    expected.is_final = true;
    assert_eq!(memory.row(LedgerKind::Queue, &start), expected);
}

/// What a generated history keeps of a supervisor.
#[derive(Default)]
struct Supervisor {
    stopped: bool,
    slots_full: bool,
    /// Its last event, in seconds after [`T0`].
    last: Option<i64>,
}

/// A pass at `secs` (not before the last one) over every event of `log`,
/// the last at `last_event`, until it leaves no mark: after it each queue
/// row the store holds is the fold of every event at the time it folds at
/// ([`Builder::folds_at`]), and it holds every one; one is settled once it
/// ended and its grace passed, and only then (`settled`: every one ended
/// is). The keys of the slots full a supervisor's life ended.
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
            row.ended_at.is_some()
                && row
                    .subject
                    .as_ref()
                    .is_some_and(|subject| subject.kind == "slots_full")
                && row.inputs_through.supervisor.is_some()
        })
        .map(|(key, _)| key)
        .collect()
}

/// One history from `seed`: two supervisors (`s1`, `s2`) whose slots full
/// start and end, that record their life, stop and hold the claims, and
/// the passes of one builder: at the time of the last event, a few
/// seconds later, exactly at and one second past a supervisor's silence,
/// and past the grace, after which every row ended is settled. Whether a
/// pass found a slots full its supervisor's life ended, and whether a later
/// one found one of those open again (an event of its supervisor came).
fn a_history_of(seed: u64) -> (bool, bool) {
    use crate::domain::supervisor_life::SILENT_AFTER_MS;
    let silence = SILENT_AFTER_MS / 1000;
    let grace = SETTLE_GRACE_MS / 1000;
    let mut rng = Lcg(seed);
    let mut log = Log::default();
    let names = ["s1", "s2"];
    let mut supervisors = [Supervisor::default(), Supervisor::default()];
    let mut held = false;
    let (mut t, mut last_event, mut clock) = (0, 0, 0);
    let memory = Memory::default();
    let mut builder = Builder::default();
    let (mut life_ended, mut reopened) = (false, false);
    let mut ended = BTreeSet::new();
    for _ in 0..40 {
        let s = usize::try_from(rng.below(2)).unwrap();
        let name = names[s];
        let events = log.events.len();
        match rng.below(8) {
            0 | 1 if !supervisors[s].stopped => {
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
                let kind = if held { "claim_resumed" } else { "claim_held" };
                held = !held;
                supervisors[s].last = Some(t);
                log.add(None, None, kind, json!({"supervisor": name}), t);
            }
            5 => {
                clock = clock.max(t);
                let now_ended = checked_pass(
                    &memory,
                    &mut builder,
                    &log,
                    (clock, last_event),
                    false,
                    seed,
                );
                reopened |= ended.difference(&now_ended).next().is_some();
                life_ended |= !now_ended.is_empty();
                ended = now_ended;
            }
            6 => t += i64::try_from(1 + rng.below(12)).unwrap(),
            7 => {
                if let Some(last) = supervisors[s].last.filter(|_| !supervisors[s].stopped) {
                    t = t.max(last + silence + i64::try_from(rng.below(2)).unwrap());
                }
                clock = clock.max(t);
                let now_ended = checked_pass(
                    &memory,
                    &mut builder,
                    &log,
                    (clock, last_event),
                    false,
                    seed,
                );
                reopened |= ended.difference(&now_ended).next().is_some();
                life_ended |= !now_ended.is_empty();
                ended = now_ended;
            }
            _ => {}
        }
        if log.events.len() > events {
            last_event = t;
        }
    }
    for secs in [t, t + silence, t + silence + 1, t + silence + grace + 1] {
        clock = clock.max(secs);
        let settled = secs > t + silence + grace;
        let now_ended = checked_pass(
            &memory,
            &mut builder,
            &log,
            (clock, last_event),
            settled,
            seed,
        );
        reopened |= ended.difference(&now_ended).next().is_some();
        life_ended |= !now_ended.is_empty();
        ended = now_ended;
    }
    (life_ended, reopened)
}

/// The histories of many fixed seeds ([`a_history_of`]): two supervisors'
/// slots full (started and ended), life, stops and holds, and a builder's
/// passes at the time of an event, at a supervisor's silence and one
/// second past it, and past the grace. Every pass leaves each queue row
/// the fold of every event read so far and no mark; some find a slots
/// full its supervisor's life ended, and some find one open again.
#[test]
fn every_history_leaves_the_queue_rows_the_fold_of_every_event() {
    let histories: Vec<(bool, bool)> = (0..400).map(a_history_of).collect();
    assert!(histories.iter().any(|(life_ended, _)| *life_ended));
    assert!(histories.iter().any(|(_, reopened)| *reopened));
}
