//! The KPIs of the queue no run carries: slots, supervisors, verification,
//! backend failures and the integration slot.

use super::*;

/// The KPIs no run carries: slot usage while a supervisor lived, the
/// candidates' samples, asks per landing and a person's wait, attentions
/// and findings.
#[test]
fn derives_the_queue_kpis_of_a_window() {
    let mut queue = Queue::default();
    start(&mut queue, 2, MONDAY);
    queue.queue_event(
        window::CANDIDATES_SAMPLED,
        json!({"supervisor": "s", "candidates": 2, "free_slots": 1, "ready": 2}),
        MONDAY,
    );
    // Two hours of one run in four hours of two slots.
    queue.run(&Run::new(
        1,
        Some("runtime".parse::<TaskChange>().unwrap()),
        MONDAY + HOUR,
        2 * HOUR - 100,
    ));
    queue.queue_event(
        window::CANDIDATES_SAMPLED,
        json!({"supervisor": "s", "candidates": 0, "free_slots": 1, "ready": 1}),
        MONDAY + 2 * HOUR,
    );
    queue.queue_event(
        marks::SUPERVISOR_STOPPED,
        json!({"supervisor": "s"}),
        MONDAY + 4 * HOUR,
    );
    queue.push(
        Some(1),
        None,
        "ask_opened",
        json!({"ask_id": 7, "kind": "worker_question", "reason_category": "scope"}),
        MONDAY + 90 * 60,
    );
    queue.push(
        Some(1),
        None,
        "ask_answered",
        json!({"ask_id": 7, "answered_by": "human"}),
        MONDAY + 100 * 60,
    );
    queue.push(
        Some(1),
        None,
        "ask_opened",
        json!({"ask_id": 8, "kind": "stuck_exit"}),
        MONDAY + 100 * 60,
    );
    queue.push(
        Some(1),
        None,
        "ask_answered",
        json!({"ask_id": 8, "runtime_closed": true}),
        MONDAY + 101 * 60,
    );
    queue.queue_event("finding_recorded", json!({"finding_id": 1}), MONDAY + HOUR);
    queue.queue_event("finding_recorded", json!({"finding_id": 2}), MONDAY + HOUR);
    queue.queue_event(
        "finding_status_changed",
        json!({"finding_id": 1, "from": "open", "to": "resolved"}),
        MONDAY + 3 * HOUR,
    );
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    let day = result.periods.last().unwrap();
    assert!(!day.partial);
    assert_eq!(measure(day, "slot_usage", ALL).value, Some(0.25));
    assert_eq!(measure(day, "slot_usage", ALL).n, 1);
    // 2 candidates for two hours, then none until the stop (two hours).
    let candidates = measure(day, "candidates", ALL);
    assert_eq!(candidates.value, Some(1.0));
    assert_eq!(candidates.max, Some(2.0));
    assert_eq!(
        day.window.details["candidates"]["starved_secs"],
        json!(2 * HOUR)
    );
    assert_eq!(measure(day, "asks_per_landing", ALL).value, Some(2.0));
    assert_eq!(
        measure(day, "asks_per_landing", "change=runtime").value,
        Some(2.0)
    );
    // The runtime's own close is no person's wait.
    let wait = measure(day, "ask_wait", ALL);
    assert_eq!((wait.n, wait.median), (1, Some(600.0)));
    assert_eq!(measure(day, "findings_open", ALL).value, Some(1.0));
    assert_eq!(
        measure(day, "finding_resolve_time", ALL).median,
        Some(7200.0)
    );
    assert_eq!(day.window.details["findings"]["recorded"], json!(2));
    assert!(measure(day, "attentions_per_landing", ALL).value.is_some());
    // No finding was linked to a proposal: no improvement runs.
    assert_eq!(measure(day, "improvement_proposals", ALL).value, Some(0.0));
    assert!(!day.window.unavailable.contains_key("improvement_proposals"));
    assert!(!day.window.unavailable.contains_key("candidates"));
    // A day before the queue: no supervisor, no sample.
    let before = &result.periods[0];
    assert_eq!(measure(before, "slot_usage", ALL).value, None);
    assert_eq!(before.window.unavailable["candidates"], "no_samples");
    assert_eq!(measure(before, "landings", ALL).value, Some(0.0));
}

