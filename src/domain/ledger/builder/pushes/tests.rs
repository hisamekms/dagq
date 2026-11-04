use serde_json::json;

use super::*;
use crate::domain::ledger::testkit::{Log, pushes_across, run_id};
use crate::domain::run_phase::Phase;

/// The summaries made again of the push records cut, up to any position,
/// are those [`Kept`] made of every event up to it.
#[test]
fn the_summaries_of_the_push_records_up_to_a_position_are_those_of_every_event() {
    let (log, _) = pushes_across(2);
    let cut: Vec<RunEvent> = log
        .events
        .iter()
        .filter(|event| is_push_record(event))
        .map(cut)
        .collect();
    assert!(cut.len() < log.events.len());
    let mut closed = 0;
    for at in 0..=log.events.len() {
        let through = EventId::new(i64::try_from(at).unwrap());
        let kept = Kept::of(&log.events[..at]);
        let held = PushesHeld::at(cut.clone(), through);
        assert_eq!(held, kept.pushes_held(), "at {at}");
        closed = closed.max(held.closing.len());
    }
    // Two settled ones, run 1 and run 5 at the end.
    assert_eq!(closed, 4);
    let end = PushesHeld::at(cut, log.last());
    // Runs 3 and 4.
    assert_eq!(end.waiting_push.len(), 2);
    assert_eq!(end.failed_push.len(), 7);
}

/// What a push and a phase keep of their payloads is what the summaries
/// read; a push of no run is a push record, and an event of another kind
/// is not, nor a failed push or a phase of no run.
#[test]
fn a_cut_keeps_where_a_push_went_and_a_phase_name() {
    let mut log = Log::default();
    let run = run_id(1);
    log.add(
        Some(1),
        Some(&run),
        "push_failed",
        json!({"remote": "origin", "branch": "main", "error": "long"}),
        0,
    )
    .phase(1, &run, 0, Phase::PushPending, "push_failed")
    .add(Some(1), None, "push_finished", json!({}), 1)
    .add(Some(1), Some(&run), "note_added", json!({}), 2)
    .add(Some(1), None, "push_failed", json!({}), 3);
    let [failed, phase, of_no_run, note, failed_of_no_run] = &log.events[..] else {
        unreachable!()
    };
    assert_eq!(
        cut(failed).payload,
        json!({"remote": "origin", "branch": "main"})
    );
    assert_eq!(cut(phase).payload, json!({"phase": "push_pending"}));
    assert_eq!(PushedTo::of(&cut(failed)), PushedTo::of(failed));
    assert!(is_push_record(failed) && is_push_record(phase) && is_push_record(of_no_run));
    assert!(!is_push_record(note) && !is_push_record(failed_of_no_run));
}

/// A row is settled at a position only when settled by today's fold and
/// no input it folded came after it.
#[test]
fn a_row_is_settled_at_a_position_only_with_no_input_after_it() {
    let head = LedgerHead {
        kind: LedgerKind::Run,
        key: run_id(1),
        inputs_from: EventId::new(1),
        inputs_through: crate::domain::ledger::InputsThrough {
            events: EventId::new(5),
            supervisor: None,
            life_ended: Vec::new(),
            node_samples: None,
            steps: None,
            resources: None,
            inside_resources: None,
        },
        is_final: true,
        ledger_v: LEDGER_V,
        records: vec![EventId::new(1), EventId::new(4)],
    };
    assert!(settled_at(&head, false, EventId::new(5), 0));
    assert!(!settled_at(&head, false, EventId::new(4), 0));
    // A legacy row's mark, as first written, says nothing of it.
    assert!(!settled_at(&head, true, EventId::new(5), 0));
    assert!(reaches_past(&head, EventId::new(4), 0) && !reaches_past(&head, EventId::new(5), 0));
    let open = LedgerHead {
        is_final: false,
        ..head.clone()
    };
    assert!(!settled_at(&open, false, EventId::new(9), 0));
    let older = LedgerHead {
        ledger_v: LEDGER_V - 1,
        ..head.clone()
    };
    assert!(!settled_at(&older, false, EventId::new(9), 0));
    let mut waited = head.clone();
    waited.inputs_through.supervisor = Some(EventId::new(7));
    assert!(
        !settled_at(&waited, false, EventId::new(6), 0)
            && settled_at(&waited, false, EventId::new(7), 0)
    );
    // A window of its resources after the cursor of the windows reaches
    // past it: its run's settling waits on the last one.
    let mut measured = head.clone();
    measured.inputs_through.resources = Some(3);
    assert!(reaches_past(&measured, EventId::new(5), 2));
    assert!(
        !settled_at(&measured, false, EventId::new(5), 2)
            && settled_at(&measured, false, EventId::new(5), 3)
    );
    assert_eq!(target_of(&head), Some(Target::Run(run_id(1))));
    let task = LedgerHead {
        kind: LedgerKind::Task,
        key: "12".into(),
        ..head
    };
    assert_eq!(target_of(&task), Some(Target::Task(TaskId::new(12))));
}

/// A task row names the runs of today's fold or, a legacy one, those of its
/// values; another kind of row, none.
#[test]
fn a_task_row_names_its_runs_of_its_detail_or_its_legacy_values() {
    let (old, new) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.worked(1, &new, 0);
    let rows = crate::domain::ledger::build(&log.events, 1, "");
    let of =
        |key: &str, rows: &[LedgerRow]| rows.iter().find(|row| row.key == key).unwrap().clone();
    assert_eq!(task_runs(&of("1", &rows)), [new.as_str()]);
    assert!(task_runs(&of(&new, &rows)).is_empty());
    log.legacy(1, &old, 1000);
    let rows = crate::domain::ledger::build(&log.events, 1, "");
    let task = of("1", &rows);
    assert!(task.is_legacy());
    assert_eq!(task_runs(&task), [new.as_str(), old.as_str()]);
}

/// Whether a legacy row was settled at a position is its fold up to it, as
/// the build writes it: a task with a legacy run among its events is
/// settled whatever a later input or the version the store keeps it at;
/// up to before that run ended it is a task of today's form. A row of an
/// older version, not legacy, is not settled.
#[test]
fn a_legacy_row_is_settled_by_its_fold_up_to_the_position() {
    let (old, new) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.legacy(1, &old, 0);
    let ended = log.last();
    log.worked(1, &new, 2000);
    let now = 1_000_000_000_000_000;
    let rows = crate::domain::ledger::build(&log.events, now, "");
    let task = rows
        .iter()
        .find(|row| row.kind == LedgerKind::Task)
        .unwrap()
        .clone();
    assert!(task.is_legacy());
    let held = |through: EventId| PushesHeld {
        through,
        ..PushesHeld::default()
    };
    let of_an_older_version = LedgerRow {
        ledger_v: LEDGER_V - 1,
        ..task.clone()
    };
    assert!(held(ended).settled_folded(&task, log.events.clone(), &[], now));
    assert!(held(ended).settled_folded(&of_an_older_version, log.events.clone(), &[], now));
    let before = EventId::new(ended.as_i64() - 2);
    assert!(!held(before).settled_folded(&task, log.events.clone(), &[], now));
    let today = LedgerRow {
        legacy: None,
        ..of_an_older_version
    };
    assert!(!held(ended).settled_folded(&today, log.events, &[], now));
}
