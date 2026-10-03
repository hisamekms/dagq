//! A planner session (ADR-0041 decisions 1, 6, 12, 13): an on-demand cmux
//! workspace where a planner writes goals and tasks and submits them as a
//! proposal. A person opens one with `dagq plan`, the runtime opens one for
//! a proposal it sends back or a follow_up draft. Each is recorded apart
//! (`planners`), with the pids and heartbeat of its session wrapper, so its
//! liveness and idleness are judged the way a worker's are.

use serde::Serialize;

use super::{
    FindingId, PlannerId, PlannerOrigin, PlannerRoute, PlannerState, ProposalId, TaskId,
    heartbeat_stale,
};

/// One planner as the queue records it. Times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannerSession {
    pub id: PlannerId,
    /// Who opened it: `person` (`dagq plan`) or `runtime`.
    pub origin: PlannerOrigin,
    /// The proposal the runtime opened it for; a person's planner has none
    /// until it submits (the proposal then names its workspace).
    pub proposal_id: Option<ProposalId>,
    /// The draft the runtime opened it for (ADR-0041 decision 16): a
    /// follow_up or another draft the runtime or a job registered.
    pub draft_task_id: Option<TaskId>,
    /// The finding the runtime opened it for (ADR-0044 decision 19): one
    /// marked for a proposal.
    pub finding_id: Option<FindingId>,
    /// The cmux workspace's UUID, once cmux created it (ADR-0026).
    pub workspace_id: Option<String>,
    pub wrapper_pid: Option<u32>,
    pub agent_pid: Option<u32>,
    pub heartbeat_at: Option<i64>,
    /// The agent's exit code, recorded by the wrapper.
    pub exit_code: Option<i32>,
    pub exited_at: Option<i64>,
    /// When the queue gave the planner up: its workspace failed to open or
    /// is gone.
    pub closed_at: Option<i64>,
    pub error: Option<String>,
    pub created_at: i64,
    /// How its agent runs (ADR-t1394-2): interactive in a terminal, or one
    /// call per turn.
    pub route: PlannerRoute,
}

/// How long a planner may take from its record to its wrapper's
/// registration before it counts as `lost`, as a run's startup is bounded.
pub const PLANNER_STARTUP_SECS: i64 = 120;

/// How long a person's planner stays open after its agent exited before the
/// runtime closes its workspace and row (ADR-t1300-1): time for a person to
/// read the last screen.
pub const PERSON_PLANNER_CLOSE_GRACE_SECS: i64 = 60;

/// What was looked at to judge a planner: whether cmux still lists its
/// workspace, whether its wrapper's process is alive, the idle marker its
/// agent's `Stop` hook wrote (only one no older than the session's last
/// input), whether its screen shows the agent at work (`None` when the
/// screen could not be read) and, without such a marker, since when its
/// screen shows it idle (ADR-t803-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerProbe {
    pub now: i64,
    pub workspace_listed: bool,
    pub wrapper_alive: bool,
    pub idle: Option<IdleProbe>,
    pub working: Option<bool>,
    /// The idle the screen was inferred from (ADR-t803-1): since the first
    /// capture of its span, with the background work it showed.
    pub screen_idle: Option<IdleProbe>,
}

/// The idle marker of a planner's agent: when it was written (Unix
/// seconds) and whether the agent left background work running then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleProbe {
    pub since: i64,
    pub background_running: bool,
}

impl PlannerSession {
    /// The planner's state from what `probe` saw. A closed planner or one
    /// whose workspace cmux no longer lists is `closed`; an agent that
    /// exited in a workspace still open is `exited`; a wrapper whose process
    /// is gone or whose heartbeat is older than [`HEARTBEAT_TIMEOUT_SECS`]
    /// without a recorded exit is `lost`; before the wrapper registers it is
    /// `opening`, and `lost` once [`PLANNER_STARTUP_SECS`] passed; a live
    /// wrapper that has not recorded its agent's pid is `opening` too. A live agent is `working` while its screen shows a turn,
    /// and `idle` once its `Stop` hook wrote the marker with no background
    /// work left; without a marker (or with one older than its last
    /// input) it is `idle` once its screen was inferred idle with no
    /// background work shown, `working` until then.
    pub fn state(&self, probe: &PlannerProbe) -> PlannerState {
        if self.closed_at.is_some() || (self.workspace_id.is_some() && !probe.workspace_listed) {
            return PlannerState::Closed;
        }
        if self.workspace_id.is_none() || self.wrapper_pid.is_none() {
            // A workspace that never ran its wrapper (cmux could not start
            // it, or the opening process died before it recorded the
            // workspace) is not opening any more.
            return if probe.now - self.created_at > PLANNER_STARTUP_SECS {
                PlannerState::Lost
            } else {
                PlannerState::Opening
            };
        }
        if self.exited_at.is_some() {
            return PlannerState::Exited;
        }
        let age = self.heartbeat_at.map_or(i64::MAX, |at| probe.now - at);
        if heartbeat_stale(probe.wrapper_alive, age) {
            return PlannerState::Lost;
        }
        if self.agent_pid.is_none() {
            // The wrapper runs but has not recorded its agent yet: no idle
            // marker or screen is the agent's (task 1329), and input sent
            // now would reach no agent.
            return PlannerState::Opening;
        }
        if probe.working == Some(true) {
            return PlannerState::Working;
        }
        match probe.idle {
            Some(idle) if !idle.background_running => PlannerState::Idle,
            None if probe
                .screen_idle
                .is_some_and(|idle| !idle.background_running) =>
            {
                PlannerState::Idle
            }
            _ => PlannerState::Working,
        }
    }
}

