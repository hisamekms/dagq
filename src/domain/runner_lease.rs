//! Whether the control side accepts a runner's report on a run, under the
//! run's fenced lease (ADR-t3987-1 decisions 1 and 2). Each grant of a run to
//! a runner advances the run's generation by one; every report carries the
//! generation it was granted, and only a report of the current generation,
//! from the runner it was granted to, while the lease is held and before its
//! end, is accepted. A lease that ran out, was released or was lost never
//! comes back at its generation: the next grant takes a new one. The lease's
//! end is the control side's clock alone, passed in as `now_ms`; no time a
//! runner sends enters a judgment.
//!
//! This lease is neither the local `run_leases` (token, pid and heartbeat of
//! a session on the host) nor the write leader's lease
//! ([`super::write_leader`]); it follows the same principle of refusing an
//! older holder by its generation. The local default deployment writes no
//! runner lease. The lease names a runner of [`super::runner_registration`],
//! whose records the runner tables keep beside the lease's.

use std::time::Duration;

/// How long a runner lease lasts from its grant or its last accepted report:
/// four and a half [`REPORT_INTERVAL`]s, so three late liveness reports in a
/// row do not lose a run that a runner still works on, while a runner that
/// stopped is noticed within about a minute and a half.
pub const LEASE: Duration = Duration::from_secs(90);

/// How often a runner reports that it is alive while it holds a lease.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(20);

const _: () = assert!(LEASE.as_secs() >= 4 * REPORT_INTERVAL.as_secs());

/// Where a lease stands, apart from its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseState {
    /// Granted and neither returned nor lost; it still runs out at its end.
    Held,
    /// The runner gave it back (a stop of its own).
    Released,
    /// The control side took it away (a revoked runner, for one).
    Lost,
}

impl LeaseState {
    /// The value stored in the lease's `state` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Released => "released",
            Self::Lost => "lost",
        }
    }

    /// The state of a stored `state` value, `None` for an unknown one.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "held" => Some(Self::Held),
            "released" => Some(Self::Released),
            "lost" => Some(Self::Lost),
            _ => None,
        }
    }
}

/// A run's current lease: the one of its latest generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerLease {
    pub run_id: String,
    /// The fencing number, from 1 at the run's first grant.
    pub generation: i64,
    /// The runner the generation was granted to.
    pub runner_id: String,
    /// The lease's end in Unix milliseconds of the control side's clock.
    pub expires_at_ms: i64,
    pub state: LeaseState,
}

/// What a report of a runner on a run claims: the generation it was granted
/// and the runner it comes from (the runner's token says which).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report<'a> {
    pub runner_id: &'a str,
    pub generation: i64,
}

/// The answer to a report, an extension or a return of a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The report is of the current lease; accepting it extends the lease
    /// to `expires_at_ms`.
    Accept { expires_at_ms: i64 },
    /// Not of the current lease: an older or a newer generation, another
    /// runner, a lease that ran out, was released or was lost, or a run with
    /// no lease. Nothing is written for it (`lease_lost`).
    LeaseLost,
}

/// Whether `report` is of `current`, judged at the control side's `now_ms`.
/// Pure: the liveness report, the result report and the report of a run
/// branch's update all pass through it, and the store's update is
/// conditioned on the same generation, runner and state.
pub fn judge(current: Option<&RunnerLease>, report: Report<'_>, now_ms: i64) -> Verdict {
    match current {
        Some(lease)
            if lease.generation == report.generation
                && lease.runner_id == report.runner_id
                && lease.state == LeaseState::Held
                && now_ms < lease.expires_at_ms =>
        {
            Verdict::Accept {
                expires_at_ms: lease_end(now_ms),
            }
        }
        _ => Verdict::LeaseLost,
    }
}

/// Whether the control side may grant the run to a runner now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    /// Grant `generation`, one above the current one (1 for a run never
    /// granted), whose lease ends at `expires_at_ms`.
    Grant { generation: i64, expires_at_ms: i64 },
    /// The current generation's lease is still held until `until_ms`.
    Held { generation: i64, until_ms: i64 },
}

/// Whether to grant the run whose current lease is `current` at `now_ms`.
/// A held lease that ran out is lost by the grant: its generation is passed
/// and never comes back.
pub fn grant(current: Option<&RunnerLease>, now_ms: i64) -> Grant {
    match current {
        Some(lease) if lease.state == LeaseState::Held && now_ms < lease.expires_at_ms => {
            Grant::Held {
                generation: lease.generation,
                until_ms: lease.expires_at_ms,
            }
        }
        _ => Grant::Grant {
            generation: current.map_or(1, |lease| lease.generation.saturating_add(1)),
            expires_at_ms: lease_end(now_ms),
        },
    }
}

