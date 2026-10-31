//! The readers of a run's phases agree with the one fold
//! ([`super::fold::fold_run`]): for the same run, `timeline`'s gaps,
//! `status`' `progress` and the forecast's run in flight name the phase of
//! the fold's interval they fall in, for a run that lands at its first
//! try, one sent back to a revise, one resumed and one that waits for the
//! answer to its `approve_landing` ask; and a run in its task's hold's
//! wait reads as `held` in each.

use serde_json::json;

use super::fold::{RunLife, fold_run};
use super::testkit::{Log, T0, ms, run_id};
use crate::domain::forecast::running;
use crate::domain::run_phase::{Attempt, AttemptKind, Phase};
use crate::domain::run_progress::{Lease, Progress};
use crate::domain::stats::timestamp_millis;
use crate::domain::timeline::gaps;
use crate::domain::{RunEvent, RunStatus};

/// A run's moves: each phase in its attempt at seconds from [`T0`], with
/// an event of the run in each interval (so each is a gap of its own),
/// ended at `end`.
fn run(n: u8, moves: &[(i64, Phase, Attempt)], end: i64) -> (String, Log) {
    let r = run_id(n);
    let mut log = Log::default();
    log.add(Some(1), Some(&r), "run_claimed", json!({}), 0);
    for (secs, phase, attempt) in moves {
        log.phase_in(1, &r, *secs, *phase, *attempt, "x");
        log.add(Some(1), Some(&r), "heartbeat_noted", json!({}), secs + 1);
    }
    log.phase(1, &r, end, Phase::Ended, "pushed");
    (r, log)
}

/// The run's events up to `secs` after [`T0`].
fn upto(events: &[RunEvent], secs: i64) -> Vec<RunEvent> {
    events
        .iter()
        .filter(|event| timestamp_millis(&event.created_at).is_some_and(|at| at <= ms(secs)))
        .cloned()
        .collect()
}

/// Each reader names the phase of the fold's interval it falls in.
fn agree(events: &[RunEvent], end: i64) {
    let life: RunLife = fold_run(events, &[], ms(end)).unwrap();
    assert_eq!(life.end, Some(ms(end)));
    // Every gap of the timeline starts in an interval and has its phase.
    let found = gaps(events, 1, None);
    assert!(!found.is_empty());
    for gap in &found {
        let at = timestamp_millis(&gap.from).unwrap();
        let interval = life
            .intervals
            .iter()
            .rev()
            .find(|interval| interval.start <= at && at < interval.end);
        match interval {
            Some(interval) => assert_eq!(gap.reason, interval.phase, "{gap:?}"),
            None => assert_eq!(gap.reason, "ended", "{gap:?}"),
        }
    }
    // Midway through each interval, status and the forecast read its phase
    // from the events up to then.
    for interval in &life.intervals {
        let mid = (interval.start + interval.end) / 2 / 1000 - T0;
        let seen = upto(events, mid);
        let progress = Progress::of(RunStatus::Running, Lease::Live, &seen, T0 + mid);
        assert_eq!(progress.phase.as_deref(), Some(interval.phase.as_str()));
        assert_eq!(progress.since, Some(interval.start / 1000));
        let in_flight = running(&seen, ms(mid));
        assert_eq!(in_flight.phase.as_deref(), Some(interval.phase.as_str()));
    }
    // After its end, neither has a phase.
    assert_eq!(
        Progress::of(RunStatus::Integrated, Lease::Live, events, T0 + end).phase,
        None
    );
    assert_eq!(running(events, ms(end)).phase, None);
}

#[test]
fn a_run_landed_at_its_first_try() {
    let first = Attempt::FIRST;
    let (_, log) = run(
        1,
        &[
            (0, Phase::Provisioning, first),
            (10, Phase::Worker, first),
            (600, Phase::Validating, first),
            (660, Phase::Review, first),
            (900, Phase::AwaitingSlot, first),
            (1000, Phase::Landing, first),
            (1200, Phase::Push { in_slot: true }, first),
        ],
        1230,
    );
    agree(&log.events, 1230);
}

#[test]
fn a_run_sent_back_to_a_revise() {
    let first = Attempt::FIRST;
    let revise = Attempt::of(AttemptKind::Revise, 1);
    let (_, log) = run(
        2,
        &[
            (0, Phase::Worker, first),
            (600, Phase::Validating, first),
            (660, Phase::Review, first),
            (900, Phase::Revise, revise),
            (1500, Phase::Validating, revise),
            (1560, Phase::Review, revise),
            (1800, Phase::Landing, revise),
            (2000, Phase::Push { in_slot: true }, revise),
        ],
        2030,
    );
    agree(&log.events, 2030);
}

#[test]
fn a_run_resumed() {
    let first = Attempt::FIRST;
    let resume = Attempt::of(AttemptKind::Resume, 1);
    let (_, log) = run(
        3,
        &[
            (0, Phase::Worker, first),
            (600, Phase::Validating, first),
            (660, Phase::E2e, first),
            (900, Phase::NeedsSession, first),
            (1200, Phase::Resume, resume),
            (1800, Phase::Validating, resume),
            (1860, Phase::Landing, resume),
            (2000, Phase::Push { in_slot: false }, resume),
        ],
        2030,
    );
    agree(&log.events, 2030);
}

#[test]
fn a_run_that_waits_for_its_approve_landing_answer() {
    let first = Attempt::FIRST;
    let (_, log) = run(
        4,
        &[
            (0, Phase::Worker, first),
            (600, Phase::Validating, first),
            (660, Phase::Review, first),
            (900, Phase::LandingAnswer, first),
            (4000, Phase::LandingQueue, first),
            (4100, Phase::Landing, first),
            (4300, Phase::Push { in_slot: false }, first),
        ],
        4330,
    );
    agree(&log.events, 4330);
    // A person's answer is the human's time: the timeline's gap and the
    // forecast's wait.
    let waiting = gaps(&log.events, 1, None)
        .into_iter()
        .find(|gap| gap.reason == "landing_answer")
        .unwrap();
    assert_eq!(
        waiting.blocker,
        Some(crate::domain::run_phase::Blocker::Human)
    );
    let in_flight = running(&upto(&log.events, 2000), ms(2000));
    assert_eq!(in_flight.waiting, Some(1100));
}

/// The wait of a run for its task's hold (ADR-t1879-1) reads as `held` in
/// the timeline's gap and in `status`' progress, as before the fold: the
/// gap's reason and the progress' phase are `held`, from its record.
#[test]
fn a_run_in_its_holds_wait_reads_as_held() {
    let first = Attempt::FIRST;
    let (_, log) = run(
        5,
        &[
            (0, Phase::Worker, first),
            (600, Phase::Held, first),
            (7800, Phase::Worker, first),
            (8400, Phase::Validating, first),
            (8460, Phase::Landing, first),
            (8600, Phase::Push { in_slot: true }, first),
        ],
        8630,
    );
    agree(&log.events, 8630);
    let held = gaps(&log.events, 300, None)
        .into_iter()
        .find(|gap| gap.reason == "held")
        .unwrap();
    assert_eq!(held.secs, 7800 - 601);
    let progress = Progress::of(
        RunStatus::Running,
        Lease::Live,
        &upto(&log.events, 1000),
        T0 + 1000,
    );
    assert_eq!(
        (
            progress.phase.as_deref(),
            progress.since,
            progress.elapsed_secs
        ),
        (Some("held"), Some(T0 + 600), Some(400))
    );
}