impl PlannerSession {
    /// Whether the planner's row can be closed for good (a person's
    /// planner included): not closed yet, its workspace recorded and not in
    /// cmux's listing, and its wrapper done (its agent's exit recorded, its
    /// process gone or its heartbeat older than [`HEARTBEAT_TIMEOUT_SECS`],
    /// or never registered within [`PLANNER_STARTUP_SECS`] of the record).
    /// A workspace not listed alone is no evidence: a wrapper still alive
    /// keeps the row open.
    pub fn abandoned(&self, probe: &PlannerProbe) -> bool {
        if self.closed_at.is_some() || self.workspace_id.is_none() || probe.workspace_listed {
            return false;
        }
        if self.wrapper_pid.is_none() {
            return probe.now - self.created_at > PLANNER_STARTUP_SECS;
        }
        let age = self.heartbeat_at.map_or(i64::MAX, |at| probe.now - at);
        self.exited_at.is_some() || heartbeat_stale(probe.wrapper_alive, age)
    }
}

impl PlannerSession {
    /// Whether the runtime closes this person's planner now (ADR-t1300-1):
    /// opened by a person, not closed, its workspace and wrapper recorded,
    /// and its agent's exit recorded more than
    /// [`PERSON_PLANNER_CLOSE_GRACE_SECS`] before `now`. A planner whose
    /// wrapper is lost, or that is alive, is not.
    pub fn person_exit_closes(&self, now: i64) -> bool {
        self.origin == PlannerOrigin::Person
            && self.closed_at.is_none()
            && self.workspace_id.is_some()
            && self.wrapper_pid.is_some()
            && self
                .exited_at
                .is_some_and(|at| now - at > PERSON_PLANNER_CLOSE_GRACE_SECS)
    }
}

impl PlannerSession {
    /// Whether nothing runs the planner's wrapper binary (its `runner`)
    /// any more, so the snapshot can go: its agent's exit is recorded, its
    /// wrapper's process is gone or its heartbeat older than
    /// [`HEARTBEAT_TIMEOUT_SECS`], or it never registered within
    /// [`PLANNER_STARTUP_SECS`] of the record and its row is closed (its
    /// workspace never opened, or cmux no longer lists it: a slow shell
    /// in a workspace still open may yet run the runner). A closed row
    /// alone is no evidence: a wrapper still alive keeps its runner.
    pub fn runner_unused(&self, probe: &PlannerProbe) -> bool {
        if self.exited_at.is_some() {
            return true;
        }
        if self.wrapper_pid.is_none() {
            return self.closed_at.is_some() && probe.now - self.created_at > PLANNER_STARTUP_SECS;
        }
        let age = self.heartbeat_at.map_or(i64::MAX, |at| probe.now - at);
        heartbeat_stale(probe.wrapper_alive, age)
    }
}

