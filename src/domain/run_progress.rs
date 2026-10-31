//! What a run that `status` and `doctor` list is doing now (goal 98): the
//! phase it is in and since when, from the fold of its events up to now
//! ([`crate::domain::ledger::fold::fold_run`]: the phase of its last
//! `run_phase_changed`, the one the supervisor recorded as it moved), and
//! how it uses its supervisor's slot. The run's persistent status stays
//! what it is; this only reads the events the runtime already records.
//!
//! A run with no `run_phase_changed` (from before the phases were
//! recorded) is not folded: its phase is `None` and it is marked
//! `legacy`. A run whose life ended (`ended`) has no phase. A run nobody
//! leases, or whose lease no working process holds (stale), has no step
//! going on, except a landing it waits for in the queue
//! (`landing_queue`): one nobody leases only when its events queue it to
//! land. This is not `stats`' `watched_phase`, which only times the
//! sessions the supervisor watches.

use crate::domain::EventKind;
use serde::Serialize;

use super::{
    RunEvent, RunHistory, RunStatus, ledger::fold::fold_run, run_phase::Phase, waiting::WaitState,
};

/// The phase of a run that waits only for its landing turn, outside the
/// slots.
const LANDING_QUEUE: &str = Phase::LandingQueue.name();

/// The phase the fold of `events` leaves a run in at unix milliseconds
/// `now_ms`, and since when: the last interval while its life goes on.
/// `None` for a run with no `run_phase_changed` (not folded), and for one
/// whose life ended.
fn current(events: &[RunEvent], now_ms: i64) -> Option<(String, i64)> {
    if !recorded(events) {
        return None;
    }
    let life = fold_run(events, &[], now_ms)?;
    if life.end.is_some() {
        return None;
    }
    life.intervals
        .last()
        .map(|interval| (interval.phase.clone(), interval.start))
}

/// Whether the run's `events` record any phase.
fn recorded(events: &[RunEvent]) -> bool {
    events
        .iter()
        .any(|event| event.kind == EventKind::RunPhaseChanged)
}

/// Whether a run with `lease` can be shown in `phase` in `status`, and
/// with `queued` whether its events queue it to land with no lease
/// ([`RunHistory::queued_to_land`]): without a working process holding
/// its lease only a landing it waits for in the queue goes on, and with
/// no lease at all only one the supervisor lands from its events (a run
/// whose lease was given back is in no queue, ADR-t3049-1).
fn shown(phase: &str, status: RunStatus, lease: Lease, queued: bool) -> bool {
    lease == Lease::Live
        || (phase == LANDING_QUEUE
            && status == RunStatus::AwaitingIntegration
            && (lease == Lease::Stale || queued))
}

/// How a run uses its supervisor's slot, as `status`' `slots` and
/// `waiting` count it ([`WaitState`]): a leased run that does not wait
/// takes a slot (`used`), one that waits only for its landing turn
/// (`landing_queue`, ADR-t1591-1) leaves room for a light task, one that
/// waits for a person is out of it (`waiting`), one whose wait ended waits
/// to go back (`returning`). A run nobody leases has none (`None`).
/// Counting the `slot` of a supervisor's `run_ids` gives its `slots` and
/// `waiting`, but for a landing queue longer than `parallel`, whose runs
/// past `parallel` `slots` counts in `used`; the number of `run_ids` is not `slots.used`.
/// The supervisor counts its landing queue only for a run whose last look
/// found another run landing, so while it holds its landings (and its
/// claims), and before a needed e2e starts, `status`' `landing_queue` may
/// be more than the supervisor's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotUse {
    Used,
    LandingQueue,
    Waiting,
    Returning,
}

impl SlotUse {
    pub fn of(leased: bool, wait: Option<&WaitState>) -> Option<Self> {
        leased.then_some(match wait {
            None => Self::Used,
            Some(WaitState { ended: None, .. }) => Self::Waiting,
            Some(_) => Self::Returning,
        })
    }
}

/// Whether a run in `status` waits only for its landing turn by its events
/// (ADR-t1591-1): awaiting integration, in the phase `landing_queue` its
/// last `run_phase_changed` recorded.
pub fn in_landing_queue(status: RunStatus, events: &[RunEvent]) -> bool {
    status == RunStatus::AwaitingIntegration
        && current(events, 0).is_some_and(|(phase, _)| phase == LANDING_QUEUE)
}

/// The run's lease as [`Progress::of`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lease {
    /// Nobody leases the run.
    None,
    /// A lease whose holder is dead or whose heartbeat is stale: it still
    /// counts in its supervisor's slots, but nothing goes on.
    Stale,
    /// A lease a working process holds.
    Live,
}

