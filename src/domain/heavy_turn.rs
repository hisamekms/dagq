//! Which of the host's heavy jobs may start now (ADR-t2719-1): the
//! automatic update's gate (its job's build and e2e) and the landing
//! checks (a landing's verification, a direct `integrate`'s included, and
//! the landing recheck) never run at the same time, whichever process of
//! the queue runs them (the turns are its lock files,
//! `application::heavy_turn`). The landing checks still run beside each
//! other, as they did; only the two kinds take turns.
//!
//! The order: the update's gate waits only for the landing checks that
//! run, and once it waits, no landing check starts until its job built and
//! ran its e2e. So the update waits for the checks already running and no
//! longer, however often runs land; a landing check waits for one job at a
//! time, and the next job needs main to move again.

/// A kind of the host's heavy jobs that take turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeavyJob {
    /// The automatic update's job from before its build to its e2e gate's
    /// verdict, not through the swap, the handoff and the watch.
    UpdateGate,
    /// A landing's verification commands and the landing recheck.
    LandingCheck,
}

impl HeavyJob {
    /// What a log line calls it.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::UpdateGate => "the automatic update's build and e2e gate",
            Self::LandingCheck => {
                "the landing checks running (a landing's verification or the landing recheck)"
            }
        }
    }
}

/// What the supervisor reads of the heavy jobs before it starts one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeavyLook {
    /// An automatic update's job builds or runs its e2e gate.
    pub update_gate_running: bool,
    /// The automatic update has a head to build and waits for its turn.
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

    /// Landings that keep coming do not starve the update: once it waits,
    /// the checks running end and none starts, so its next look starts it;
    /// while its gate runs, the landings wait, and they go on once it ended.
    #[test]
    fn a_flow_of_landings_takes_turns_with_the_update() {
        // A landing's verification runs; the update finds a head to build.
        let mut state = look(false, false, true);
        assert_eq!(update_gate_waits_for(state), Some(HeavyJob::LandingCheck));
        state.update_gate_waiting = true;
        // The next landing is ready while the first still verifies: it waits.
        assert_eq!(landing_check_waits_for(state), Some(HeavyJob::UpdateGate));
        // The verification ends: the update starts, the landing still waits.
        state.landing_checks_running = false;
        assert_eq!(update_gate_waits_for(state), None);
        state = look(true, false, false);
        assert_eq!(landing_check_waits_for(state), Some(HeavyJob::UpdateGate));
        // The gate ends: the landing starts.
        state.update_gate_running = false;
        assert_eq!(landing_check_waits_for(state), None);
    }

    #[test]
    fn each_kind_is_described_for_the_log() {
        assert!(HeavyJob::UpdateGate.describe().contains("e2e gate"));
        assert!(HeavyJob::LandingCheck.describe().contains("verification"));
    }
}
