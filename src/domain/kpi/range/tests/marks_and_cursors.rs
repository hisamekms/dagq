//! [`range_events`] and the event cursor's time, on the cases of Spike
//! 4135's (i)–(iv) and Spike 4670's (vi): the marks and routine of the
//! range are every event's, and what is read does not grow with the
//! settled history outside it.
use super::*;

/// The range is three hours after `units` settled units; as many follow it
/// when `after` (a range in the past).
fn around(units: i64, inside: impl FnOnce(&mut Log, i64), after: bool) -> (Log, i64, i64, ()) {
    let mut log = Log::default();
    log.run(None, attributes("v0", 4, Some("0.45.0")), T0);
    log.start("v1", 4, T0 + MIN);
    let from = log.settled(units, T0 + 2 * MIN);
    let to = from + 180 * MIN;
    inside(&mut log, from);
    if after {
        log.settled(units, to + MIN);
    }
    (log, from, to, ())
}

/// A mark written after the range and backdated far into it, a pruned stop
/// whose heartbeat is in the range written after it, and a mark written in
/// the range but backdated far before it (not of it).
#[test]
fn a_mark_backdated_far_is_read_by_when_it_took_effect() {
    let build = |units| {
        let (mut log, from, to, ()) = around(
            units,
            |log, from| {
                log.run(None, attributes("v1", 4, None), from + 10 * MIN);
                log.mark(
                    "in range, backdated out",
                    Some(T0 + 5 * MIN),
                    from + 20 * MIN,
                );
            },
            true,
        );
        let last = log.events.last().map(created_ms).unwrap();
        log.mark(
            "backdated into the range",
            Some(from + 30 * MIN),
            last + MIN,
        );
        log.queue(
            EventKind::SupervisorStopped,
            json!({"supervisor": "s", "outcome": "pruned", "dagq_version": "v1",
                   "last_heartbeat_at": (from + 40 * MIN) / 1000}),
            last + 2 * MIN,
        );
        (log, from, to, ())
    };
    let (small, large) = reads_with(build, |store, from, to, ()| {
        let whole = whole_marks(store, from, to);
        assert_eq!(marks_in(store, from, to), whole);
        let labels: Vec<&str> = whole.iter().map(|m| m.label.as_str()).collect();
        assert!(labels.contains(&"backdated into the range"), "{labels:?}");
        assert!(labels.contains(&"supervisor stopped: v1"), "{labels:?}");
        assert!(!labels.contains(&"in range, backdated out"), "{labels:?}");
    });
    assert_eq!(small, large);
}

/// A mark of the range retracted after the range, other marks retracted
/// after it too: the retraction is read by the mark it names.
#[test]
fn a_retraction_after_the_range_is_read_by_the_mark_it_names() {
    let build = |units| {
        let mut mark = EventId::new(0);
        let (mut log, from, to, ()) = around(
            units,
            |log, from| {
                mark = log.mark("retracted later", None, from + 10 * MIN);
                log.mark("kept", None, from + 20 * MIN);
            },
            true,
        );
        let last = log.events.last().map(created_ms).unwrap();
        log.retract(mark, last + MIN);
        (log, from, to, ())
    };
    let (small, large) = reads_with(build, |store, from, to, ()| {
        let whole = whole_marks(store, from, to);
        assert_eq!(marks_in(store, from, to), whole);
        assert!(
            whole
                .iter()
                .any(|m| m.label == "retracted later" && m.retracted_by.is_some())
        );
        assert!(
            whole
                .iter()
                .any(|m| m.label == "kept" && m.retracted_by.is_none())
        );
    });
    assert_eq!(small, large);
}

/// Codex's version last recorded at the start of the history, a long run of
/// claims without it, a new version in the range; a new build announced by
/// a start just before the range, then another start.
#[test]
fn each_attributes_last_value_is_read_however_old() {
    let build = |units| {
        let mut log = Log::default();
        log.run(None, attributes("v0", 4, Some("0.45.0")), T0);
        log.start("v1", 4, T0 + MIN);
        let from = log.settled(units, T0 + 2 * MIN);
        log.start("v2", 4, from - MIN);
        log.start("v9", 4, from - MIN / 2);
        let to = from + 180 * MIN;
        log.run(None, attributes("v2", 4, Some("0.46.0")), from + 10 * MIN);
        log.run(None, attributes("v2", 4, None), from + 20 * MIN);
        log.run(None, attributes("v3", 4, Some("0.46.0")), from + 30 * MIN);
        (log, from, to, ())
    };
    let (small, large) = reads_with(build, |store, from, to, ()| {
        let whole = whole_marks(store, from, to);
        assert_eq!(marks_in(store, from, to), whole);
        let labels: Vec<&str> = whole.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(
            labels,
            ["codex_version 0.45.0 → 0.46.0", "dagq_version v2 → v3"],
            "the start announced v1 → v2"
        );
    });
    assert_eq!(small, large);
}