/// A listed run's `progress`: the phase its last `run_phase_changed`
/// recorded (a name of [`crate::domain::run_phase::Phase`]), when it
/// started (`since`, unix seconds) and for how long (`elapsed_secs` at
/// `now`), and its slot; `legacy` for a run with no phase recorded, whose
/// phase is null.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Progress {
    pub phase: Option<String>,
    pub since: Option<i64>,
    pub elapsed_secs: Option<i64>,
    pub slot: Option<SlotUse>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub legacy: bool,
}

impl Progress {
    /// The progress of a run in `status` with `lease`, from its events
    /// (oldest first) at `now` (unix seconds).
    pub fn of(status: RunStatus, lease: Lease, events: &[RunEvent], now: i64) -> Self {
        let queued =
            lease == Lease::None && RunHistory::from_events(events).queued_to_land().is_some();
        let current =
            current(events, now * 1000).filter(|(phase, _)| shown(phase, status, lease, queued));
        let since = current.as_ref().map(|(_, start)| start.div_euclid(1000));
        let landing_queue = status == RunStatus::AwaitingIntegration
            && current
                .as_ref()
                .is_some_and(|(phase, _)| phase == LANDING_QUEUE);
        Self {
            phase: current.map(|(phase, _)| phase),
            since,
            elapsed_secs: since.map(|since| (now - since).max(0)),
            slot: SlotUse::of(lease != Lease::None, WaitState::of(events).as_ref()).map(|slot| {
                if slot == SlotUse::Used && landing_queue {
                    SlotUse::LandingQueue
                } else {
                    slot
                }
            }),
            legacy: !recorded(events),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ledger::testkit::{Log, T0, run_id};
    use crate::domain::run_phase::{Attempt, AttemptKind};
    use crate::domain::{EventId, TaskId};
    use serde_json::{Value, json};

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: format!("2026-10-04T00:00:{id:02}.000Z"),
            actor: None,
        }
    }

    /// The phase each record leaves the run in is the phase it recorded,
    /// through a revise, a resume and a person's answer, since the
    /// record's time; the end of its life leaves none.
    #[test]
    fn the_phase_is_the_last_recorded_one_since_its_record() {
        let run = run_id(1);
        let mut log = Log::default();
        let revise = Attempt::of(AttemptKind::Revise, 1);
        let resume = Attempt::of(AttemptKind::Resume, 1);
        let steps: [(Phase, Attempt, RunStatus); 8] = [
            (Phase::Worker, Attempt::FIRST, RunStatus::Running),
            (Phase::Validating, Attempt::FIRST, RunStatus::Validating),
            (
                Phase::Review,
                Attempt::FIRST,
                RunStatus::AwaitingIntegration,
            ),
            (Phase::Revise, revise, RunStatus::AwaitingIntegration),
            (Phase::NeedsSession, revise, RunStatus::NeedsSession),
            (Phase::Resume, resume, RunStatus::NeedsSession),
            (Phase::LandingAnswer, resume, RunStatus::AwaitingIntegration),
            (Phase::Landing, resume, RunStatus::Integrating),
        ];
        for (i, (phase, attempt, status)) in steps.iter().enumerate() {
            let at = i64::try_from(i).unwrap() * 100;
            log.phase_in(1, &run, at, *phase, *attempt, "x");
            let progress = Progress::of(*status, Lease::Live, &log.events, T0 + at + 30);
            assert_eq!(progress.phase.as_deref(), Some(phase.name()), "{i}");
            assert_eq!(
                (progress.since, progress.elapsed_secs),
                (Some(T0 + at), Some(30))
            );
            assert!(!progress.legacy);
        }
        log.phase(1, &run, 900, Phase::Ended, "pushed");
        let ended = Progress::of(RunStatus::Integrated, Lease::Live, &log.events, T0 + 950);
        assert_eq!(
            (ended.phase, ended.since, ended.legacy),
            (None, None, false)
        );
    }

    /// A run with no phase recorded is not folded: its phase is null and
    /// it is marked legacy, its slot as before.
    #[test]
    fn a_run_with_no_phase_recorded_has_none_and_is_legacy() {
        let events = [
            event(1, "run_claimed", json!({})),
            event(2, "review_started", json!({})),
        ];
        let progress = Progress::of(RunStatus::AwaitingIntegration, Lease::Live, &events, 0);
        assert_eq!(
            progress,
            Progress {
                phase: None,
                since: None,
                elapsed_secs: None,
                slot: Some(SlotUse::Used),
                legacy: true,
            }
        );
        let json = serde_json::to_value(&progress).unwrap();
        assert_eq!(json["legacy"], true);
        assert!(!in_landing_queue(RunStatus::AwaitingIntegration, &events));
        // A folded run carries no mark in its JSON.
        let run = run_id(2);
        let mut log = Log::default();
        log.phase(1, &run, 0, Phase::Worker, "x");
        let folded = Progress::of(RunStatus::Running, Lease::Live, &log.events, T0);
        assert!(
            serde_json::to_value(&folded)
                .unwrap()
                .get("legacy")
                .is_none()
        );
    }

