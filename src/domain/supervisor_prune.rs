//! Whether a supervisor's registration was left behind by a process that
//! is gone, which `up`, `down` and a live supervisor of the queue prune
//! (docs/design/supervisor-lifecycle/up-down.md): its pid runs nothing, or
//! it stopped heartbeating and its pid now belongs to another process.

use super::{HEARTBEAT_TIMEOUT_SECS, SupervisorRegistration, recovery::ProcessInfo};

/// How much later than its registration's `started_at` a process at the
/// registered pid may seem to have started and still be the one that
/// registered: `ps` prints the age to the second.
const PID_START_SLACK_SECS: i64 = 5;

/// Whether the registration's last heartbeat is older than
/// [`HEARTBEAT_TIMEOUT_SECS`] at `now`.
pub fn heartbeat_expired(registration: &SupervisorRegistration, now: i64) -> bool {
    now - registration.heartbeat_at > HEARTBEAT_TIMEOUT_SECS
}

/// Whether the pid of a registration that stopped heartbeating now belongs
/// to another process (task 330): the supervisor died without removing its
/// row and the system gave its pid to another process ([`pid_reused`]).
/// Such a row is as dead as one whose pid runs nothing, while an
/// alive-but-silent supervisor, which started before its registration, is
/// kept. When the processes could not be listed (`listed` none), nothing is
/// taken over.
pub fn pid_taken_over(
    registration: &SupervisorRegistration,
    now: i64,
    listed: Option<&[ProcessInfo]>,
) -> bool {
    heartbeat_expired(registration, now) && pid_reused(registration, now, listed)
}

/// Whether the pid of `registration` runs, at `now`, another process than
/// the one that registered: one of another user (absent from `listed`, this
/// user's processes, which every supervisor of this queue is), or one of
/// this user's that started after the registration, which the process that
/// registered cannot be (an exec keeps the start). Not judged, so `false`,
/// when the processes could not be listed (`listed` none).
pub fn pid_reused(
    registration: &SupervisorRegistration,
    now: i64,
    listed: Option<&[ProcessInfo]>,
) -> bool {
    let Some(listed) = listed else {
        return false;
    };
    match listed
        .iter()
        .find(|process| process.pid == registration.pid)
    {
        None => true,
        Some(process) => {
            let started = now - i64::try_from(process.elapsed_secs).unwrap_or(i64::MAX);
            started > registration.started_at + PID_START_SLACK_SECS
        }
    }
}

/// Whether every supervisor of `live` ended its drain as the processes
/// alone tell it at `now`, for `down --wait` once the queue's service is out
/// of reach (the last supervisor stops it at the end of its drain, so its
/// registrations cannot be read): each pid runs nothing (`alive` false) or
/// another process ([`pid_reused`]). The heartbeat is not looked at, as a
/// draining supervisor's goes on.
pub fn drain_ended_by_processes(
    live: &[SupervisorRegistration],
    now: i64,
    alive: impl Fn(u32) -> bool,
    listed: Option<&[ProcessInfo]>,
) -> bool {
    !live
        .iter()
        .any(|registration| alive(registration.pid) && !pid_reused(registration, now, listed))
}