/// The first start of the range after one before it with the same
/// `parallel` is routine, after one with another is not.
#[test]
fn routine_reads_the_last_start_before_the_range() {
    for (before, routine) in [(4, true), (3, false)] {
        let build = |units| {
            let (mut log, from, to, ()) = around(units, |_, _| {}, false);
            log.start("v1", before, from - MIN / 2);
            log.start("v1", 4, from + 10 * MIN);
            log.mark("a change", None, from + 20 * MIN);
            (log, from, to, ())
        };
        let (small, large) = reads_with(build, |store, from, to, ()| {
            let whole = classified(&store.events, from, to);
            let events = range_events(store, from, to).unwrap();
            assert_eq!(classified(&events, from, to), whole);
            assert_eq!(whole[0].1, routine, "{before}");
        });
        assert_eq!(small, large);
    }
}

/// Starts written (0 min, parallel 1), (20 min, 1), (10 min, 2), the clock
/// turned back before the third: in `(15, 25]` the start of 20 min follows
/// the one of 10 min in time, whose `parallel` differs, so it is not
/// routine (by id it would be).
#[test]
fn routine_takes_the_start_before_it_in_time_not_in_id() {
    let build = |units| {
        let mut log = Log::default();
        let base = log.settled(units, T0);
        log.start("v1", 1, base);
        log.start("v1", 1, base + 20 * MIN);
        log.start("v1", 2, base + 10 * MIN);
        (log, base + 15 * MIN, base + 25 * MIN, ())
    };
    let (small, large) = reads_with(build, |store, from, to, ()| {
        let whole = classified(&store.events, from, to);
        assert_eq!(whole.len(), 1);
        assert!(!whole[0].1, "not routine of every event");
        let events = range_events(store, from, to).unwrap();
        assert_eq!(classified(&events, from, to), whole);
    });
    assert_eq!(small, large);
}

/// Claims written v1 (in the range), v2 (the clock turned back out of it),
/// v2 (in it): the third follows the second by id and is no change.
#[test]
fn a_claim_is_compared_with_the_claim_before_it_in_id() {
    let build = |units| {
        let mut log = Log::default();
        let base = log.settled(units, T0);
        log.run(None, attributes("v1", 4, None), base + 100 * MIN);
        log.run(None, attributes("v2", 4, None), base - 50 * MIN);
        log.run(None, attributes("v2", 4, None), base + 110 * MIN);
        (log, base + 90 * MIN, base + 120 * MIN, ())
    };
    let (small, large) = reads_with(build, |store, from, to, ()| {
        let whole = whole_marks(store, from, to);
        assert!(whole.is_empty(), "{whole:?}");
        assert_eq!(marks_in(store, from, to), whole);
    });
    assert_eq!(small, large);
}

/// A start written between a claim before the range and one of it by id,
/// by a clock ahead past the range's end, announces the claim's new build:
/// the change is the start's and derives no mark.
#[test]
fn a_start_between_the_claims_by_id_is_sought_whatever_its_time() {
    let build = |units| {
        let mut log = Log::default();
        let from = log.settled(units, T0);
        let to = from + 60 * MIN;
        let start = log.start("v2", 4, to + 60 * MIN);
        log.run(None, attributes("v2", 4, None), from + 10 * MIN);
        (log, from, to, start)
    };
    let (small, large) = reads_with(build, |store, from, to, start| {
        let whole = whole_marks(store, from, to);
        assert!(whole.is_empty(), "{whole:?}");
        let events = range_events(store, from, to).unwrap();
        assert!(events.iter().any(|e| e.id == *start));
        assert_eq!(marks_in(store, from, to), whole);
    });
    assert_eq!(small, large);
}

/// A run written before a claim of the range by a clock ahead past the
/// range's end: its claim is the range claim's previous one by id.
#[test]
fn a_previous_claim_is_sought_by_id_not_by_time() {
    let build = |units| {
        let mut log = Log::default();
        let from = log.settled(units, T0);
        let to = from + 60 * MIN;
        log.run(None, attributes("v2", 4, None), to + 30 * MIN);
        log.run(None, attributes("v2", 4, None), from + 10 * MIN);
        log.settled(units, to + 120 * MIN);
        (log, from, to, ())
    };
    let (small, large) = reads_with(build, |store, from, to, ()| {
        let whole = whole_marks(store, from, to);
        assert!(whole.is_empty(), "the change is the earlier claim's");
        assert_eq!(marks_in(store, from, to), whole);
    });
    assert_eq!(small, large);
}