/// A run closed on a verified `already_satisfied` claim (ADR-t1818-1)
/// holds its slot while its verification runs, none while it rests on its
/// `approve_close` ask, one again while the verification runs again for
/// the answer, and none after its `run_satisfied_closed`, which carries no
/// status; it is no landing.
#[test]
fn a_closed_claim_holds_its_slot_only_while_it_verifies_and_is_no_landing() {
    let mut queue = Queue::default();
    start(&mut queue, 2, MONDAY);
    let run = "00000001-0000-4000-8000-000000000001";
    queue.goals.insert(TaskId::new(1), None);
    let mut push =
        |kind: &str, payload: Value, secs: i64| queue.push(Some(1), Some(run), kind, payload, secs);
    push(
        "run_claimed",
        json!({"parallel": 2, "slots": 1}),
        MONDAY + HOUR,
    );
    push(
        "run_satisfied_snapshot",
        json!({"head": "a"}),
        MONDAY + HOUR,
    );
    push(
        "run_satisfied_verified",
        json!({"head": "a", "passed": true}),
        MONDAY + 90 * 60,
    );
    push(
        "run_satisfied_snapshot",
        json!({"head": "b", "ask_id": 1}),
        MONDAY + 3 * HOUR,
    );
    push(
        "run_satisfied_verified",
        json!({"head": "b", "passed": true, "ask_id": 1}),
        MONDAY + 210 * 60,
    );
    push(
        "run_satisfied_closed",
        json!({"head": "b", "ask_id": 1, "answer": "close"}),
        MONDAY + 210 * 60,
    );
    queue.queue_event(
        marks::SUPERVISOR_STOPPED,
        json!({"supervisor": "s"}),
        MONDAY + 4 * HOUR,
    );
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    let day = result.periods.last().unwrap();
    // Two half hours of one run in four hours of two slots.
    assert_eq!(measure(day, "slot_usage", ALL).value, Some(0.125));
    assert_eq!(measure(day, "landings", ALL).value, Some(0.0));
    assert_eq!(day.window.details["already_satisfied"], json!(1));
}

#[test]
fn a_claim_held_for_its_container_holds_no_slot_until_its_verification_starts() {
    let mut queue = Queue::default();
    start(&mut queue, 2, MONDAY);
    let run = "00000001-0000-4000-8000-000000000001";
    queue.goals.insert(TaskId::new(1), None);
    let mut push =
        |kind: &str, payload: Value, secs: i64| queue.push(Some(1), Some(run), kind, payload, secs);
    push(
        "run_claimed",
        json!({"parallel": 2, "slots": 1}),
        MONDAY + HOUR,
    );
    // Accepted while its verification's container cannot be used: the
    // claim gives its slot back and rests until the verification starts.
    push(
        "validation_finished",
        json!({"status": "validating", "already_satisfied": true}),
        MONDAY + 90 * 60,
    );
    push(
        "lease_released",
        json!({"reason": "finished"}),
        MONDAY + 90 * 60,
    );
    push(
        "run_satisfied_snapshot",
        json!({"head": "a"}),
        MONDAY + 3 * HOUR,
    );
    push(
        "run_satisfied_verified",
        json!({"head": "a", "passed": true, "ask_id": 1}),
        MONDAY + 210 * 60,
    );
    push(
        "run_satisfied_closed",
        json!({"head": "a", "ask_id": 1, "answer": "close"}),
        MONDAY + 225 * 60,
    );
    queue.queue_event(
        marks::SUPERVISOR_STOPPED,
        json!({"supervisor": "s"}),
        MONDAY + 4 * HOUR,
    );
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    let day = result.periods.last().unwrap();
    // The worker's half hour and the verification's, not the wait for the
    // container nor the answer, in four hours of two slots.
    assert_eq!(measure(day, "slot_usage", ALL).value, Some(0.125));
}