/// Whether the control side may take away the lease of `generation` (a
/// revoked runner's, for one), whether or not it ran out: only the held
/// lease of the current generation, answered with the end it had; any
/// other is [`Verdict::LeaseLost`], so a lease is lost at most once.
pub fn lose(current: Option<&RunnerLease>, generation: i64) -> Verdict {
    match current {
        Some(lease) if lease.generation == generation && lease.state == LeaseState::Held => {
            Verdict::Accept {
                expires_at_ms: lease.expires_at_ms,
            }
        }
        _ => Verdict::LeaseLost,
    }
}

/// Whether a verified ref of `generation` may be recorded for the run whose
/// current lease is `current`: a hand-over verifies the next generation's ref
/// before it grants that generation, so from 1 up to one above the current
/// generation (1 for a run never granted) and no further.
pub fn ref_generation_allowed(current: Option<&RunnerLease>, generation: i64) -> bool {
    let next = current.map_or(1, |lease| lease.generation.saturating_add(1));
    (1..=next).contains(&generation)
}

fn lease_end(now_ms: i64) -> i64 {
    now_ms.saturating_add(LEASE.as_millis() as i64)
}

/// A run branch head the control side accepted from a report of the
/// current lease. Resume, review, verification and landing use the newest
/// one, never the tip of the branch on GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedHead {
    pub generation: i64,
    pub head: String,
}

/// A generation whose ref the control side read back from GitHub and found
/// to point at `head`. A hand-over starts again from the newest one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRef {
    pub generation: i64,
    pub head: String,
}

/// What the control side recorded for one generation of a run: the newest
/// head accepted from that generation's reports, and the head its ref was
/// read back at from GitHub. A generation's ref may be verified before its
/// lease is granted, so a record does not imply a lease.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenerationRecord {
    pub generation: i64,
    pub accepted_head: Option<String>,
    pub verified_ref_head: Option<String>,
}

/// The head to record for a report with `head` and `verdict`: only an
/// accepted report's head, so a refused report's head never becomes an
/// accepted head.
pub fn head_to_record(verdict: Verdict, head: Option<&str>) -> Option<&str> {
    match verdict {
        Verdict::Accept { .. } => head,
        Verdict::LeaseLost => None,
    }
}

/// The newest accepted head of a run: the one of its highest generation that
/// accepted any. A later generation that has accepted nothing yet leaves the
/// earlier one's head standing.
pub fn newest_accepted_head(records: &[GenerationRecord]) -> Option<AcceptedHead> {
    records
        .iter()
        .filter_map(|r| {
            r.accepted_head.as_ref().map(|head| AcceptedHead {
                generation: r.generation,
                head: head.clone(),
            })
        })
        .max_by_key(|h| h.generation)
}

