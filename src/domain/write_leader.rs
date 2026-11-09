//! Whether a process of the queue service may take over the write leader's
//! row (ADR-t3986-1 decisions 2 and 4). Safety rests on the generation
//! alone: a takeover advances it, and an older generation's writes are
//! refused by the row's conditional update whatever the clocks say. The
//! lease's end only says when a process that is not the leader may take
//! over, judged by the DB's clock; a SQLite store whose process holds the
//! service's lock file in the DB's own directory has proof that no earlier
//! leader lives, and takes over without waiting.

use std::time::Duration;

/// How long a lease lasts from its takeover or its last extension: three
/// times [`RENEW_INTERVAL`], so two late extensions in a row do not lose the
/// leader. Longer than a DB failover (RDS Multi-AZ, about 1 to 2 minutes)
/// would not shorten the stop, since nobody writes while the DB fails over.
/// A local SQLite queue takes over on its lock file's proof, so this value
/// does not lengthen a local restart.
pub const LEASE: Duration = Duration::from_secs(30);

/// How often a leader with no write to make extends its lease by a write
/// of the row alone; every write transaction extends it too.
pub const RENEW_INTERVAL: Duration = Duration::from_secs(10);

/// How often a process that is not the leader tries to take over: with
/// [`LEASE`], writes stop for at most about 35 seconds when a leader dies.
pub const TAKEOVER_RETRY: Duration = Duration::from_secs(5);

/// How long a planned stop may take to empty the write queue before the
/// leader releases the row: inside ECS's default stop grace of 30 seconds.
pub const RELEASE_DRAIN_LIMIT: Duration = Duration::from_secs(20);

/// Stop callers allow the whole drain plus five seconds for release and
/// cleanup, still inside the deployment's thirty-second stop grace.
pub const SERVICE_STOP_GRACE: Duration = RELEASE_DRAIN_LIMIT.saturating_add(Duration::from_secs(5));

/// A caller's start/probe timeout must never shorten an accepted write's drain.
pub fn stop_grace(requested: Duration) -> Duration {
    requested.max(SERVICE_STOP_GRACE)
}

/// The DB clock and committed lease decide whether a renewal is needed.
/// Refused requests do not change this clock; a committed write does.
pub fn lease_renewal_due(lease_until_ms: i64, now_ms: i64) -> bool {
    lease_until_ms.saturating_sub(now_ms) <= (LEASE - RENEW_INTERVAL).as_millis() as i64
}

const _: () = assert!(LEASE.as_millis() >= 3 * RENEW_INTERVAL.as_millis());
const _: () = assert!(RELEASE_DRAIN_LIMIT.as_secs() < 30);

/// The write leader's row as it stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteLeader {
    /// The leader's identifier, `None` while no process has ever led.
    pub holder: Option<String>,
    /// The fencing number; 0 while no process has led.
    pub generation: i64,
    /// The lease's end in Unix milliseconds of the DB's clock; 0 when
    /// released or never taken.
    pub lease_until_ms: i64,
    /// When the generation was taken, by the DB's clock; observation only.
    pub acquired_at_ms: Option<i64>,
}

/// What a process that is not the leader does about the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Takeover {
    /// Take it over: the lease ran out, was released or never taken, or the
    /// process has the local proof.
    Take,
    /// Wait: another leader's lease runs until `until_ms` by the DB's clock.
    Wait { until_ms: i64 },
}

/// Whether to take over `row`, with the DB's clock at `now_ms` and
/// `local_proof` true only for a SQLite store whose process holds the
/// service's lock file in the DB's directory. Pure: the takeover's own
/// conditional update ([`crate::application::WriteLeaderStore::take_write_leader`])
/// decides the same way at the DB, so a race between two processes leaves
/// one of them with no row changed.
pub fn takeover(row: &WriteLeader, now_ms: i64, local_proof: bool) -> Takeover {
    if local_proof || row.lease_until_ms <= now_ms {
        Takeover::Take
    } else {
        Takeover::Wait {
            until_ms: row.lease_until_ms,
        }
    }
}