/// The slot usage of Monday with one run holding a slot for two hours
/// under a supervisor of two slots started at midnight, once `alive`
/// wrote its end (or left it out).
fn monday_slot_usage(alive: impl FnOnce(&mut Queue)) -> Option<f64> {
    let mut queue = Queue::default();
    start(&mut queue, 2, MONDAY);
    queue.run(&Run::new(
        1,
        Some("runtime".parse::<TaskChange>().unwrap()),
        MONDAY + HOUR,
        2 * HOUR - 100,
    ));
    alive(&mut queue);
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    measure(result.periods.last().unwrap(), "slot_usage", ALL).value
}

/// A supervisor that went stale with neither its stop nor a later start
/// ends at its last heartbeat (ADR-0051 decision 10), not now; a live one
/// is alive until now.
#[test]
fn a_stale_supervisor_ends_at_its_last_heartbeat() {
    // Two hours of one run in four hours of two slots.
    let stale = monday_slot_usage(|queue| {
        queue.heartbeats.insert("s".to_owned(), MONDAY + 4 * HOUR);
    });
    assert_eq!(stale, Some(0.25));
    // Alive (its heartbeat fresh at now): the whole day.
    let live = monday_slot_usage(|queue| {
        queue.heartbeats.insert("s".to_owned(), MONDAY + DAY - 1);
    });
    assert_eq!(live, Some(round3(2.0 / 48.0)));
    // `up` or `down` pruned its row: the stop they recorded ends it at the
    // row's last heartbeat, not at the prune.
    let pruned = monday_slot_usage(|queue| {
        queue.queue_event(
            marks::SUPERVISOR_STOPPED,
            json!({"supervisor": "s", "outcome": "pruned", "last_heartbeat_at": MONDAY + 4 * HOUR}),
            MONDAY + 10 * HOUR,
        );
    });
    assert_eq!(pruned, Some(0.25));
    // Its row went without a stop: it ends at the last event it recorded.
    let gone = monday_slot_usage(|queue| {
        queue.queue_event(
            window::CANDIDATES_SAMPLED,
            json!({"supervisor": "s", "candidates": 0, "free_slots": 2, "ready": 0}),
            MONDAY + 4 * HOUR,
        );
    });
    assert_eq!(gone, Some(0.25));
    // A later start still ends it first.
    let restarted = monday_slot_usage(|queue| {
        queue.heartbeats.insert("s".to_owned(), MONDAY + 6 * HOUR);
        queue.queue_event(
            marks::SUPERVISOR_STARTED,
            json!({"supervisor": "t", "parallel": 2}),
            MONDAY + 4 * HOUR,
        );
        queue.queue_event(
            marks::SUPERVISOR_STOPPED,
            json!({"supervisor": "t"}),
            MONDAY + 4 * HOUR,
        );
    });
    assert_eq!(restarted, Some(0.25));
}

/// Only the `integrate` attempts that ran a verification command count
/// towards `verification_failed_rate`: one stopped before its commands
/// (an empty rebase, a dirty worktree) does not.
#[test]
fn verification_failed_rate_counts_the_attempts_that_verified() {
    let mut queue = Queue::default();
    let landed = queue.run(&Run::new(1, None, MONDAY + HOUR, HOUR));
    let receipt = landed - 100;
    let id = format!("{:08x}-0000-4000-8000-{:012x}", 1, MONDAY + HOUR);
    let (task, id) = (Some(1), Some(id.as_str()));
    let attempt = |queue: &mut Queue, at: i64, verified: Option<i64>, code: &str| {
        queue.push(task, id, "integration_started", json!({}), at);
        queue.push(task, id, "integration_rebased", json!({}), at + 1);
        if let Some(number) = verified {
            queue.push(
                task,
                id,
                "verification_command",
                json!({"phase": "integration", "attempt": number, "index": 1, "exit_code": 1}),
                at + 2,
            );
        }
        queue.push(
            task,
            id,
            "integration_deferred",
            json!({"code": code}),
            at + 3,
        );
    };
    attempt(&mut queue, receipt + 21, None, "rebase_empty");
    attempt(&mut queue, receipt + 25, None, "dirty_worktree");
    attempt(&mut queue, receipt + 30, Some(3), "verification_failed");
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    let rate = measure(
        result.periods.last().unwrap(),
        "verification_failed_rate",
        ALL,
    );
    // The failed attempt 3 and the landing's attempt 1 of the four rebased.
    assert_eq!((rate.n, rate.value), (2, Some(0.5)));
}

