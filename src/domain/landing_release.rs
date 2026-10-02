//! The wait of a dead landing's release on the processes left in its
//! worktree (task 1129). The supervisor gives back the integration slot of
//! an `integrating` run nobody lands (task 1118) only once nothing works in
//! its worktree; a process of the dead landing that hangs would hold the
//! slot for good. The first pass that finds one records
//! `landing_release_waiting` (`pids`, `seen_at`), so that the grace counts
//! from it across supervisors; past [`GRACE_SECS`] the runtime stops those
//! processes by pid and releases the run. When it cannot (a process
//! outlives its `SIGKILL`, or the processes cannot be listed for
//! [`UNLISTED_SECS`]), `landing_release_stuck` is the inbox's attention,
//! once for the run's landing.

use super::{
    RunEvent,
    event_kind::{INTEGRATION_STARTED, LANDING_RELEASE_STUCK, LANDING_RELEASE_WAITING},
};

/// How long the release waits for the processes in a dead landing's
/// worktree to end by themselves: the time limit of one verification
/// command of `integrate` (`VERIFICATION_TIMEOUT`, 30 minutes), past which
/// a live landing would have stopped it too.
pub const GRACE_SECS: i64 = 30 * 60;

/// How long the release goes on without a list of the host's processes
/// before it is the inbox's: twice [`GRACE_SECS`].
pub const UNLISTED_SECS: i64 = 2 * GRACE_SECS;

/// `landing_release_stuck`'s `cause` when a process stopped by pid was
/// still alive after its `SIGKILL`.
pub const SURVIVED_STOP: &str = "survived_stop";
/// `landing_release_stuck`'s `cause` when the host's processes could not
/// be listed for [`UNLISTED_SECS`].
pub const UNLISTED: &str = "unlisted";

/// The events of the run's latest landing: those after its latest
/// `integration_started`.
fn this_landing(events: &[RunEvent]) -> &[RunEvent] {
    let start = events
        .iter()
        .rposition(|e| e.kind == INTEGRATION_STARTED)
        .map_or(0, |i| i + 1);
    &events[start..]
}

/// When the release of the run's latest landing first found that it had
/// to wait (`landing_release_waiting`'s `seen_at`), if it did.
pub fn waiting_since(events: &[RunEvent]) -> Option<i64> {
    this_landing(events)
        .iter()
        .find(|e| e.kind == LANDING_RELEASE_WAITING)
        .and_then(|e| e.payload["seen_at"].as_i64())
}

/// The `landing_release_stuck` recorded for the run's latest landing: the
/// attention stands while the run stays `integrating`.
pub fn stuck(events: &[RunEvent]) -> Option<&RunEvent> {
    this_landing(events)
        .iter()
        .rev()
        .find(|e| e.kind == LANDING_RELEASE_STUCK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;
    use serde_json::{Value, json};

    fn event(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    /// Only the latest landing's wait and stuck count: an earlier landing
    /// of the run that waited does not shorten the grace of the next.
    #[test]
    fn the_wait_and_the_stuck_are_those_of_the_latest_landing() {
        let earlier = [
            event(INTEGRATION_STARTED, json!({})),
            event(LANDING_RELEASE_WAITING, json!({"seen_at": 10})),
            event(LANDING_RELEASE_STUCK, json!({"cause": SURVIVED_STOP})),
        ];
        assert_eq!(waiting_since(&earlier), Some(10));
        assert!(stuck(&earlier).is_some());
        let mut events = earlier.to_vec();
        events.push(event(INTEGRATION_STARTED, json!({})));
        assert_eq!(waiting_since(&events), None);
        assert!(stuck(&events).is_none());
        events.push(event(LANDING_RELEASE_WAITING, json!({"seen_at": 50})));
        events.push(event(LANDING_RELEASE_WAITING, json!({"seen_at": 90})));
        assert_eq!(waiting_since(&events), Some(50));
        assert_eq!(waiting_since(&[]), None);
    }
}