/// A mark written before the range by the DB's clock with `--at` in it (the
/// CLI's clock ahead), a pruned stop whose heartbeat is in the range written
/// before it, and a mark whose `--at` is after the DB's now.
#[test]
fn a_clock_ahead_of_the_db_is_read_by_the_effective_time() {
    let build = |units| {
        let mut log = Log::default();
        let from = log.settled(units, T0);
        log.mark("cli ahead", Some(from + 2 * MIN), from - MIN);
        log.queue(
            EventKind::SupervisorStopped,
            json!({"supervisor": "s", "outcome": "pruned", "dagq_version": "v1",
                   "last_heartbeat_at": (from + 3 * MIN) / 1000}),
            from - MIN / 2,
        );
        log.run(None, attributes("v1", 4, None), from + 5 * MIN);
        let now = from + 10 * MIN;
        log.mark("after the db's now", Some(now + 5 * MIN), now);
        (log, from, now + 10 * MIN, ())
    };
    let (small, large) = reads_with(build, |store, from, to, ()| {
        let whole = whole_marks(store, from, to);
        let labels: Vec<&str> = whole.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(
            labels,
            ["cli ahead", "supervisor stopped: v1", "after the db's now"]
        );
        assert_eq!(marks_in(store, from, to), whole);
    });
    assert_eq!(small, large);
}

/// A mark of the range retracted later by id but by a clock turned back
/// before the range.
#[test]
fn a_retraction_by_a_clock_turned_back_is_read_by_the_mark() {
    let build = |units| {
        let mut log = Log::default();
        let from = log.settled(units, T0);
        let mark = log.mark("retracted", None, from + 10 * MIN);
        log.retract(mark, from - 30 * MIN);
        (log, from, from + 60 * MIN, ())
    };
    let (small, large) = reads_with(build, |store, from, to, ()| {
        let whole = whole_marks(store, from, to);
        assert!(whole[0].retracted_by.is_some());
        assert_eq!(marks_in(store, from, to), whole);
    });
    assert_eq!(small, large);
}

/// An event cursor stands for the latest time at or before its id, which a
/// clock turned back makes another event's. Near now the latest events
/// hold it; in the past, with more written after it than the walk's limit,
/// the aggregate of the times at or before it finds it: either way the
/// cursor's time is every event's, the question has a range, and the read
/// does not grow with the history before or after the cursor.
#[test]
fn an_event_cursor_across_a_clock_turned_back_is_found_near_now_and_in_the_past() {
    let build = |past: bool| {
        move |units| {
            let mut log = Log::default();
            let base = log.settled(if past { 30 } else { units }, T0);
            log.mark("ahead", None, base + 60 * MIN);
            let cursor = log.mark("behind", None, base + 10 * MIN);
            if past {
                log.settled(units, base + 120 * MIN);
            }
            (log, 0, 0, cursor)
        }
    };
    for past in [false, true] {
        let (small, large) = reads_with(build(past), |store, _, _, cursor| {
            let whole = marks::cursor_millis(Cursor::Event(*cursor), &store.events);
            let own = created_ms(&store.events[cursor.as_i64() as usize - 1]);
            assert!(whole > own, "the clock turned back");
            assert_eq!(store.cursor_time(*cursor), Ok(Some(whole)));
            let query = KpiQuery {
                at: Some(Cursor::Event(*cursor)),
                ..KpiQuery::default()
            };
            let now = created_ms(store.events.last().unwrap()) / 1000;
            let range = plan(
                &query,
                &KpiConfig::default(),
                now,
                0,
                store,
                &HashMap::new(),
            )
            .unwrap()
            .range;
            assert_eq!(
                range.until_ms,
                Some(whole - whole.rem_euclid(DAY_MS) + DAY_MS)
            );
        });
        if past {
            // The latest events and one aggregate, however long the history
            // after the cursor (with 30 units the latest events still hold
            // it, with 300 the aggregate finds it), for each of the two
            // reads of the cursor (its own and `plan`'s).
            assert!(
                small <= 2 * (WALK + 1) && large == 2 * (WALK + 1),
                "{small} {large}"
            );
        } else {
            assert_eq!(small, large);
        }
    }
}