/// The details split the backend failures, as `stats` does, into the
/// attempts a retry took up and the calls that gave up, per `op` too;
/// `backend_failures_per_run` still counts them all.
#[test]
fn details_split_the_backend_failures_into_retried_and_exhausted() {
    let mut queue = Queue::default();
    let landed = queue.run(&Run::new(1, None, MONDAY + HOUR, HOUR));
    let failed = |queue: &mut Queue, op: &str, retry: Option<Value>, at: i64| {
        let mut payload = json!({"op": op, "attempt": 1, "max_attempts": 3});
        if let Some(retry) = retry {
            payload["retry_after_ms"] = retry;
        }
        queue.push(Some(1), None, "backend_call_failed", payload, at);
    };
    failed(&mut queue, "capture", Some(json!(2000)), landed - 300);
    failed(&mut queue, "capture", Some(Value::Null), landed - 200);
    failed(&mut queue, "send_text", Some(json!(4000)), landed - 150);
    // Before task 326: no retry_after_ms at all.
    failed(&mut queue, "close", None, landed - 120);
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    let day = result.periods.last().unwrap();
    assert_eq!(
        measure(day, "backend_failures_per_run", ALL).value,
        Some(4.0)
    );
    let details = &day.window.details;
    assert_eq!(
        details["backend_failures_by_op"],
        json!({"capture": 2, "send_text": 1, "close": 1})
    );
    assert_eq!(
        details["backend_failures_retried"],
        json!({"count": 2, "by_op": {"capture": 1, "send_text": 1}})
    );
    assert_eq!(
        details["backend_failures_exhausted"],
        json!({"count": 2, "by_op": {"capture": 1, "close": 1}})
    );
}

/// The integration slot (goal 72): every `integrate` attempt holds it,
/// landed or not, one at a time, so the day's total never exceeds the
/// day; the peak is the busiest hour, and a day not over yet counts up
/// to now.
#[test]
fn landing_utilization_counts_every_attempt_within_the_day() {
    let mut queue = Queue::default();
    // Lands at 01:00 + 30m + 100s, its attempt from 40s before the end.
    let landed = queue.run(&Run::new(1, None, MONDAY + HOUR, 30 * 60));
    let id = format!("{:08x}-0000-4000-8000-{:012x}", 2, MONDAY + HOUR);
    let (task, id) = (Some(2), Some(id.as_str()));
    queue.changes.insert(TaskId::new(2), None);
    queue.goals.insert(TaskId::new(2), None);
    queue.push(task, id, "run_claimed", json!({}), MONDAY + HOUR);
    queue.push(
        task,
        id,
        "landing_queued",
        json!({"via": "exit"}),
        landed - 200,
    );
    // Deferred after 10 minutes: it held the slot all the same.
    queue.push(task, id, "integration_started", json!({}), landed);
    queue.push(
        task,
        id,
        "integration_deferred",
        json!({"code": "verification_failed", "status": "needs_session"}),
        landed + 600,
    );
    // Attempts back to back for 50 hours' worth cannot fill more than the
    // day: a run whose attempts never recorded an end is cut by the next.
    for hour in 3..24 {
        queue.push(
            task,
            id,
            "integration_started",
            json!({}),
            MONDAY + hour * HOUR,
        );
    }
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY + 12 * HOUR, &KpiConfig::default(), &query);
    let day = result.periods.last().unwrap();
    let details = &day.window.details["landing_utilization"];
    let busy = details["busy_secs"].as_i64().unwrap();
    assert!(busy <= details["window_secs"].as_i64().unwrap());
    assert_eq!(details["window_secs"], json!(DAY));
    // 60s landed, 600s deferred, 21 hours of attempts from 03:00.
    assert_eq!(busy, 60 + 600 + 21 * HOUR);
    assert_eq!(details["attempts"], json!(23));
    assert_eq!(details["landed"], json!(1));
    let utilization = measure(day, "landing_utilization", ALL);
    assert_eq!(utilization.value, Some(round3(float(busy) / float(DAY))));
    assert_eq!(utilization.max, Some(1.0));
    assert_eq!(
        measure(day, "landing_utilization.peak", ALL).value,
        Some(1.0)
    );
    assert_eq!(
        details["peak_hour"]["start"],
        marks::utc_text((MONDAY + 3 * HOUR) * 1000)
    );
    // The last attempt is still open at the day's end: 22 ended in it.
    let attempt = measure(day, "landing_attempt", ALL);
    assert_eq!((attempt.n, attempt.median), (22, Some(float(HOUR))));
    assert_eq!(attempt.min, Some(60.0));
    let waiting = measure(day, "landing_queue_depth", ALL);
    assert_eq!((waiting.n, waiting.max), (1, Some(1.0)));
    assert_eq!(waiting.value, Some(round3(200.0 / float(DAY))));

    // Monday not over yet at 04:00: three hours of attempts in four.
    let result = queue.kpi(MONDAY + 4 * HOUR, &KpiConfig::default(), &query);
    let partial = result.periods.last().unwrap();
    assert!(partial.partial);
    let details = &partial.window.details["landing_utilization"];
    assert_eq!(details["window_secs"], json!(4 * HOUR));
    assert_eq!(details["busy_secs"], json!(60 + 600 + HOUR));
}

