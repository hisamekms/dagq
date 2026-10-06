//! Whether a run whose review passed starts its landing now, waits for it
//! leased, or (in a supervisor that drains or hands off) goes back to a
//! person awaiting integration: a missing program of `[run.env]` (ADR-0049
//! decision 9), a CI that cannot be read (ADR-t1920-1 decision 2), an
//! unresolved landing branch (ADR-t615-1) or short free disk space (task
//! 377) hold its landing. A drain hands a run back only for a hold that
//! does not end by itself: not for a shortage a cleanup is still curing
//! (task 648, task 1426), nor for a CI check that has not answered yet.

/// What holds landings now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LandingHoldInputs {
    pub run_env_missing: bool,
    /// `[ci_watch]` is set and this process has no answer yet or the last
    /// one found the means missing.
    pub ci_held: bool,
    /// The last answer found the means missing.
    pub ci_unreadable: bool,
    pub landing_unresolved: bool,
    pub landing_short: bool,
    /// A cleanup for room runs (or the rest of one another job took on).
    pub disk_cleaning: bool,
    /// The supervisor drains or hands off.
    pub draining: bool,
}

/// What becomes of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandingHold {
    /// Nothing holds it.
    Proceed,
    /// It waits leased for the hold to end.
    Wait,
    /// The lease goes back and a person takes it, for this reason.
    HandBack(&'static str),
}

/// See the module's documentation.
pub const fn judge(inputs: LandingHoldInputs) -> LandingHold {
    let held = inputs.run_env_missing
        || inputs.ci_held
        || inputs.landing_unresolved
        || inputs.landing_short;
    if !held {
        return LandingHold::Proceed;
    }
    if !inputs.draining {
        return LandingHold::Wait;
    }
    if inputs.run_env_missing {
        LandingHold::HandBack("a program [run.env] names is missing")
    } else if inputs.ci_unreadable {
        LandingHold::HandBack("the CI [ci_watch] watches cannot be read")
    } else if inputs.landing_unresolved {
        LandingHold::HandBack("the landing branch does not resolve")
    } else if inputs.landing_short && !inputs.disk_cleaning {
        LandingHold::HandBack("the free disk space is short of what its verification needs")
    } else {
        LandingHold::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draining(inputs: LandingHoldInputs) -> LandingHold {
        judge(LandingHoldInputs {
            draining: true,
            ..inputs
        })
    }

    #[test]
    fn nothing_held_proceeds_and_a_hold_waits_outside_a_drain() {
        assert_eq!(judge(LandingHoldInputs::default()), LandingHold::Proceed);
        let held = LandingHoldInputs {
            ci_held: true,
            ci_unreadable: true,
            ..Default::default()
        };
        assert_eq!(judge(held), LandingHold::Wait);
        assert_eq!(draining(LandingHoldInputs::default()), LandingHold::Proceed);
    }

    #[test]
    fn a_drain_hands_back_a_lasting_hold_with_its_reason() {
        let cases = [
            (
                LandingHoldInputs {
                    run_env_missing: true,
                    ci_held: true,
                    ci_unreadable: true,
                    ..Default::default()
                },
                "a program [run.env] names is missing",
            ),
            (
                LandingHoldInputs {
                    ci_held: true,
                    ci_unreadable: true,
                    ..Default::default()
                },
                "the CI [ci_watch] watches cannot be read",
            ),
            (
                LandingHoldInputs {
                    landing_unresolved: true,
                    ..Default::default()
                },
                "the landing branch does not resolve",
            ),
            (
                LandingHoldInputs {
                    landing_short: true,
                    ..Default::default()
                },
                "the free disk space is short of what its verification needs",
            ),
        ];
        for (inputs, why) in cases {
            assert_eq!(draining(inputs), LandingHold::HandBack(why), "{inputs:?}");
        }
    }

    #[test]
    fn a_drain_waits_for_a_first_ci_answer_and_a_cleanup_in_progress() {
        // The first CI check has not answered: no gh is known missing.
        let pending = LandingHoldInputs {
            ci_held: true,
            ..Default::default()
        };
        assert_eq!(draining(pending), LandingHold::Wait);
        // A shortage a cleanup is curing, alone or with a pending check.
        let cleaning = LandingHoldInputs {
            landing_short: true,
            disk_cleaning: true,
            ..Default::default()
        };
        assert_eq!(draining(cleaning), LandingHold::Wait);
        assert_eq!(
            draining(LandingHoldInputs {
                ci_held: true,
                ..cleaning
            }),
            LandingHold::Wait
        );
        // A pending check with a shortage nobody cures: the shortage.
        assert_eq!(
            draining(LandingHoldInputs {
                ci_held: true,
                landing_short: true,
                ..Default::default()
            }),
            LandingHold::HandBack("the free disk space is short of what its verification needs")
        );
    }
}
