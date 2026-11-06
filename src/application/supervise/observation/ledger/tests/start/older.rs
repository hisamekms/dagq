//! A builder gone on from R on a store with rows of an older version: it
//! folds them again by target, and no cursor moves back for them. With no
//! R it reads from its cursors' start whatever the rows' versions.

use super::*;

/// Make every row `memory` holds but a legacy one of an older version,
/// and stale ([`older_and_stale`]).
fn every_row_older(memory: &Memory) {
    let keys: Vec<(LedgerKind, String)> = memory
        .rows
        .borrow()
        .iter()
        .filter(|(_, row)| !row.is_legacy())
        .map(|(key, _)| key.clone())
        .collect();
    for key in &keys {
        older_and_stale(memory, key);
    }
}

/// How far a stale time is from the fold's: past the times of the events
/// of the histories, so that a late event reads otherwise by it.
const STALE_MS: i64 = 3_600_000;

/// Make the row of `key` of an older version, every value of it an older
/// fold could have folded otherwise made so; its records and what it read
/// past them stay, being its inputs and not what they fold to, and so does
/// that a supervisor's life ended it, which the lives read
/// ([`crate::domain::pre_claim::rows::living`]). A landing slot's through is
/// one of those inputs here.
fn older_and_stale(memory: &Memory, key: &(LedgerKind, String)) {
    let mut rows = memory.rows.borrow_mut();
    let row = rows.get_mut(key).unwrap();
    row.ledger_v = LEDGER_V - 1;
    "older".clone_into(&mut row.built_at);
    if key.0 != LedgerKind::Queue {
        row.detail["older"] = json!(true);
        return;
    }
    row.started_at = row.started_at.map(|at| at - STALE_MS);
    row.ended_at = row.ended_at.map(|at| at + STALE_MS);
    // One a supervisor's life ended says so still: the lives read that.
    if row.outcome.as_deref() != Some(crate::domain::claim_hold::ENDED_SUPERVISOR_GONE) {
        row.outcome = row.outcome.as_ref().map(|_| "older".to_owned());
    }
    let kind = row
        .subject
        .as_ref()
        .map(|s| s.kind.clone())
        .unwrap_or_default();
    if let Some(subject) = row.subject.as_mut() {
        "older".clone_into(&mut subject.id);
    }
    match kind.as_str() {
        "live_alert_stream" => {
            let baseline = row.detail["baseline"].as_bool().unwrap_or(false);
            row.detail["baseline"] = json!(!baseline);
            row.detail["version"] = json!(99);
        }
        "slots_full" | "claim_hold" | "claim_deferral" | "landing_hold" => {
            let interval = &mut row.detail["interval"];
            interval["reason"] = json!("older");
            interval["started_at"] = json!("2000-01-01T00:00:00.000Z");
            if !interval["ended_at"].is_null() {
                interval["ended_at"] = json!("2099-01-01T00:00:00.000Z");
                interval["ended_by"] = json!("older");
            }
            if !interval["task_id"].is_null() {
                interval["task_id"] = json!(999_999);
            }
            if !interval["supervisor"].is_null() {
                interval["supervisor"] = json!("older");
            }
            if interval["class"] == json!("heavy") {
                interval["class"] = json!("light");
            }
        }
        _ => {
            row.detail = json!({"older": true});
        }
    }
}

/// The histories of [`histories`], and one of the queue rows a start need
/// not read a head of at this version.
fn histories_and_forgotten() -> Vec<(&'static str, Log, Vec<NodeSample>, i64)> {
    let mut all = histories();
    all.push(("forgotten", forgotten_log(2), Vec::new(), LATER));
    all
}

/// Every read of an input after a cursor since `memory`'s scans were
/// cleared read after that cursor's position in `r`.
#[track_caller]
fn scans_read_after(memory: &Memory, r: Cursors) {
    for (cursor, after, _) in memory.scans.borrow().iter() {
        let from = match *cursor {
            "events" => r.events.as_i64(),
            "samples" => r.samples,
            "steps" => r.steps,
            "resources" => r.resources,
            "inside_resources" => r.inside_resources,
            other => panic!("a scan of {other}"),
        };
        assert!(*after >= from, "{cursor} read after {after}, R at {from}");
    }
}

