//! The wait of a dead landing's release on the processes left in its
//! worktree (task 1129). The supervisor gives back the integration slot of
//! an `integrating` run nobody lands (task 1118) only once nothing works in
//! its worktree; a process of the dead landing that hangs would hold the
//! slot for good. The first pass that finds one records
//! `landing_release_waiting` (`pids`, `seen_at`), so that the grace counts
//! from it across supervisors; past the grace ([`grace_secs`], the limit of
//! one verification command) the runtime stops those processes by pid and
//! releases the run. When it cannot (a process outlives its `SIGKILL`, or
//! the processes cannot be listed for twice the grace, [`unlisted_secs`]), `landing_release_stuck` is the inbox's attention,
//! once for the run's landing.

use std::time::Duration;

use serde_json::{Value, json};

use super::{
    EventKind, RecoveredLanding, RunEvent, RunHistory, RunStatus, heartbeat_stale,
    recovery::ProcessInfo,
};

/// How long the release waits for the processes in a dead landing's
/// worktree to end by themselves when the limit of one verification
/// command is the default
/// ([`DEFAULT_COMMAND_TIMEOUT_SECS`](super::verification_limit::DEFAULT_COMMAND_TIMEOUT_SECS),
/// 60 minutes): [`grace_secs`] of it.
pub const GRACE_SECS: i64 = super::verification_limit::DEFAULT_COMMAND_TIMEOUT_SECS as i64;

/// How long the release goes on without a list of the host's processes
/// before it is the inbox's, with the default grace: twice [`GRACE_SECS`].
pub const UNLISTED_SECS: i64 = unlisted_secs(GRACE_SECS);

/// The grace of the release for `limit`, the time limit of one
/// verification command (the one `dagq.toml` declares, else the
/// runtime's): past it a live landing would have stopped the command too.
/// A fraction of a second counts as a whole one.
pub fn grace_secs(limit: Duration) -> i64 {
    let secs = limit
        .as_secs()
        .saturating_add(u64::from(limit.subsec_nanos() > 0));
    i64::try_from(secs).unwrap_or(i64::MAX)
}

/// How long the release goes on without a list of the host's processes
/// before it is the inbox's: twice the grace `grace_secs`.
pub const fn unlisted_secs(grace_secs: i64) -> i64 {
    grace_secs.saturating_mul(2)
}

/// `landing_release_stuck`'s `cause` when a process stopped by pid was
/// still alive after its `SIGKILL`.
pub const SURVIVED_STOP: &str = "survived_stop";
/// `landing_release_stuck`'s `cause` when the host's processes could not
/// be listed for twice the grace ([`unlisted_secs`]).
pub const UNLISTED: &str = "unlisted";

/// The events of the run's latest landing: those after its latest
/// `integration_started`.
fn this_landing(events: &[RunEvent]) -> &[RunEvent] {
    let start = events
        .iter()
        .rposition(|e| e.kind == EventKind::IntegrationStarted)
        .map_or(0, |i| i + 1);
    &events[start..]
}

/// When the release of the run's latest landing first found that it had
/// to wait (`landing_release_waiting`'s `seen_at`), if it did.
pub fn waiting_since(events: &[RunEvent]) -> Option<i64> {
    this_landing(events)
        .iter()
        .find(|e| e.kind == EventKind::LandingReleaseWaiting)
        .and_then(|e| e.payload["seen_at"].as_i64())
}

/// The `landing_release_stuck` recorded for the run's latest landing: the
/// attention stands while the run stays `integrating`.
pub fn stuck(events: &[RunEvent]) -> Option<&RunEvent> {
    this_landing(events)
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::LandingReleaseStuck)
}

/// The lease of an `integrating` run as the release reads it: whether
/// its supervisor's pid lives and how old its heartbeat is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseLook {
    pub pid_alive: bool,
    pub heartbeat_age_secs: i64,
}

/// Whether nobody lands an `integrating` run (task 1118): it has no lease
/// row, or its lease is stale with its supervisor's pid dead. The landing
/// of a live supervisor is left alone, with whatever works in its
/// worktree.
pub fn nobody_lands(lease: Option<LeaseLook>) -> bool {
    lease.is_none_or(|lease| {
        heartbeat_stale(lease.pid_alive, lease.heartbeat_age_secs) && !lease.pid_alive
    })
}