/// Compatible migrations retain the running leader; initial schema creation
/// has no row yet. Only a breaking change of an existing row takes it over.
pub fn migration_takes_leader(breaking: bool, row_exists: bool) -> bool {
    breaking && row_exists
}

/// Inspect the lease periodically even while requests queue up. Only the DB
/// lease decides whether that inspection needs a renewal transaction.
pub fn renewal_due(since_check: Duration) -> bool {
    since_check >= RENEW_INTERVAL
}

/// A planned stop releases only after every accepted write has finished.
/// On a drain timeout the process exits without releasing; takeover fences
/// any write still in flight. A fenced process never releases another leader.
pub fn release_after_stop(drained: bool, refused: bool) -> bool {
    drained && !refused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renewal_and_release_follow_the_line() {
        assert!(!renewal_due(RENEW_INTERVAL - Duration::from_millis(1)));
        assert!(renewal_due(RENEW_INTERVAL));
        assert!(renewal_due(RENEW_INTERVAL + Duration::from_secs(1)));
        assert!(release_after_stop(true, false));
        assert!(!release_after_stop(false, false));
        assert!(!release_after_stop(true, true));
        assert!(!release_after_stop(false, true));
        assert!(SERVICE_STOP_GRACE > RELEASE_DRAIN_LIMIT);
        assert_eq!(stop_grace(Duration::from_secs(10)), SERVICE_STOP_GRACE);
        assert_eq!(stop_grace(Duration::from_secs(40)), Duration::from_secs(40));
        assert!(!lease_renewal_due(30_000, 9_999));
        assert!(lease_renewal_due(30_000, 10_000));
        assert!(lease_renewal_due(30_000, 40_000));
        assert!(!lease_renewal_due(40_000, 10_000));
        assert!(!migration_takes_leader(false, true));
        assert!(!migration_takes_leader(true, false));
        assert!(migration_takes_leader(true, true));
    }

    fn row(generation: i64, lease_until_ms: i64) -> WriteLeader {
        WriteLeader {
            holder: (generation > 0).then(|| "a".to_owned()),
            generation,
            lease_until_ms,
            acquired_at_ms: (generation > 0).then_some(1_000),
        }
    }

    #[test]
    fn a_lease_that_ran_out_is_taken() {
        assert_eq!(takeover(&row(3, 9_999), 10_000, false), Takeover::Take);
        assert_eq!(takeover(&row(3, 10_000), 10_000, false), Takeover::Take);
    }

    #[test]
    fn a_released_lease_is_taken() {
        assert_eq!(takeover(&row(3, 0), 10_000, false), Takeover::Take);
    }

    #[test]
    fn a_lease_still_running_is_waited_for() {
        assert_eq!(
            takeover(&row(3, 10_001), 10_000, false),
            Takeover::Wait { until_ms: 10_001 }
        );
    }

    #[test]
    fn the_local_proof_takes_a_lease_still_running() {
        assert_eq!(takeover(&row(3, 40_000), 10_000, true), Takeover::Take);
    }

    #[test]
    fn a_row_no_process_has_led_is_taken() {
        let fresh = row(0, 0);
        assert_eq!(fresh.holder, None);
        assert_eq!(takeover(&fresh, 10_000, false), Takeover::Take);
        assert_eq!(takeover(&fresh, 10_000, true), Takeover::Take);
        // Fixture setup may have written at generation zero, extending
        // its lease without a holder. Only local lock proof bypasses it.
        let written = row(0, 40_000);
        assert_eq!(
            takeover(&written, 10_000, false),
            Takeover::Wait { until_ms: 40_000 }
        );
        assert_eq!(takeover(&written, 10_000, true), Takeover::Take);
    }
}