/// The newest generation whose ref was verified; a generation without the
/// mark is never returned.
pub fn newest_verified_ref(records: &[GenerationRecord]) -> Option<VerifiedRef> {
    records
        .iter()
        .filter_map(|r| {
            r.verified_ref_head.as_ref().map(|head| VerifiedRef {
                generation: r.generation,
                head: head.clone(),
            })
        })
        .max_by_key(|v| v.generation)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(generation: i64, expires_at_ms: i64, state: LeaseState) -> RunnerLease {
        RunnerLease {
            run_id: "run".into(),
            generation,
            runner_id: "runner-a".into(),
            expires_at_ms,
            state,
        }
    }

    fn report(generation: i64) -> Report<'static> {
        Report {
            runner_id: "runner-a",
            generation,
        }
    }

    #[test]
    fn a_report_of_the_current_generation_within_its_end_is_accepted() {
        let current = lease(3, 10_000, LeaseState::Held);
        assert_eq!(
            judge(Some(&current), report(3), 9_999),
            Verdict::Accept {
                expires_at_ms: 9_999 + LEASE.as_millis() as i64
            }
        );
    }

    #[test]
    fn every_other_report_is_lease_lost() {
        let held = lease(3, 10_000, LeaseState::Held);
        // An older and a newer generation.
        assert_eq!(judge(Some(&held), report(2), 5_000), Verdict::LeaseLost);
        assert_eq!(judge(Some(&held), report(4), 5_000), Verdict::LeaseLost);
        // Another runner with the current generation.
        let other = Report {
            runner_id: "runner-b",
            generation: 3,
        };
        assert_eq!(judge(Some(&held), other, 5_000), Verdict::LeaseLost);
        // A lease that ran out, at its end and after it.
        assert_eq!(judge(Some(&held), report(3), 10_000), Verdict::LeaseLost);
        assert_eq!(judge(Some(&held), report(3), 10_001), Verdict::LeaseLost);
        // A returned and a lost lease, even before their end.
        for state in [LeaseState::Released, LeaseState::Lost] {
            let current = lease(3, 10_000, state);
            assert_eq!(judge(Some(&current), report(3), 5_000), Verdict::LeaseLost);
        }
        // A run with no lease.
        assert_eq!(judge(None, report(1), 5_000), Verdict::LeaseLost);
    }

    #[test]
    fn each_grant_advances_the_generation_and_an_old_one_does_not_come_back() {
        let Grant::Grant {
            generation: first, ..
        } = grant(None, 0)
        else {
            panic!("a run never granted is granted")
        };
        assert_eq!(first, 1);
        let held = lease(first, 1_000, LeaseState::Held);
        assert_eq!(
            grant(Some(&held), 999),
            Grant::Held {
                generation: 1,
                until_ms: 1_000
            }
        );
        for current in [
            lease(1, 1_000, LeaseState::Held),
            lease(1, 5_000, LeaseState::Released),
            lease(1, 5_000, LeaseState::Lost),
        ] {
            let Grant::Grant { generation, .. } = grant(Some(&current), 1_000) else {
                panic!("a lease no longer held is granted again")
            };
            assert_eq!(generation, 2);
            // The old generation's report stays refused once passed.
            let next = RunnerLease {
                generation,
                ..lease(
                    generation,
                    1_000 + LEASE.as_millis() as i64,
                    LeaseState::Held,
                )
            };
            assert_eq!(judge(Some(&next), report(1), 1_001), Verdict::LeaseLost);
            assert!(matches!(
                judge(Some(&next), report(2), 1_001),
                Verdict::Accept { .. }
            ));
        }
    }

    #[test]
    fn states_round_trip_their_stored_values() {
        for state in [LeaseState::Held, LeaseState::Released, LeaseState::Lost] {
            assert_eq!(LeaseState::parse(state.as_str()), Some(state));
        }
        assert_eq!(LeaseState::parse("other"), None);
    }

    #[test]
    fn only_accepted_heads_are_recorded_and_the_newest_is_read() {
        let accepted = Verdict::Accept { expires_at_ms: 1 };
        assert_eq!(head_to_record(accepted, Some("aaa")), Some("aaa"));
        assert_eq!(head_to_record(accepted, None), None);
        assert_eq!(head_to_record(Verdict::LeaseLost, Some("bbb")), None);
        let records = [
            GenerationRecord {
                generation: 1,
                accepted_head: Some("aaa".into()),
                verified_ref_head: Some("aaa".into()),
            },
            GenerationRecord {
                generation: 2,
                accepted_head: Some("ccc".into()),
                verified_ref_head: None,
            },
            GenerationRecord {
                generation: 3,
                accepted_head: None,
                verified_ref_head: Some("ccc".into()),
            },
        ];
        assert_eq!(
            newest_accepted_head(&records),
            Some(AcceptedHead {
                generation: 2,
                head: "ccc".into()
            })
        );
        assert_eq!(
            newest_verified_ref(&records),
            Some(VerifiedRef {
                generation: 3,
                head: "ccc".into()
            })
        );
        assert_eq!(newest_verified_ref(&records[1..2]), None);
        assert_eq!(newest_accepted_head(&[]), None);
    }

    #[test]
    fn only_the_held_lease_of_the_current_generation_is_lost() {
        assert_eq!(lose(None, 1), Verdict::LeaseLost);
        let held = lease(3, 10_000, LeaseState::Held);
        assert_eq!(
            lose(Some(&held), 3),
            Verdict::Accept {
                expires_at_ms: 10_000
            }
        );
        // A lease that ran out is still the held one and may be lost.
        let ran_out = lease(3, 0, LeaseState::Held);
        assert!(matches!(lose(Some(&ran_out), 3), Verdict::Accept { .. }));
        assert_eq!(lose(Some(&held), 2), Verdict::LeaseLost);
        assert_eq!(lose(Some(&held), 4), Verdict::LeaseLost);
        for state in [LeaseState::Released, LeaseState::Lost] {
            assert_eq!(lose(Some(&lease(3, 10_000, state)), 3), Verdict::LeaseLost);
        }
    }

    #[test]
    fn a_verified_ref_goes_up_to_one_above_the_current_generation() {
        assert!(!ref_generation_allowed(None, 0));
        assert!(ref_generation_allowed(None, 1));
        assert!(!ref_generation_allowed(None, 2));
        for state in [LeaseState::Held, LeaseState::Released, LeaseState::Lost] {
            let current = lease(3, 10_000, state);
            assert!(!ref_generation_allowed(Some(&current), 0));
            assert!(ref_generation_allowed(Some(&current), 1));
            assert!(ref_generation_allowed(Some(&current), 3));
            assert!(ref_generation_allowed(Some(&current), 4));
            assert!(!ref_generation_allowed(Some(&current), 5));
        }
    }
}