/// What the release does about the processes `pids` it found in the
/// worktree of a dead landing, at `now`.
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessesStep {
    /// The first pass that found them records `landing_release_waiting`
    /// with this payload, from whose `seen_at` the grace counts across
    /// supervisors, and waits.
    Note(Value),
    /// Within the grace of the first pass that found them: wait.
    Wait { waited: i64 },
    /// Past the grace, but a process already outlived its `SIGKILL`
    /// (`landing_release_stuck`, `cause: survived_stop`): the inbox's,
    /// and nothing is stopped or recorded again.
    Stuck,
    /// Past the grace: stop them by pid.
    Stop { waited: i64 },
}

/// [`ProcessesStep`] for `pids`, found at `now` in the worktree of the
/// dead landing whose run has `events`, with the grace `grace_secs`
/// ([`grace_secs`]).
pub fn processes_step(
    events: &[RunEvent],
    pids: &[u32],
    now: i64,
    grace_secs: i64,
) -> ProcessesStep {
    let Some(since) = waiting_since(events) else {
        return ProcessesStep::Note(
            json!({"pids": pids, "seen_at": now, "grace_secs": grace_secs}),
        );
    };
    let waited = now - since;
    if waited < grace_secs {
        return ProcessesStep::Wait { waited };
    }
    if stuck(events).is_some_and(|e| e.payload["cause"] == SURVIVED_STOP) {
        return ProcessesStep::Stuck;
    }
    ProcessesStep::Stop { waited }
}

/// `landing_release_stuck` for the processes `left` alive after their
/// `SIGKILL` (`processes`, as listed), `waited` seconds after the first
/// pass that found them.
pub fn survived_stop(left: &[u32], processes: &[&ProcessInfo], waited: i64) -> Value {
    json!({
        "cause": SURVIVED_STOP,
        "pids": left,
        "processes": processes,
        "waited_secs": waited,
        "error": format!("processes {left:?} in the worktree of the dead landing outlived SIGKILL; stop them by pid"),
    })
}

/// `auto_repaired` (`repair: landing_processes_stopped`) for the processes
/// `working` stopped by pid `waited` seconds after the first pass that
/// found them, those in `killed` with `SIGKILL`, past the grace
/// `grace_secs`.
pub fn processes_stopped(
    working: &[ProcessInfo],
    killed: &[u32],
    waited: i64,
    grace_secs: i64,
) -> Value {
    let pids: Vec<u32> = working.iter().map(|p| p.pid).collect();
    json!({
        "layer": "runtime",
        "repair": "landing_processes_stopped",
        "conditions": {
            "pids": pids,
            "waited_secs": waited,
            "grace_secs": grace_secs,
        },
        "detail": {
            "stopped": working.iter().map(|p| json!({
                "pid": p.pid,
                "ppid": p.ppid,
                "command": p.command,
                "cwd": p.cwd,
                "killed": killed.contains(&p.pid),
            })).collect::<Vec<_>>(),
        },
    })
}

/// What the release records at `now` when the processes in the worktree
/// of the dead landing whose run has `events` could not be listed
/// (`error`): the first time `landing_release_waiting` without `pids`,
/// and past twice the grace `grace_secs` ([`unlisted_secs`]) from it
/// `landing_release_stuck` (`cause: unlisted`), once; `None` otherwise.
pub fn unlisted(
    events: &[RunEvent],
    now: i64,
    error: &str,
    grace_secs: i64,
) -> Option<(EventKind, Value)> {
    match waiting_since(events) {
        None => Some((
            EventKind::LandingReleaseWaiting,
            json!({"pids": null, "seen_at": now, "error": error}),
        )),
        Some(since) if now - since >= unlisted_secs(grace_secs) && stuck(events).is_none() => {
            Some((
                EventKind::LandingReleaseStuck,
                json!({
                    "cause": UNLISTED,
                    "pids": [],
                    "waited_secs": now - since,
                    "error": format!("the processes in the worktree of the dead landing could not be listed: {error}"),
                }),
            ))
        }
        Some(_) => None,
    }
}