/// Every row of every kind of an older version, at any position of every
/// history: going on from R writes what a builder from its cursors' start
/// writes on the same store and what a build on an empty store holds, and
/// no scan reads before R. The positions cross the landing slot's attempts
/// whose through its row holds past R (`two late`).
#[test]
fn rows_of_an_older_version_are_written_again_with_no_scan_before_r() {
    for (name, log, samples, secs) in histories_and_forgotten() {
        let whole = {
            let memory = Memory::of(&log);
            memory.samples.replace(samples.clone());
            drain(&memory, &mut Builder::default(), secs);
            memory.rows.into_inner()
        };
        for upto in 1..=log.events.len() {
            let memory = drained_store(&log, &samples, upto, secs);
            let r = r_of(&memory);
            every_row_older(&memory);
            let from_start = from_the_start_on(&memory, &log, secs);
            memory.scans.borrow_mut().clear();
            let rows = started_and_drained(&memory, &log, secs);
            assert_eq!(differing(&rows, &from_start), Vec::new(), "{name} {upto}");
            assert_eq!(
                differing_from_whole(&rows, &whole),
                Vec::new(),
                "{name} {upto}"
            );
            scans_read_after(&memory, r);
        }
    }
}

/// One row of an older version, with stale values, of each row in turn of
/// a store R is at the end of: only that row is written, and as the build
/// of every input holds it, whichever kind it is. Nothing is scanned of the
/// events, so what it read of them it read by target. A next start finds
/// none left.
#[test]
fn one_row_of_an_older_version_is_written_again_alone_from_reads_by_target() {
    let mut kinds = BTreeSet::new();
    for (name, log, samples, secs) in histories_and_forgotten() {
        let whole = drained_store(&log, &samples, log.events.len(), secs)
            .rows
            .into_inner();
        for key in whole.keys().filter(|key| !whole[*key].is_legacy()) {
            let memory = drained_store(&log, &samples, log.events.len(), secs);
            older_and_stale(&memory, key);
            assert_ne!(memory.rows.borrow()[key], whole[key], "{name} {key:?}");
            kinds.insert(
                whole[key]
                    .subject
                    .as_ref()
                    .map_or(String::new(), |subject| subject.kind.clone()),
            );
            memory.scans.borrow_mut().clear();
            memory.written_keys.borrow_mut().clear();
            let r = r_of(&memory);
            let mut builder = start_builder(&memory, ms(secs)).unwrap();
            assert_eq!(builder.cursors(), r, "{name} {key:?}");
            drain(&memory, &mut builder, secs);
            assert_eq!(
                differing(&memory.rows.borrow(), &whole),
                Vec::new(),
                "{name} {key:?}"
            );
            assert_eq!(
                *memory.written_keys.borrow(),
                std::slice::from_ref(key),
                "{name} {key:?}"
            );
            assert_eq!(
                scans_of(&memory).get("events").map_or(0, |scan| scan.1),
                0,
                "{name} {key:?}"
            );
            scans_read_after(&memory, r);
            assert_eq!(memory.ledger_older_rows_after(None, 9).unwrap(), Vec::new());
        }
    }
    for kind in [
        "landing_slot",
        "live_alert_stream",
        "live_alert",
        "finding",
        "slots_full",
        "claim_hold",
        "claim_deferral",
        "landing_hold",
    ] {
        assert!(kinds.contains(kind), "{kind} in {kinds:?}");
    }
}

/// A landing slot's attempt is folded again on its run's events and the
/// next slot's start, but of its through it keeps what its row holds, the
/// one value of a queue row no read by target gives: a store with it stale
/// ends with that value while no event after R reaches the attempt.
#[test]
fn a_landing_slots_row_of_an_older_version_keeps_only_its_through_from_its_row() {
    let log = two_late_log();
    let whole = drained_store(&log, &[], log.events.len(), LATER)
        .rows
        .into_inner();
    let slots = landing_slots(&whole);
    let (key, slot) = slots
        .iter()
        .find(|(_, row)| row.ended_at.is_some())
        .unwrap();
    let key = (LedgerKind::Queue, key.clone());
    let memory = drained_store(&log, &[], log.events.len(), LATER);
    older_and_stale(&memory, &key);
    let through = EventId::new(slot.inputs_through.events.as_i64() + 1);
    memory
        .rows
        .borrow_mut()
        .get_mut(&key)
        .unwrap()
        .inputs_through
        .events = through;
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    drain(&memory, &mut builder, LATER);
    let mut expected = slot.clone();
    expected.inputs_through.events = through;
    assert_eq!(memory.rows.borrow()[&key], expected);
}