    /// Without a live lease no step goes on, but a landing it waits for in
    /// the queue: a stale lease's, or one a `land` answer queued with no
    /// lease (a run whose lease was given back is in no queue,
    /// ADR-t3049-1).
    #[test]
    fn without_a_live_lease_only_a_queued_landing_is_shown() {
        let run = run_id(3);
        let mut log = Log::default();
        log.phase(1, &run, 0, Phase::Review, "x");
        let review = |lease| Progress::of(RunStatus::AwaitingIntegration, lease, &log.events, T0);
        assert_eq!(review(Lease::Live).phase.as_deref(), Some("review"));
        let stale = review(Lease::Stale);
        assert_eq!((stale.phase, stale.slot), (None, Some(SlotUse::Used)));
        let unleased = review(Lease::None);
        assert_eq!((unleased.phase, unleased.slot), (None, None));
        log.phase(1, &run, 10, Phase::LandingQueue, "landing_queued");
        let queued =
            |lease| Progress::of(RunStatus::AwaitingIntegration, lease, &log.events, T0 + 10);
        // Given back: in no queue while nobody leases it.
        assert_eq!(queued(Lease::None).phase, None);
        assert_eq!(queued(Lease::Stale).phase.as_deref(), Some("landing_queue"));
        let live = queued(Lease::Live);
        assert_eq!(
            (live.phase.as_deref(), live.slot),
            (Some("landing_queue"), Some(SlotUse::LandingQueue))
        );
        assert!(in_landing_queue(
            RunStatus::AwaitingIntegration,
            &log.events
        ));
        assert!(!in_landing_queue(RunStatus::NeedsSession, &log.events));
        // A `land` answer queued it: shown with no lease.
        log.add(
            Some(1),
            Some(&run),
            "integration_approved",
            json!({"ask_id": 9, "push": true}),
            20,
        )
        .add(
            Some(1),
            Some(&run),
            "landing_queued",
            json!({"via": "approve", "ask_id": 9}),
            20,
        )
        .phase(1, &run, 20, Phase::LandingQueue, "approved");
        let approved = Progress::of(
            RunStatus::AwaitingIntegration,
            Lease::None,
            &log.events,
            T0 + 20,
        );
        assert_eq!(approved.phase.as_deref(), Some("landing_queue"));
        // Landing, it is in its slot.
        log.phase(1, &run, 30, Phase::Landing, "x");
        assert!(!in_landing_queue(
            RunStatus::AwaitingIntegration,
            &log.events
        ));
    }

    /// A run in its hold's wait (ADR-t1879-1) shows the phase `held`.
    #[test]
    fn a_held_run_shows_its_holds_wait() {
        let run = run_id(4);
        let mut log = Log::default();
        log.phase(1, &run, 0, Phase::Worker, "x")
            .add(Some(1), None, "task_held", json!({"hold_id": 1}), 10)
            .phase(1, &run, 40, Phase::Held, "task_held");
        let progress = Progress::of(RunStatus::Running, Lease::Live, &log.events, T0 + 100);
        assert_eq!(
            (
                progress.phase.as_deref(),
                progress.since,
                progress.elapsed_secs
            ),
            (Some("held"), Some(T0 + 40), Some(60))
        );
    }

    #[test]
    fn the_slot_is_used_waiting_or_returning_as_the_wait_stands() {
        let started = event(
            1,
            "run_waiting_started",
            json!({"phase": "revise", "status": "awaiting_integration", "ask_id": 4, "ask_kind": "worker_question"}),
        );
        let waiting = [started.clone()];
        assert_eq!(
            Progress::of(RunStatus::AwaitingIntegration, Lease::Live, &waiting, 0).slot,
            Some(SlotUse::Waiting)
        );
        let returning = [
            started.clone(),
            event(2, "run_waiting_ended", json!({"cause": "answered"})),
        ];
        assert_eq!(
            Progress::of(RunStatus::AwaitingIntegration, Lease::Live, &returning, 0).slot,
            Some(SlotUse::Returning)
        );
        let back = [
            started,
            event(2, "run_waiting_ended", json!({"cause": "answered"})),
            event(3, "run_slot_regained", json!({})),
        ];
        assert_eq!(
            Progress::of(RunStatus::AwaitingIntegration, Lease::Live, &back, 0).slot,
            Some(SlotUse::Used)
        );
        assert_eq!(
            Progress::of(RunStatus::AwaitingIntegration, Lease::None, &returning, 0).slot,
            None
        );
    }
}