impl PlannerState {
    /// Whether the planner's session can still take text: it is opening,
    /// working or idle in a workspace cmux lists.
    pub fn alive(self) -> bool {
        matches!(self, Self::Opening | Self::Working | Self::Idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::HEARTBEAT_TIMEOUT_SECS;

    fn session() -> PlannerSession {
        PlannerSession {
            id: PlannerId::new(1),
            origin: PlannerOrigin::Person,
            proposal_id: None,
            draft_task_id: None,
            finding_id: None,
            workspace_id: Some("W".into()),
            wrapper_pid: Some(10),
            agent_pid: Some(11),
            heartbeat_at: Some(100),
            exit_code: None,
            exited_at: None,
            closed_at: None,
            error: None,
            created_at: 90,
            route: PlannerRoute::Interactive,
        }
    }

    fn probe() -> PlannerProbe {
        PlannerProbe {
            now: 110,
            workspace_listed: true,
            wrapper_alive: true,
            idle: None,
            working: None,
            screen_idle: None,
        }
    }

    #[test]
    fn only_a_persons_planner_whose_agent_exited_past_the_grace_is_closed() {
        let exited = PlannerSession {
            exit_code: Some(0),
            exited_at: Some(100),
            ..session()
        };
        let past = 100 + PERSON_PLANNER_CLOSE_GRACE_SECS + 1;
        assert!(exited.person_exit_closes(past));
        // Within the grace, alive, closed, without a wrapper or opened by
        // the runtime: not by this rule.
        assert!(!exited.person_exit_closes(100 + PERSON_PLANNER_CLOSE_GRACE_SECS));
        assert!(!session().person_exit_closes(past));
        for other in [
            PlannerSession {
                closed_at: Some(105),
                ..exited.clone()
            },
            PlannerSession {
                wrapper_pid: None,
                ..exited.clone()
            },
            PlannerSession {
                workspace_id: None,
                ..exited.clone()
            },
            PlannerSession {
                origin: PlannerOrigin::Runtime,
                ..exited.clone()
            },
        ] {
            assert!(!other.person_exit_closes(past), "{other:?}");
        }
    }

    #[test]
    fn a_planner_is_judged_like_a_worker_session() {
        let live = session();
        assert_eq!(live.state(&probe()), PlannerState::Working);
        let idle = IdleProbe {
            since: 105,
            background_running: false,
        };
        let idle_probe = PlannerProbe {
            idle: Some(idle),
            ..probe()
        };
        assert_eq!(live.state(&idle_probe), PlannerState::Idle);
        // The screen of a new turn wins over an older marker.
        assert_eq!(
            live.state(&PlannerProbe {
                working: Some(true),
                ..idle_probe
            }),
            PlannerState::Working
        );
        assert_eq!(
            live.state(&PlannerProbe {
                idle: Some(IdleProbe {
                    background_running: true,
                    ..idle
                }),
                ..probe()
            }),
            PlannerState::Working
        );
        // A dead wrapper, or one silent past the heartbeat timeout, is lost.
        assert_eq!(
            live.state(&PlannerProbe {
                wrapper_alive: false,
                ..probe()
            }),
            PlannerState::Lost
        );
        assert_eq!(
            live.state(&PlannerProbe {
                now: 100 + HEARTBEAT_TIMEOUT_SECS + 1,
                ..probe()
            }),
            PlannerState::Lost
        );
        assert!(PlannerState::Idle.alive() && PlannerState::Working.alive());
        assert!(!PlannerState::Lost.alive());
    }

    #[test]
    fn a_planner_without_a_fresh_marker_is_idle_by_its_screen() {
        let live = session();
        let screen = IdleProbe {
            since: 104,
            background_running: false,
        };
        let inferred = PlannerProbe {
            screen_idle: Some(screen),
            ..probe()
        };
        assert_eq!(live.state(&inferred), PlannerState::Idle);
        // A screen that shows background work keeps it working.
        assert_eq!(
            live.state(&PlannerProbe {
                screen_idle: Some(IdleProbe {
                    background_running: true,
                    ..screen
                }),
                ..probe()
            }),
            PlannerState::Working
        );
        // A screen at work wins, and a fresh marker left with background
        // work decides over the screen.
        assert_eq!(
            live.state(&PlannerProbe {
                working: Some(true),
                ..inferred
            }),
            PlannerState::Working
        );
        assert_eq!(
            live.state(&PlannerProbe {
                idle: Some(IdleProbe {
                    since: 105,
                    background_running: true,
                }),
                ..inferred
            }),
            PlannerState::Working
        );
    }

    #[test]
    fn a_planner_is_abandoned_once_its_workspace_and_wrapper_are_gone() {
        let gone = PlannerProbe {
            workspace_listed: false,
            wrapper_alive: false,
            ..probe()
        };
        assert!(session().abandoned(&gone));
        // A listed workspace, or a live wrapper, keeps the row.
        assert!(!session().abandoned(&PlannerProbe {
            workspace_listed: true,
            ..gone
        }));
        let alive = PlannerProbe {
            wrapper_alive: true,
            ..gone
        };
        assert!(!session().abandoned(&alive));
        // A live pid silent past the heartbeat timeout, or an exit, ends it.
        assert!(session().abandoned(&PlannerProbe {
            now: 100 + HEARTBEAT_TIMEOUT_SECS + 1,
            ..alive
        }));
        let exited = PlannerSession {
            exited_at: Some(105),
            ..session()
        };
        assert!(exited.abandoned(&alive));
        // A wrapper never registered is given its startup time.
        let unregistered = PlannerSession {
            wrapper_pid: None,
            heartbeat_at: None,
            ..session()
        };
        assert!(!unregistered.abandoned(&gone));
        assert!(unregistered.abandoned(&PlannerProbe {
            now: 90 + PLANNER_STARTUP_SECS + 1,
            ..gone
        }));
        // Closed rows, and rows without a workspace, are not judged.
        let closed = PlannerSession {
            closed_at: Some(109),
            ..session()
        };
        assert!(!closed.abandoned(&gone));
        let opening = PlannerSession {
            workspace_id: None,
            ..unregistered
        };
        assert!(!opening.abandoned(&PlannerProbe {
            now: 90 + PLANNER_STARTUP_SECS + 1,
            ..gone
        }));
    }

    #[test]
    fn a_planner_runner_is_unused_once_its_wrapper_is_done() {
        // A live wrapper keeps its runner, closed or not, workspace or not.
        assert!(!session().runner_unused(&probe()));
        let closed = PlannerSession {
            closed_at: Some(109),
            ..session()
        };
        assert!(!closed.runner_unused(&PlannerProbe {
            workspace_listed: false,
            ..probe()
        }));
        // An exit, a dead wrapper or a silent one frees it.
        let exited = PlannerSession {
            exited_at: Some(108),
            ..session()
        };
        assert!(exited.runner_unused(&probe()));
        assert!(closed.runner_unused(&PlannerProbe {
            wrapper_alive: false,
            ..probe()
        }));
        assert!(session().runner_unused(&PlannerProbe {
            now: 100 + HEARTBEAT_TIMEOUT_SECS + 1,
            ..probe()
        }));
        // A wrapper never registered is given its startup time, and then
        // only a closed row frees it: an open workspace may start late.
        let unregistered = PlannerSession {
            wrapper_pid: None,
            heartbeat_at: None,
            ..session()
        };
        let late = PlannerProbe {
            now: 90 + PLANNER_STARTUP_SECS + 1,
            ..probe()
        };
        assert!(!unregistered.runner_unused(&late));
        let given_up = PlannerSession {
            closed_at: Some(109),
            ..unregistered.clone()
        };
        assert!(!given_up.runner_unused(&probe()));
        assert!(given_up.runner_unused(&late));
    }

    #[test]
    fn a_planner_opens_exits_and_closes() {
        let opening = PlannerSession {
            workspace_id: None,
            wrapper_pid: None,
            ..session()
        };
        assert_eq!(opening.state(&probe()), PlannerState::Opening);
        assert!(PlannerState::Opening.alive());
        let unregistered = PlannerSession {
            wrapper_pid: None,
            ..session()
        };
        assert_eq!(unregistered.state(&probe()), PlannerState::Opening);
        // A wrapper that has not recorded its agent is opening, whatever an
        // idle marker or the screen shows (task 1329).
        let agentless = PlannerSession {
            agent_pid: None,
            ..session()
        };
        let idle = IdleProbe {
            since: 105,
            background_running: false,
        };
        for seen in [
            probe(),
            PlannerProbe {
                idle: Some(idle),
                ..probe()
            },
            PlannerProbe {
                screen_idle: Some(idle),
                ..probe()
            },
        ] {
            assert_eq!(agentless.state(&seen), PlannerState::Opening);
        }
        assert_eq!(
            agentless.state(&PlannerProbe {
                wrapper_alive: false,
                ..probe()
            }),
            PlannerState::Lost
        );
        assert_eq!(
            opening.state(&PlannerProbe {
                now: 90 + PLANNER_STARTUP_SECS + 1,
                ..probe()
            }),
            PlannerState::Lost
        );
        let exited = PlannerSession {
            exited_at: Some(108),
            exit_code: Some(0),
            ..session()
        };
        assert_eq!(exited.state(&probe()), PlannerState::Exited);
        assert!(!PlannerState::Exited.alive());
        // A workspace cmux no longer lists, or a closed record, is closed.
        assert_eq!(
            session().state(&PlannerProbe {
                workspace_listed: false,
                ..probe()
            }),
            PlannerState::Closed
        );
        let closed = PlannerSession {
            closed_at: Some(109),
            ..session()
        };
        assert_eq!(closed.state(&probe()), PlannerState::Closed);
        assert!(!PlannerState::Closed.alive());
    }
}
