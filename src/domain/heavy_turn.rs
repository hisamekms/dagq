//! Which of the host's heavy jobs may start now (ADR-t2719-1): the
//! automatic update's gate (its job's build and e2e) and the landing
//! checks (a landing's verification, a direct `integrate`'s included, and
//! the landing recheck) never run at the same time, whichever process of
//! the queue runs them (the turns are its lock files,
//! `application::heavy_turn`). The landing checks still run beside each
//! other. A run’s e2e and landing verification are serial through a separate lock.
//!
//! Landing admission gives a runnable landing priority until the queued
//! update reaches its wait limit (ADR-t5283-4). Only an admitted gate
//! reserves the next turn. An in-flight landing, including a batch and
//! its splits, keeps its turn until it finishes.

use std::time::Duration;

/// A source update may yield to runnable landings for up to two hours
/// after first finding a candidate. The limit grants the next turn; it
/// does not interrupt work or include time spent discovering a candidate.
pub const UPDATE_WAIT_LIMIT: Duration = Duration::from_hours(2);

/// Whether to reserve the next turn for a pending source update. An empty
/// queue and a queue whose entries are all blocked both have no runnable
/// landing. Candidate changes do not change the elapsed wait.
pub fn admit_update(landing_ready: bool, waited: Duration) -> bool {
    !landing_ready || waited >= UPDATE_WAIT_LIMIT
}

/// A kind of the host's heavy jobs that take turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeavyJob {
    /// The automatic update's job from before its build to its e2e gate's
    /// verdict, not through the swap, the handoff and the watch.
    UpdateGate,
    /// A landing's verification commands and the landing recheck.
    LandingCheck,
    /// A run’s e2e in the landing queue.
    RunE2e,
}

impl HeavyJob {
    /// What a log line calls it.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::UpdateGate => "the automatic update's build and e2e gate",
            Self::RunE2e => "a run’s e2e in the landing queue",
            Self::LandingCheck => "the landing checks running (verification, recheck or run e2e)",
        }
    }
}

/// What the supervisor reads of the heavy jobs before it starts one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeavyLook {
    /// An automatic update's job builds or runs its e2e gate.
    pub update_gate_running: bool,
    /// An admitted update has reserved the next turn.
    pub update_gate_waiting: bool,
    /// A landing's verification or a landing recheck runs.
    pub landing_checks_running: bool,
}

/// What the automatic update's job waits for before it starts, `None`
/// when it starts now: only the landing checks that run.
pub const fn update_gate_waits_for(look: HeavyLook) -> Option<HeavyJob> {
    if look.landing_checks_running {
        Some(HeavyJob::LandingCheck)
    } else {
        None
    }
}

/// What a landing check (a landing's verification or the landing recheck)
/// waits for before it starts, `None` when it starts now: the update's
/// gate while it runs, and while it waits for its turn, so a steady flow
/// of landings never keeps it waiting.
pub const fn landing_check_waits_for(look: HeavyLook) -> Option<HeavyJob> {
    if look.update_gate_running || look.update_gate_waiting {
        Some(HeavyJob::UpdateGate)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look(update_running: bool, update_waiting: bool, checks: bool) -> HeavyLook {
        HeavyLook {
            update_gate_running: update_running,
            update_gate_waiting: update_waiting,
            landing_checks_running: checks,
        }
    }

    #[test]
    fn the_update_waits_only_for_the_landing_checks_that_run() {
        assert_eq!(update_gate_waits_for(look(false, false, false)), None);
        assert_eq!(update_gate_waits_for(look(false, true, false)), None);
        assert_eq!(
            update_gate_waits_for(look(false, true, true)),
            Some(HeavyJob::LandingCheck)
        );
        assert_eq!(
            update_gate_waits_for(look(false, false, true)),
            Some(HeavyJob::LandingCheck)
        );
    }

    #[test]
    fn a_landing_check_waits_for_the_gate_running_or_waiting_its_turn() {
        assert_eq!(landing_check_waits_for(look(false, false, false)), None);
        // Other landing checks running do not hold it back.
        assert_eq!(landing_check_waits_for(look(false, false, true)), None);
        assert_eq!(
            landing_check_waits_for(look(true, false, false)),
            Some(HeavyJob::UpdateGate)
        );
        assert_eq!(
            landing_check_waits_for(look(false, true, true)),
            Some(HeavyJob::UpdateGate)
        );
    }

    #[test]
    fn a_runnable_landing_has_priority_until_the_update_wait_limit() {
        assert!(!admit_update(true, Duration::ZERO));
        assert!(!admit_update(true, Duration::from_secs(7_199)));
        assert!(admit_update(true, Duration::from_secs(7_200)));
        assert!(admit_update(true, Duration::from_secs(7_201)));
    }

    #[test]
    fn the_update_can_start_on_an_empty_or_entirely_blocked_queue() {
        assert!(admit_update(false, Duration::ZERO));
    }

    #[test]
    fn each_kind_is_described_for_the_log() {
        assert!(HeavyJob::UpdateGate.describe().contains("e2e gate"));
        assert!(HeavyJob::LandingCheck.describe().contains("verification"));
    }
}