/// Events the ledger's snapshot does not see among the cursor's candidates:
/// the latest event at or before the cursor invisible (the aggregate's time
/// is no visible event's, so the walk passes it to the visible one), and
/// more invisible ones at the top than the walk's limit, which alone leaves
/// the question undecided. Each answer is that of the visible events.
#[test]
fn an_event_cursor_skips_invisible_candidates_and_is_undecided_only_past_the_walk() {
    for hidden_count in [1, WALK + 1] {
        let mut log = Log::default();
        let base = log.settled(300, T0);
        let cursor = log.mark("cursor", None, base - 5 * HOUR);
        let mut hidden = HashSet::new();
        for n in 0..hidden_count as i64 {
            // Written before the cursor by id, later than it in time.
            log.events.insert(0, {
                let mut event = log.events[0].clone();
                event.created_at = marks::utc_text(base + HOUR + n);
                event
            });
        }
        for (n, event) in log.events.iter_mut().enumerate() {
            event.id = EventId::new(n as i64 + 1);
        }
        hidden.extend((1..=hidden_count as i64).map(EventId::new));
        let cursor = EventId::new(cursor.as_i64() + hidden_count as i64);
        // Settled history after the cursor: the latest events do not hold it.
        log.settled(300, base + 2 * HOUR);
        let visible: Vec<RunEvent> = log
            .events
            .iter()
            .filter(|e| !hidden.contains(&e.id))
            .cloned()
            .collect();
        let whole = marks::cursor_millis(Cursor::Event(cursor), &visible);
        let store = Store::hiding(log.events.clone(), hidden.clone());
        let query = KpiQuery {
            since: Some(Cursor::Event(cursor)),
            ..KpiQuery::default()
        };
        let now = created_ms(log.events.last().unwrap()) / 1000;
        let plan = plan(
            &query,
            &KpiConfig::default(),
            now,
            0,
            &store,
            &HashMap::new(),
        );
        if hidden_count <= WALK {
            assert_eq!(store.cursor_time(cursor), Ok(Some(whole)));
            assert_eq!(plan.unwrap().range.from_ms, 2 * whole - now * 1000);
        } else {
            assert_eq!(
                store.cursor_time(cursor),
                Err(Undecided::OutOfTimeOrder(cursor))
            );
            assert_eq!(plan, Err(Undecided::OutOfTimeOrder(cursor)));
        }
    }
}

/// The questions without a range, and the range of each kind of question.
#[test]
fn a_question_without_a_range_is_undecided_and_the_others_have_one() {
    let mut log = Log::default();
    let base = log.settled(30, T0);
    let mark = log.mark("m", None, base + 10 * MIN);
    let store = log.store();
    let config = KpiConfig::default();
    let now = (base + DAY_MS) / 1000;
    let plan_of = |query: KpiQuery| plan(&query, &config, now, 0, &store, &HashMap::new());
    let undecided = [
        (
            KpiQuery {
                read_whole_ledger: true,
                ..KpiQuery::default()
            },
            Undecided::WholeLedger,
        ),
        (
            KpiQuery {
                last: 0,
                ..KpiQuery::default()
            },
            Undecided::NoPeriods,
        ),
        (
            KpiQuery {
                until: Some(Cursor::Time(base)),
                ..KpiQuery::default()
            },
            Undecided::FromFirstEvent,
        ),
        (
            KpiQuery {
                since: Some(Cursor::Time(base)),
                until: Some(Cursor::Time(base)),
                ..KpiQuery::default()
            },
            Undecided::Refused,
        ),
        (
            KpiQuery {
                at: Some(Cursor::Event(EventId::new(0))),
                ..KpiQuery::default()
            },
            Undecided::NoEvent(EventId::new(0)),
        ),
    ];
    for (query, expected) in undecided {
        assert_eq!(plan_of(query.clone()), Err(expected), "{query:?}");
    }
    let day = |at: i64| at - at.rem_euclid(DAY_MS);
    let range = |query: KpiQuery| plan_of(query).unwrap().range;
    // The periods reach now; --at ends them with its period.
    let oldest = period_count(&KpiQuery::default(), &config) as i64 - 1;
    assert_eq!(
        range(KpiQuery::default()),
        ReadRange {
            from_ms: day(now * 1000) - oldest * DAY_MS,
            until_ms: None
        }
    );
    assert_eq!(
        range(KpiQuery {
            at: Some(Cursor::Event(mark)),
            ..KpiQuery::default()
        }),
        ReadRange {
            from_ms: day(base + 10 * MIN) - oldest * DAY_MS,
            until_ms: Some(day(base + 10 * MIN) + DAY_MS)
        }
    );
    // --by and --goal do not change it.
    assert_eq!(
        range(KpiQuery {
            by: vec!["parallel".parse().unwrap()],
            goal_id: Some(GoalId::new(1)),
            ..KpiQuery::default()
        }),
        range(KpiQuery::default())
    );
    // A window and the one as long before it.
    assert_eq!(
        range(KpiQuery {
            since: Some(Cursor::Event(mark)),
            until: Some(Cursor::Time(base + HOUR)),
            ..KpiQuery::default()
        }),
        ReadRange {
            from_ms: base + 10 * MIN - 50 * MIN,
            until_ms: Some(base + HOUR)
        }
    );
    assert_eq!(
        range(KpiQuery {
            since: Some(Cursor::Time(base)),
            ..KpiQuery::default()
        }),
        ReadRange {
            from_ms: base - (now * 1000 - base),
            until_ms: None
        }
    );
}