/// A landing slot's row of an older version whose attempt ended before R
/// keeps the through its row holds, which no read by target can cut, and
/// the events after R set it: at every position it ends as the fold's.
#[test]
fn a_landing_slots_row_of_an_older_version_ends_with_the_through_of_the_fold() {
    let log = two_late_log();
    let whole = landing_slots(
        &drained_store(&log, &[], log.events.len(), LATER)
            .rows
            .into_inner(),
    );
    assert!(!whole.is_empty());
    for upto in 1..=log.events.len() {
        let memory = drained_store(&log, &[], upto, LATER);
        every_row_older(&memory);
        let rows = started_and_drained(&memory, &log, LATER);
        assert_eq!(landing_slots(&rows), whole, "{upto}");
    }
}

/// The store at the first `upto` events of `log`, its rows of an older
/// version and stale, given every event: a start writes what a build of
/// every event on an empty store holds, at each row. `upto` is where it
/// matters to the row named by the test.
#[track_caller]
fn ends_as_the_build(log: &Log, upto: usize) {
    let whole = {
        let memory = Memory::of(log);
        drain(&memory, &mut Builder::default(), LATER);
        memory.rows.into_inner()
    };
    let memory = drained_store(log, &[], upto, LATER);
    every_row_older(&memory);
    let rows = started_and_drained(&memory, log, LATER);
    assert_eq!(differing(&rows, &whole), Vec::new());
}

/// The last landing slot's attempt open at R, the next one's start after R
/// and its run's end after that: it is folded up to R only, so the next
/// start ends it as the build does, not its run's later end.
#[test]
fn a_landing_slot_open_at_r_is_folded_on_no_event_after_r() {
    let (x, y) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.add(Some(1), Some(&x), "integration_started", json!({}), 10)
        .add(Some(2), Some(&y), "integration_started", json!({}), 20)
        .add(Some(1), Some(&x), "run_integrated", json!({}), 30)
        .add(Some(2), Some(&y), "run_integrated", json!({}), 40);
    ends_as_the_build(&log, 1);
}

/// An older slot before a landing slot of this version is folded before
/// the events after R too: a late event reaches the attempts by their ends.
#[test]
fn a_late_event_reaches_an_older_landing_slot_by_its_fold_not_its_head() {
    let (x, y) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.add(Some(1), Some(&x), "integration_started", json!({}), 10)
        .add(Some(1), Some(&x), "run_integrated", json!({}), 20)
        .add(Some(2), Some(&y), "integration_started", json!({}), 30)
        .add(Some(2), Some(&y), "run_integrated", json!({}), 40)
        .add(Some(3), None, "task_created", json!({}), 25);
    let whole = {
        let memory = Memory::of(&log);
        drain(&memory, &mut Builder::default(), LATER);
        memory.rows.into_inner()
    };
    let memory = drained_store(&log, &[], 4, LATER);
    let first = (LedgerKind::Queue, log.events[0].id.to_string());
    older_and_stale(&memory, &first);
    let rows = started_and_drained(&memory, &log, LATER);
    assert_eq!(differing(&rows, &whole), Vec::new());
}

/// A slots full started while one of its key is open is one event of its
/// own by what that other row was: alone in its own inputs it is no wait.
#[test]
fn a_start_that_goes_on_with_an_open_wait_stays_one_event_of_its_own() {
    let full = json!({"supervisor": "s", "class": "heavy"});
    let mut log = Log::default();
    log.add(
        None,
        None,
        "supervisor_started",
        json!({"supervisor": "s"}),
        0,
    )
    .add(None, None, "slots_full_started", full.clone(), 10)
    .add(None, None, "slots_full_started", full.clone(), 20)
    .add(None, None, "slots_full_ended", full, 30);
    let kinds = |rows: &Rows| -> Vec<String> {
        rows.values()
            .filter_map(|row| Some(row.subject.as_ref()?.kind.clone()))
            .collect()
    };
    let whole = Memory::of(&log);
    drain(&whole, &mut Builder::default(), LATER);
    assert!(kinds(&whole.rows.borrow()).contains(&"slots_full_started".to_owned()));
    ends_as_the_build(&log, log.events.len());
}

/// One supervisor's stop ends its hold on the claims and on the landings:
/// it is a record of the first alone, and of the other an input past its
/// records, as each is folded alone too.
#[test]
fn a_stop_that_ends_two_holds_stays_the_record_of_one() {
    let by = json!({"supervisor": "s"});
    let mut log = Log::default();
    log.add(None, None, "supervisor_started", by.clone(), 0)
        .add(None, None, "claim_held", by.clone(), 10)
        .add(None, None, "landing_held", by.clone(), 20)
        .add(None, None, "supervisor_stopped", by, 30);
    ends_as_the_build(&log, log.events.len());
}