/// What the release records once the run it gave back is
/// `awaiting_integration` again (`status`), from the run's `history`:
/// whether it lands again (`landing_queued`, `via: recover`, follows)
/// rather than being reviewed, and `auto_repaired` (`repair:
/// landing_released`) with the dead supervisor's pid (`None` without a
/// lease row) and how many processes were stopped. `None` when the
/// latest `run_recovered` is not from `integrating`: another supervisor
/// released it first, and its records stand.
pub fn released(
    history: &RunHistory<'_>,
    supervisor_pid: Option<u32>,
    stopped: usize,
    status: &str,
) -> Option<(bool, Value)> {
    if history
        .last(EventKind::RunRecovered)
        .is_none_or(|e| e.payload["previous_status"] != RunStatus::Integrating.as_str())
    {
        return None;
    }
    let land = matches!(history.recovered_landing(), Some(RecoveredLanding::Land(_)));
    let then = if land { "land" } else { "review" };
    Some((
        land,
        json!({
            "layer": "runtime",
            "repair": "landing_released",
            "conditions": {
                "lease": if supervisor_pid.is_some() { "stale" } else { "none" },
                "supervisor_pid": supervisor_pid,
                "worktree_processes": 0,
                "stopped_processes": stopped,
                "review_passed": history.review_lets_land(),
                "approved": history.approved(),
            },
            "detail": {"then": then, "status": status},
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;
    use crate::domain::event_kind::INTEGRATION_STARTED;
    use crate::domain::event_kind::LANDING_RELEASE_STUCK;
    use crate::domain::event_kind::LANDING_RELEASE_WAITING;
    use crate::domain::event_kind::RUN_RECOVERED;

    fn event(kind: &str, payload: Value) -> RunEvent {
        event_at(1, kind, payload)
    }

    fn event_at(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
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

    /// An `integrating` run is the release's only when nobody lands it: no
    /// lease, or a lease whose supervisor's pid is dead. A live supervisor's
    /// landing, its heartbeat fresh or not, is left alone with what works
    /// in its worktree.
    #[test]
    fn only_a_landing_whose_supervisor_is_gone_is_released() {
        let look = |pid_alive, heartbeat_age_secs| {
            Some(LeaseLook {
                pid_alive,
                heartbeat_age_secs,
            })
        };
        assert!(nobody_lands(None));
        assert!(!nobody_lands(look(true, 0)));
        assert!(!nobody_lands(look(true, 10 * GRACE_SECS)));
        assert!(nobody_lands(look(false, 0)));
        assert!(nobody_lands(look(false, 10 * GRACE_SECS)));
    }

    /// The first pass that finds processes in the worktree records the
    /// wait; the grace counts from it (another supervisor's included), and
    /// past it they are stopped, unless one already outlived its SIGKILL:
    /// then nothing is stopped or recorded again, however many passes go
    /// by.
    #[test]
    fn processes_are_waited_for_then_stopped_unless_one_outlived_its_stop() {
        let started = event(INTEGRATION_STARTED, json!({}));
        assert_eq!(
            processes_step(std::slice::from_ref(&started), &[7, 8], 1000, GRACE_SECS),
            ProcessesStep::Note(json!({"pids": [7, 8], "seen_at": 1000, "grace_secs": GRACE_SECS}))
        );
        let waiting = event(
            LANDING_RELEASE_WAITING,
            json!({"pids": [7], "seen_at": 1000}),
        );
        let events = [started.clone(), waiting.clone()];
        assert_eq!(
            processes_step(&events, &[7], 1000 + GRACE_SECS - 1, GRACE_SECS),
            ProcessesStep::Wait {
                waited: GRACE_SECS - 1
            }
        );
        assert_eq!(
            processes_step(&events, &[7], 1000 + GRACE_SECS + 5, GRACE_SECS),
            ProcessesStep::Stop {
                waited: GRACE_SECS + 5
            }
        );
        let survived = event(LANDING_RELEASE_STUCK, json!({"cause": SURVIVED_STOP}));
        let events = [started.clone(), waiting.clone(), survived];
        for later in [GRACE_SECS + 5, 3 * GRACE_SECS] {
            assert_eq!(
                processes_step(&events, &[7], 1000 + later, GRACE_SECS),
                ProcessesStep::Stuck
            );
        }
        // An unlisted stuck does not keep the processes found later from
        // being stopped.
        let unlisted = event(LANDING_RELEASE_STUCK, json!({"cause": UNLISTED}));
        let events = [started, waiting, unlisted];
        assert_eq!(
            processes_step(&events, &[7], 1000 + GRACE_SECS, GRACE_SECS),
            ProcessesStep::Stop { waited: GRACE_SECS }
        );
    }

    fn process(pid: u32, cwd: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid: 1,
            elapsed_secs: 5,
            command: "sleep 120".to_owned(),
            cwd: Some(cwd.to_owned()),
            cpu_ms: None,
        }
    }

    /// What the release records of the processes it stopped, and of those
    /// that outlived their SIGKILL, which are the inbox's.
    #[test]
    fn the_stopped_and_the_surviving_processes_are_recorded_by_pid() {
        let working = [process(7, "/w"), process(8, "/w/sub")];
        assert_eq!(
            processes_stopped(&working, &[8], GRACE_SECS + 5, GRACE_SECS),
            json!({
                "layer": "runtime",
                "repair": "landing_processes_stopped",
                "conditions": {"pids": [7, 8], "waited_secs": GRACE_SECS + 5, "grace_secs": GRACE_SECS},
                "detail": {"stopped": [
                    {"pid": 7, "ppid": 1, "command": "sleep 120", "cwd": "/w", "killed": false},
                    {"pid": 8, "ppid": 1, "command": "sleep 120", "cwd": "/w/sub", "killed": true},
                ]},
            })
        );
        let stuck = survived_stop(&[8], &[&working[1]], GRACE_SECS + 5);
        assert_eq!(stuck["cause"], SURVIVED_STOP);
        assert_eq!(stuck["pids"], json!([8]));
        assert_eq!(stuck["processes"][0]["pid"], 8);
        assert_eq!(stuck["processes"][0]["elapsed_secs"], 5);
        assert_eq!(stuck["waited_secs"], GRACE_SECS + 5);
        assert_eq!(
            stuck["error"],
            "processes [8] in the worktree of the dead landing outlived SIGKILL; stop them by pid"
        );
    }

    /// Processes that cannot be listed are waited for from the first pass
    /// that could not list them; past twice the grace they are the inbox's,
    /// once, however many passes go by.
    #[test]
    fn unlisted_processes_are_the_inboxs_once_past_twice_the_grace() {
        let started = event(INTEGRATION_STARTED, json!({}));
        assert_eq!(
            unlisted(
                std::slice::from_ref(&started),
                1000,
                "ps failed",
                GRACE_SECS
            ),
            Some((
                EventKind::LandingReleaseWaiting,
                json!({"pids": null, "seen_at": 1000, "error": "ps failed"})
            ))
        );
        let waiting = event(
            LANDING_RELEASE_WAITING,
            json!({"pids": null, "seen_at": 1000, "error": "ps failed"}),
        );
        let events = [started.clone(), waiting.clone()];
        assert_eq!(
            unlisted(&events, 1000 + UNLISTED_SECS - 1, "ps failed", GRACE_SECS),
            None
        );
        let (kind, payload) =
            unlisted(&events, 1000 + UNLISTED_SECS + 5, "ps failed", GRACE_SECS).unwrap();
        assert_eq!(kind, EventKind::LandingReleaseStuck);
        assert_eq!(
            payload,
            json!({
                "cause": UNLISTED,
                "pids": [],
                "waited_secs": UNLISTED_SECS + 5,
                "error": "the processes in the worktree of the dead landing could not be listed: ps failed",
            })
        );
        let stuck = event(LANDING_RELEASE_STUCK, payload);
        let events = [started, waiting, stuck];
        assert_eq!(
            unlisted(&events, 1000 + 3 * UNLISTED_SECS, "ps failed", GRACE_SECS),
            None
        );
    }

    /// The grace is the limit of one verification command, a fraction of
    /// a second counting whole, and the wait without a list twice it: a
    /// declared limit moves the wait, the stop and the stuck with it.
    #[test]
    fn the_grace_and_the_unlisted_wait_follow_the_command_limit() {
        assert_eq!(grace_secs(Duration::from_secs(3600)), GRACE_SECS);
        assert_eq!(UNLISTED_SECS, 2 * GRACE_SECS);
        assert_eq!(grace_secs(Duration::from_millis(20)), 1);
        assert_eq!(grace_secs(Duration::from_millis(1500)), 2);
        assert_eq!(grace_secs(Duration::MAX), i64::MAX);
        assert_eq!(unlisted_secs(i64::MAX), i64::MAX);
        let grace = grace_secs(Duration::from_secs(600));
        assert_eq!(grace, 600);
        let started = event(INTEGRATION_STARTED, json!({}));
        assert_eq!(
            processes_step(std::slice::from_ref(&started), &[7], 1000, grace),
            ProcessesStep::Note(json!({"pids": [7], "seen_at": 1000, "grace_secs": 600}))
        );
        let waiting = event(
            LANDING_RELEASE_WAITING,
            json!({"pids": [7], "seen_at": 1000}),
        );
        let events = [started.clone(), waiting];
        assert_eq!(
            processes_step(&events, &[7], 1599, grace),
            ProcessesStep::Wait { waited: 599 }
        );
        assert_eq!(
            processes_step(&events, &[7], 1600, grace),
            ProcessesStep::Stop { waited: 600 }
        );
        assert_eq!(
            processes_stopped(&[process(7, "/w")], &[], 600, grace)["conditions"]["grace_secs"],
            600
        );
        let unlisted_wait = event(
            LANDING_RELEASE_WAITING,
            json!({"pids": null, "seen_at": 1000, "error": "ps failed"}),
        );
        let events = [started, unlisted_wait];
        assert_eq!(unlisted(&events, 2199, "ps failed", grace), None);
        let (kind, payload) = unlisted(&events, 2200, "ps failed", grace).unwrap();
        assert_eq!(kind, EventKind::LandingReleaseStuck);
        assert_eq!(payload["waited_secs"], 1200);
    }

    /// A released run lands again when it was approved or its review passed,
    /// and is reviewed otherwise; one another supervisor released first
    /// records nothing more.
    #[test]
    fn a_released_run_lands_again_when_landable_else_is_reviewed() {
        let recovered = |id| {
            event_at(
                id,
                RUN_RECOVERED,
                json!({"previous_status": "integrating", "status": "awaiting_integration"}),
            )
        };
        let passed = [
            event_at(1, "review_finished", json!({"verdict": "pass"})),
            event_at(2, INTEGRATION_STARTED, json!({})),
            recovered(3),
        ];
        let (land, payload) = released(
            &RunHistory::from_events(&passed),
            Some(42),
            1,
            "awaiting_integration",
        )
        .unwrap();
        assert!(land);
        assert_eq!(
            payload,
            json!({
                "layer": "runtime",
                "repair": "landing_released",
                "conditions": {
                    "lease": "stale",
                    "supervisor_pid": 42,
                    "worktree_processes": 0,
                    "stopped_processes": 1,
                    "review_passed": true,
                    "approved": false,
                },
                "detail": {"then": "land", "status": "awaiting_integration"},
            })
        );
        let concern = [
            event_at(1, "review_finished", json!({"verdict": "concern"})),
            event_at(2, INTEGRATION_STARTED, json!({})),
            recovered(3),
        ];
        let (land, payload) = released(
            &RunHistory::from_events(&concern),
            None,
            0,
            "awaiting_integration",
        )
        .unwrap();
        assert!(!land);
        assert_eq!(payload["detail"]["then"], "review");
        assert_eq!(payload["conditions"]["lease"], "none");
        assert_eq!(payload["conditions"]["supervisor_pid"], Value::Null);
        assert_eq!(payload["conditions"]["review_passed"], false);
        // Another supervisor released it first, and it was leased again.
        let mut again = passed.to_vec();
        again.push(event_at(
            4,
            RUN_RECOVERED,
            json!({"previous_status": "awaiting_integration"}),
        ));
        assert_eq!(
            released(
                &RunHistory::from_events(&again),
                Some(42),
                0,
                "awaiting_integration"
            ),
            None
        );
        assert_eq!(released(&RunHistory::from_events(&[]), None, 0, "x"), None);
    }
}
