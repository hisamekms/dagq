//! The check of the slots' arithmetic (`details.slot_check`): the runs held
//! at once from the slots' hold beside the landings per hour times the
//! slot time of a landing from the landed runs' phases.

use super::*;

/// Monday's `slot_check` of a queue with a supervisor of two slots alive
/// all day and the runs `runs` add.
fn monday_slot_check(runs: impl FnOnce(&mut Queue)) -> Value {
    let mut queue = Queue::default();
    start(&mut queue, 2, MONDAY);
    queue.heartbeats.insert("s".to_owned(), MONDAY + DAY - 1);
    runs(&mut queue);
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    result.periods.last().unwrap().window.details["slot_check"].clone()
}

/// Four runs of an hour each from claim to landing: four hours held in a
/// day is 1/6 of a run at once, and 4 landings a day of 3600 s each is
/// the same, so the ratio is 1.
#[test]
fn steady_runs_hold_as_many_slots_as_their_landings_say() {
    let check = monday_slot_check(|queue| {
        for task in 1..=4 {
            queue.run(&Run::new(task, None, MONDAY + task * 2 * HOUR, HOUR - 100));
        }
    });
    assert_eq!(check["occupied"], json!(round3(4.0 / 24.0)));
    assert_eq!(check["landings_per_hour"], json!(round3(4.0 / 24.0)));
    assert_eq!(check["slot_secs_per_landing"], json!(3600.0));
    assert_eq!(check["expected"], json!(round3(4.0 / 24.0)));
    assert_eq!(check["ratio"], json!(1.0));
    assert!(check.get("reason").is_none());
}

/// A run whose end was never recorded holds its slot up to now on the
/// slots' side only: the landed runs' phases do not see it, so the ratio
/// moves off 1.
#[test]
fn a_run_without_its_end_holds_a_slot_the_landings_do_not_see() {
    let check = monday_slot_check(|queue| {
        queue.run(&Run::new(1, None, MONDAY + HOUR, HOUR - 100));
        queue.goals.insert(TaskId::new(2), None);
        let id = format!("{:08x}-0000-4000-8000-{:012x}", 2, MONDAY + 12 * HOUR);
        queue.push(
            Some(2),
            Some(&id),
            "run_claimed",
            json!({"parallel": 2, "slots": 1}),
            MONDAY + 12 * HOUR,
        );
    });
    // One hour landed, twelve more held by the run with no end.
    assert_eq!(check["occupied"], json!(round3(13.0 / 24.0)));
    assert_eq!(check["expected"], json!(round3(1.0 / 24.0)));
    assert_eq!(check["ratio"], json!(13.0));
}

/// A run that left its slot to wait for a person holds none while it
/// waits, and its slot time per landing leaves the wait out too, so the
/// two sides still agree; its phases alone would read the wait as slot
/// time.
#[test]
fn a_person_s_wait_leaves_both_sides_of_the_check() {
    let check = monday_slot_check(|queue| {
        let run = Run::new(1, None, MONDAY + HOUR, 3 * HOUR - 100);
        queue.run(&run);
        let id = run_id(&run).to_string();
        queue.push(
            Some(1),
            Some(&id),
            "run_waiting_started",
            json!({}),
            MONDAY + 2 * HOUR,
        );
        queue.push(
            Some(1),
            Some(&id),
            "run_slot_regained",
            json!({}),
            MONDAY + 4 * HOUR,
        );
    });
    // Three hours from claim to landing, two of them waiting.
    assert_eq!(check["slot_secs_per_landing"], json!(3600.0));
    assert_eq!(check["occupied"], json!(round3(1.0 / 24.0)));
    assert_eq!(check["ratio"], json!(1.0));
}

/// No ratio without a landing, nor without a live supervisor, and the
/// reason says which.
#[test]
fn the_check_has_no_ratio_without_landings_or_a_supervisor() {
    let none = monday_slot_check(|_| {});
    assert_eq!(none["occupied"], json!(0.0));
    assert_eq!(none["landings_per_hour"], json!(0.0));
    assert_eq!(none["ratio"], Value::Null);
    assert_eq!(none["reason"], json!("no_landings"));

    let mut queue = Queue::default();
    start(&mut queue, 2, MONDAY);
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    // The day before the queue: no supervisor was alive.
    let before = &result.periods[0].window.details["slot_check"];
    assert_eq!(before["occupied"], Value::Null);
    assert_eq!(before["ratio"], Value::Null);
    assert_eq!(before["reason"], json!("no_supervisor"));
}