/// A wait a supervisor's silence ended says a time an older fold made: the
/// life reads its last event, not that time, for this row and another of
/// the same supervisor.
#[test]
fn a_life_is_not_made_of_an_older_folds_time() {
    let full = json!({"supervisor": "s", "class": "heavy"});
    let mut log = Log::default();
    log.add(
        None,
        None,
        "supervisor_started",
        json!({"supervisor": "s"}),
        0,
    )
    .add(None, None, "slots_full_started", full.clone(), 10)
    .add(
        None,
        None,
        "slots_full_started",
        json!({"supervisor": "s", "class": "light"}),
        20,
    );
    let memory = drained_store(&log, &[], log.events.len(), LATER);
    let whole = memory.rows.borrow().clone();
    assert!(whole.values().any(|row| {
        row.outcome.as_deref() == Some(crate::domain::claim_hold::ENDED_SUPERVISOR_GONE)
    }));
    let first = (LedgerKind::Queue, log.events[1].id.to_string());
    older_and_stale(&memory, &first);
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    drain(&memory, &mut builder, LATER);
    assert_eq!(differing(&memory.rows.borrow(), &whole), Vec::new());
}

/// Rows of this version settled alone add nothing to what folding those of
/// an older version reads: the same reads by target, the same events and
/// no scan more, on 2 rows settled or 40. Of those of this version none is
/// read, and without an older row neither is the older one's.
#[test]
fn the_reads_of_rows_of_an_older_version_do_not_grow_with_the_rows_of_this_version() {
    let run = many_run_id(1);
    let older = |memory: &Memory| {
        // The supervisor's first event, and the first landing slot's row.
        let slot = landing_slots(&memory.rows.borrow())
            .into_keys()
            .min_by_key(|key| key.parse::<i64>().unwrap())
            .unwrap();
        for row in memory.rows.borrow_mut().values_mut() {
            if row.key == run
                || (row.kind == LedgerKind::Task && row.key == "1")
                || (row.kind == LedgerKind::Queue && (row.key == "1" || row.key == slot))
            {
                row.ledger_v = LEDGER_V - 1;
            }
        }
    };
    let (few, many) = (
        reads_of_a_start_on(2, &older),
        reads_of_a_start_on(40, &older),
    );
    assert_eq!(few.scans, many.scans);
    assert_eq!(few.events, many.events);
    assert_eq!(few.runs_read, many.runs_read);
    assert_eq!(few.read_one.len(), many.read_one.len());
    assert_eq!(few.cut, many.cut);
    assert_eq!(few.scans["events"], 2);
    let of_run: Vec<&str> = many.runs_read.iter().map(RunId::as_str).collect();
    let open = [many_run_id(9_001), many_run_id(9_002)];
    assert!(
        of_run
            .iter()
            .all(|read| *read == run || open.iter().any(|id| id == read)),
        "{of_run:?}"
    );
    assert!(of_run.contains(&run.as_str()), "{of_run:?}");

    let none = reads_of_a_start(40);
    assert!(!none.runs_read.iter().any(|read| read.as_str() == run));
    assert!(none.events < many.events);
}