/// The same process lifetime bounds both the weighted denominator and
/// starvation, including windows entirely within downtime.
#[test]
fn candidates_end_with_their_supervisor_and_exclude_downtime() {
    for ending in [
        "stopped",
        "restart",
        "next_start",
        "stale",
        "pruned",
        "gone",
        "live",
        "held",
        "resumed",
    ] {
        let mut queue = Queue::default();
        start(&mut queue, 2, MONDAY);
        queue.queue_event(
            window::CANDIDATES_SAMPLED,
            json!({"supervisor": "s", "candidates": 2, "free_slots": 1, "ready": 2}),
            MONDAY,
        );
        queue.queue_event(
            window::CANDIDATES_SAMPLED,
            json!({"supervisor": "s", "candidates": 0, "free_slots": 1, "ready": 1}),
            MONDAY + 2 * HOUR,
        );
        match ending {
            "stopped" | "restart" => queue.queue_event(
                marks::SUPERVISOR_STOPPED,
                json!({"supervisor": "s"}),
                MONDAY + 4 * HOUR,
            ),
            "stale" => {
                queue.heartbeats.insert("s".into(), MONDAY + 4 * HOUR);
            }
            "pruned" => queue.queue_event(
                marks::SUPERVISOR_STOPPED,
                json!({"supervisor": "s", "last_heartbeat_at": MONDAY + 4 * HOUR}),
                MONDAY + 10 * HOUR,
            ),
            "gone" => {
                queue.queue_event("claim_held", json!({"supervisor": "s"}), MONDAY + 4 * HOUR)
            }
            "live" | "held" | "resumed" => {
                queue.heartbeats.insert("s".into(), MONDAY + DAY - 1);
            }
            "next_start" => queue.queue_event(
                marks::SUPERVISOR_STARTED,
                json!({"supervisor": "t", "parallel": 2}),
                MONDAY + 4 * HOUR,
            ),
            _ => unreachable!(),
        }
        if matches!(ending, "held" | "resumed") {
            queue.queue_event(window::CANDIDATES_SAMPLED,
                json!({"supervisor": "s", "candidates": 0, "free_slots": 1, "ready": 1, "claims_held": true}), MONDAY + 4 * HOUR);
        }
        if ending == "resumed" {
            queue.queue_event(window::CANDIDATES_SAMPLED,
                json!({"supervisor": "s", "candidates": 0, "free_slots": 1, "ready": 1, "claims_held": false}), MONDAY + 6 * HOUR);
        }
        if matches!(ending, "restart" | "next_start") {
            if ending == "restart" {
                queue.queue_event(
                    marks::SUPERVISOR_STARTED,
                    json!({"supervisor": "t", "parallel": 2}),
                    MONDAY + 8 * HOUR,
                );
            }
            // A different supervisor's stop cannot close t; nor may a
            // late sample from s borrow t's lifetime.
            queue.queue_event(
                marks::SUPERVISOR_STOPPED,
                json!({"supervisor": "other"}),
                MONDAY + 9 * HOUR,
            );
            queue.queue_event(
                window::CANDIDATES_SAMPLED,
                json!({"supervisor": "s", "candidates": 100, "free_slots": 1, "ready": 100}),
                MONDAY + 10 * HOUR,
            );
            queue.queue_event(
                window::CANDIDATES_SAMPLED,
                json!({"supervisor": "t", "candidates": 4, "free_slots": 1, "ready": 4}),
                MONDAY + 12 * HOUR,
            );
            queue.heartbeats.insert("t".into(), MONDAY + DAY - 1);
        }
        let query = KpiQuery {
            at: Some(Cursor::Time(MONDAY * 1000)),
            ..KpiQuery::default()
        };
        let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
        let day = result.periods.last().unwrap();
        let (mean, max, starved) = match ending {
            "live" => (4.0 / 24.0, 2.0, 22 * HOUR),
            "held" => (4.0 / 24.0, 2.0, 2 * HOUR),
            "resumed" => (4.0 / 24.0, 2.0, 20 * HOUR),
            "restart" | "next_start" => (52.0 / 16.0, 4.0, 2 * HOUR),
            _ => (1.0, 2.0, 2 * HOUR),
        };
        assert_eq!(
            measure(day, "candidates", ALL).value,
            Some(round3(mean)),
            "{ending}"
        );
        assert_eq!(measure(day, "candidates", ALL).max, Some(max), "{ending}");
        assert_eq!(
            day.window.details["candidates"]["starved_secs"], starved,
            "{ending}"
        );
        if !matches!(ending, "live" | "held" | "resumed") {
            let gap = queue.kpi(
                MONDAY + DAY,
                &KpiConfig::default(),
                &KpiQuery {
                    since: Some(Cursor::Time((MONDAY + 5 * HOUR) * 1000)),
                    until: Some(Cursor::Time((MONDAY + 7 * HOUR) * 1000)),
                    ..KpiQuery::default()
                },
            );
            assert_eq!(
                gap.periods[0].window.unavailable["candidates"], "no_samples",
                "{ending}"
            );
        }
    }
}