/// Whether a live supervisor prunes another's registration on its own: the
/// heartbeat expired, and the pid runs nothing (`alive` false) or was taken
/// over ([`pid_taken_over`]). A registration with a fresh heartbeat is
/// kept whatever its pid tells, as a draining supervisor's and one in the
/// middle of a handoff are; `up` and `down`, which a person runs, prune a
/// dead pid without the heartbeat.
pub fn left_behind(
    registration: &SupervisorRegistration,
    now: i64,
    alive: bool,
    listed: Option<&[ProcessInfo]>,
) -> bool {
    heartbeat_expired(registration, now) && (!alive || pid_taken_over(registration, now, listed))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;
    const STARTED: i64 = NOW - 3_600;

    fn registration(heartbeat_at: i64) -> SupervisorRegistration {
        serde_json::from_value(serde_json::json!({
            "token": "other",
            "pid": 42,
            "parallel": 1,
            "started_at": STARTED,
            "heartbeat_at": heartbeat_at,
        }))
        .unwrap()
    }

    fn process(pid: u32, elapsed_secs: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid: 1,
            elapsed_secs,
            command: "dagq supervise".into(),
            cwd: None,
            cpu_ms: None,
        }
    }

    /// The pid is this user's and started before the registration: the
    /// supervisor that registered.
    fn own_process() -> Vec<ProcessInfo> {
        vec![process(42, u64::try_from(NOW - STARTED + 1).unwrap())]
    }

    /// The pid is this user's but started after the registration.
    fn later_process() -> Vec<ProcessInfo> {
        let after = STARTED + PID_START_SLACK_SECS + 1;
        vec![process(42, u64::try_from(NOW - after).unwrap())]
    }

    #[test]
    fn the_heartbeat_expires_only_past_the_timeout() {
        assert!(!heartbeat_expired(&registration(NOW), NOW));
        assert!(!heartbeat_expired(
            &registration(NOW - HEARTBEAT_TIMEOUT_SECS),
            NOW
        ));
        assert!(heartbeat_expired(
            &registration(NOW - HEARTBEAT_TIMEOUT_SECS - 1),
            NOW
        ));
    }

    /// The pid of a silent registration is taken over when it is not this
    /// user's or started after the registration; within the slack, or
    /// with no listing, it is not.
    #[test]
    fn a_silent_pid_is_taken_over_by_another_process() {
        let silent = registration(NOW - HEARTBEAT_TIMEOUT_SECS - 1);
        assert!(pid_taken_over(&silent, NOW, Some(&[])));
        assert!(pid_taken_over(&silent, NOW, Some(&later_process())));
        assert!(!pid_taken_over(&silent, NOW, Some(&own_process())));
        let within = vec![process(
            42,
            u64::try_from(NOW - STARTED - PID_START_SLACK_SECS).unwrap(),
        )];
        assert!(!pid_taken_over(&silent, NOW, Some(&within)));
        assert!(!pid_taken_over(&silent, NOW, None));
        // A heartbeating registration is never taken over.
        assert!(!pid_taken_over(&registration(NOW), NOW, Some(&[])));
    }

    /// Out of the service's reach, a drain is over once each supervisor's
    /// pid runs nothing or another process, its heartbeat fresh or not; the
    /// process that registered, still alive, is waited for, as is one that
    /// cannot be told apart for want of a listing.
    #[test]
    fn a_drain_ends_by_the_processes_when_the_pid_is_gone_or_reused() {
        let draining = [registration(NOW)];
        let alive = |_: u32| true;
        let dead = |_: u32| false;
        assert!(!drain_ended_by_processes(
            &draining,
            NOW,
            alive,
            Some(&own_process())
        ));
        assert!(drain_ended_by_processes(
            &draining,
            NOW,
            dead,
            Some(&own_process())
        ));
        // Reused: by a later process of this user's, or another user's.
        assert!(drain_ended_by_processes(
            &draining,
            NOW,
            alive,
            Some(&later_process())
        ));
        assert!(drain_ended_by_processes(&draining, NOW, alive, Some(&[])));
        assert!(!drain_ended_by_processes(&draining, NOW, alive, None));
        assert!(drain_ended_by_processes(&draining, NOW, dead, None));
        assert!(drain_ended_by_processes(&[], NOW, alive, None));
    }

    /// Each pair of the heartbeat (fresh or expired) and the pid (dead,
    /// taken over, alive and its own).
    #[test]
    fn a_registration_is_left_behind_only_when_silent_and_its_pid_is_gone() {
        let fresh = registration(NOW - HEARTBEAT_TIMEOUT_SECS);
        let silent = registration(NOW - HEARTBEAT_TIMEOUT_SECS - 1);
        let other = Some(&[][..]);
        let own = own_process();
        let later = later_process();
        // A fresh heartbeat keeps it, whatever the pid tells (a drain, a
        // handoff).
        assert!(!left_behind(&fresh, NOW, false, Some(&own)));
        assert!(!left_behind(&fresh, NOW, true, other));
        assert!(!left_behind(&fresh, NOW, true, Some(&later)));
        assert!(!left_behind(&fresh, NOW, true, Some(&own)));
        // Silent: a dead pid or one taken over goes.
        assert!(left_behind(&silent, NOW, false, Some(&own)));
        assert!(left_behind(&silent, NOW, false, None));
        assert!(left_behind(&silent, NOW, true, other));
        assert!(left_behind(&silent, NOW, true, Some(&later)));
        // Silent, its pid alive and its own (or unlisted): kept.
        assert!(!left_behind(&silent, NOW, true, Some(&own)));
        assert!(!left_behind(&silent, NOW, true, None));
    }
}