/// More rows of an older version than a pass writes are marked and folded
/// a share at a time, with no more marks waiting than a pass writes; a
/// builder of a new process goes on with those left, by their version.
#[test]
fn rows_of_an_older_version_are_folded_a_share_at_a_time_and_a_later_start_finds_those_left() {
    let memory = Memory::default();
    let log = settled_and_open(WRITES_PER_PASS, &memory);
    drain(&memory, &mut Builder::default(), LATER);
    let whole = memory.rows.borrow().clone();
    every_row_older(&memory);
    let older = whole.len();

    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    let built = pass(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    assert!(built.written <= WRITES_PER_PASS);
    assert!(built.backlog > 0);
    assert!(builder.backlog() <= WRITES_PER_PASS + 1, "{built:?}");
    let left = memory
        .ledger_older_rows_after(None, usize::MAX)
        .unwrap()
        .len();
    assert!(left < older && left > 0, "{left} of {older}");

    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    assert_eq!(builder.cursors().events, log.last());
    drain(&memory, &mut builder, LATER);
    assert_eq!(memory.ledger_older_rows_after(None, 9).unwrap(), Vec::new());
    assert_eq!(differing(&memory.rows.borrow(), &whole), Vec::new());
}

/// More waits of an older version than a pass's share of the rows, every one
/// ended by an event and stale, and a task that ended after R and whose grace
/// has passed: the task settles in the first pass, reading those waits, and
/// no later pass folds it again, so each is folded before that pass and none
/// of their records stays held for it.
#[test]
fn a_task_that_settles_in_the_first_pass_reads_the_folds_of_the_older_waits() {
    let mut log = Log::default();
    log.add(Some(8), None, "task_created", json!({"status": "draft"}), 0)
        .add(
            Some(8),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "ready"}),
            1,
        );
    let waits = i64::try_from(WRITES_PER_PASS).unwrap() + 20;
    for wait in 0..waits {
        let by = json!({"supervisor": format!("s{wait}")});
        log.add(None, None, "claim_held", by.clone(), 2 + wait * 2)
            .add(None, None, "claim_resumed", by, 3 + wait * 2);
    }
    let at_r = log.events.len();
    log.add(
        Some(8),
        None,
        "task_status_changed",
        json!({"from": "ready", "to": "completed"}),
        3 + waits * 2,
    );
    let whole = {
        let memory = Memory::of(&log);
        drain(&memory, &mut Builder::default(), LATER);
        memory.rows.into_inner()
    };
    assert!(whole[&(LedgerKind::Task, "8".to_owned())].is_final);

    let memory = drained_store(&log, &[], at_r, LATER);
    every_row_older(&memory);
    memory.events.replace(log.events.clone());
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    assert_eq!(builder.kept().size().queue, 0);
    assert_eq!(pass(&memory, &mut builder, LATER).failure, None);
    let settled = memory.row(LedgerKind::Task, "8");
    assert!(settled.is_final);
    assert_eq!(settled.ledger_v, LEDGER_V);
    assert_eq!(settled, whole[&(LedgerKind::Task, "8".to_owned())]);
    assert!(memory.ledger_older_rows().unwrap(), "a share is left");
    drain(&memory, &mut builder, LATER);
    assert_eq!(differing(&memory.rows.borrow(), &whole), Vec::new());
}

/// With no mark the builder reads from its cursors' start whatever the
/// versions of the rows, and writes what a build of every input holds.
/// So it does with marks of an older version, which give no R. A legacy
/// row keeps the version it was first written at, and is no row to fold
/// again: its version takes no builder from R.
#[test]
fn a_store_with_no_r_starts_from_the_cursors_start_whatever_the_versions_of_its_rows() {
    let (log, samples) = every_kind_of_row();
    let whole = drained_store(&log, &samples, log.events.len(), LATER)
        .rows
        .into_inner();

    let memory = drained_store(&log, &samples, log.events.len(), LATER);
    every_row_older(&memory);
    memory.mark.replace(None);
    memory.rows_mark.replace(None);
    assert_eq!(rows_written(&memory).unwrap(), None);
    memory.scans.borrow_mut().clear();
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    assert_eq!(builder.cursors(), Cursors::default());
    drain(&memory, &mut builder, LATER);
    assert_eq!(differing(&memory.rows.borrow(), &whole), Vec::new());
    assert_eq!(memory.scans.borrow().first().unwrap().1, 0);

    let memory = drained_store(&log, &samples, log.events.len(), LATER);
    every_row_older(&memory);
    let mark = memory.mark.borrow().map(|mark| SessionsWritten {
        ledger_v: LEDGER_V - 1,
        ..mark
    });
    memory.mark.replace(mark);
    assert_eq!(rows_written(&memory).unwrap(), None);
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    assert_eq!(builder.cursors(), Cursors::default());
    drain(&memory, &mut builder, LATER);
    assert_eq!(differing(&memory.rows.borrow(), &whole), Vec::new());

    let legacy = Memory::default();
    let mut log = Log::default();
    log.legacy(1, &run_id(1), 0);
    legacy.events.replace(log.events.clone());
    drain(&legacy, &mut Builder::default(), LATER);
    assert!(
        legacy
            .rows
            .borrow_mut()
            .values_mut()
            .filter(|row| row.is_legacy())
            .map(|row| row.ledger_v = LEDGER_V - 1)
            .count()
            > 0
    );
    assert_eq!(legacy.ledger_older_rows_after(None, 9).unwrap(), Vec::new());
    let builder = start_builder(&legacy, ms(LATER)).unwrap();
    assert_eq!(builder.cursors(), r_of(&legacy));
    assert_eq!(builder.events, log.last());
}