/// Links `finding` to `proposal`, submitted with `tasks` by `by` (a
/// `planner` of the runtime's or a `person`; `None` records no `by`).
fn improvement(
    queue: &mut Queue,
    finding: i64,
    proposal: i64,
    tasks: &[i64],
    by: Option<&str>,
    secs: i64,
) {
    for &task in tasks {
        queue.push(
            Some(task),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "submitted"}),
            secs,
        );
        queue.push(
            Some(task),
            None,
            "task_submitted",
            json!({"proposal_id": proposal}),
            secs,
        );
    }
    let mut link = json!({"finding_id": finding, "from": "open", "to": "proposed",
                          "proposal_id": proposal});
    if let Some(by) = by {
        link["by"] = json!(by);
    }
    queue.queue_event("finding_status_changed", link, secs);
}

fn task_to(queue: &mut Queue, task: i64, to: &str, secs: i64) {
    queue.push(
        Some(task),
        None,
        "task_status_changed",
        json!({"to": to}),
        secs,
    );
}

/// `improvement_proposals` (ADR-0051 decision 25) counts, at each period's
/// end, the proposals of the runtime's planners linked to a finding with a
/// task still to be done: not one whose tasks all ended (completed or
/// canceled), one withdrawn, one a person submitted or resubmitted, one
/// whose last task plan review reopened into another, nor one whose finding
/// a person marked for a new proposal; a proposal of two findings once.
#[test]
fn counts_the_improvement_proposals_running_at_each_periods_end() {
    let mut queue = Queue::default();
    let at = MONDAY + HOUR;
    // Running: one task completed, the other ready.
    improvement(&mut queue, 1, 10, &[101, 102], Some("planner"), at);
    task_to(&mut queue, 101, "completed", at + HOUR);
    task_to(&mut queue, 102, "ready", at + HOUR);
    // Settled on Tuesday: its only task completes, the finding resolved.
    improvement(&mut queue, 2, 11, &[111], Some("planner"), at);
    task_to(&mut queue, 111, "completed", at + DAY);
    queue.queue_event(
        "finding_status_changed",
        json!({"finding_id": 2, "from": "proposed", "to": "resolved", "proposal_id": 11,
               "by": "runtime"}),
        at + DAY + 60,
    );
    // Withdrawn on Monday, its finding open again.
    improvement(&mut queue, 3, 12, &[121], Some("planner"), at);
    queue.push(
        Some(121),
        None,
        "proposal_withdrawn",
        json!({"proposal_id": 12, "from": "submitted"}),
        at + 2 * HOUR,
    );
    task_to(&mut queue, 121, "draft", at + 2 * HOUR);
    // A person's proposal, running.
    improvement(&mut queue, 4, 13, &[131], Some("person"), at);
    // Every task canceled.
    improvement(&mut queue, 5, 14, &[141], Some("planner"), at);
    task_to(&mut queue, 141, "canceled", at + HOUR);
    // Two findings of one proposal, running.
    improvement(&mut queue, 6, 15, &[151], Some("planner"), at);
    improvement(&mut queue, 7, 15, &[], Some("planner"), at);
    // Unlinked on Monday: a person marked its finding for a new proposal.
    improvement(&mut queue, 8, 16, &[161], Some("planner"), at);
    queue.queue_event(
        "finding_status_changed",
        json!({"finding_id": 8, "from": "dismissed", "to": "open", "ask_id": 3}),
        at + 3 * HOUR,
    );
    // Sent back and resubmitted by a person on Monday: no longer counted.
    improvement(&mut queue, 9, 17, &[171], Some("planner"), at);
    task_to(&mut queue, 171, "draft", at + HOUR);
    task_to(&mut queue, 171, "submitted", at + 2 * HOUR);
    queue.push(
        Some(171),
        None,
        "task_submitted",
        json!({"proposal_id": 17, "owner_origin": "person"}),
        at + 2 * HOUR,
    );
    // Its done task completed, its other task reopened by plan review
    // into a proposal of its own on Monday: ended.
    improvement(&mut queue, 10, 18, &[181, 182], Some("planner"), at);
    task_to(&mut queue, 181, "completed", at + HOUR);
    task_to(&mut queue, 182, "submitted", at + 2 * HOUR);
    queue.push(
        Some(182),
        None,
        "task_reopened",
        json!({"proposal_id": 19, "reviewed_proposal_id": 20}),
        at + 2 * HOUR,
    );
    let query = KpiQuery {
        at: Some(Cursor::Time((MONDAY + DAY) * 1000)),
        last: 3,
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + 2 * DAY, &KpiConfig::default(), &query);
    let count = |index: usize| measure(&result.periods[index], "improvement_proposals", ALL).value;
    // Before the queue, Monday (10, 11 and 15) and Tuesday (10 and 15).
    assert_eq!(
        (count(0), count(1), count(2)),
        (Some(0.0), Some(3.0), Some(2.0))
    );
    assert!(result.periods.iter().all(|period| {
        !period
            .window
            .unavailable
            .contains_key("improvement_proposals")
    }));
}

/// A running improvement whose link does not record who submitted it
/// cannot be told from a person's: the KPI has no value then and says
/// why, while one of the same kind that ended does not stop the count.
#[test]
fn an_improvement_of_an_unknown_owner_makes_the_count_unavailable() {
    let mut queue = Queue::default();
    let at = MONDAY + HOUR;
    improvement(&mut queue, 1, 10, &[101], None, at);
    task_to(&mut queue, 101, "completed", at + DAY);
    improvement(&mut queue, 2, 11, &[111], Some("planner"), at);
    let query = KpiQuery {
        at: Some(Cursor::Time((MONDAY + DAY) * 1000)),
        last: 2,
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + 2 * DAY, &KpiConfig::default(), &query);
    let monday = &result.periods[0];
    assert_eq!(measure(monday, "improvement_proposals", ALL).value, None);
    assert_eq!(
        monday.window.unavailable["improvement_proposals"],
        "not_recorded"
    );
    let tuesday = &result.periods[1];
    assert_eq!(
        measure(tuesday, "improvement_proposals", ALL).value,
        Some(1.0)
    );
}
