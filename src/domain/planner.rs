//! A planner session (ADR-0041 decisions 1, 6, 12, 13): an on-demand cmux
//! workspace where a planner writes goals and tasks and submits them as a
//! proposal. The runtime opens one for a proposal it sends back, a draft, a
//! finding or a planning request; the row of a person's planner (`dagq
//! plan`, abolished by ADR-t1394-1) opened before is closed without cmux
//! (ADR-t1433-2 decision 5). Each is recorded apart
//! (`planners`), with the pids and heartbeat of its session wrapper, so its
//! liveness and idleness are judged the way a worker's are.

use serde::Serialize;

use super::{
    FindingId, PlannerId, PlannerOrigin, PlannerRoute, PlannerState, ProposalId, RequestId, TaskId,
    heartbeat_stale,
};

/// One planner as the queue records it. Times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannerSession {
    pub id: PlannerId,
    /// Who opened it: `person` (`dagq plan`, before ADR-t1394-1) or
    /// `runtime`.
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
    /// The planning request the runtime opened it for (ADR-t1394-1
    /// decision 4).
    pub request_id: Option<RequestId>,
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

/// What was looked at to judge a planner: whether its session is still
/// open, whether its wrapper's process is alive and the idle marker its
/// agent's `Stop` hook wrote (only one no older than the session's last
/// input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerProbe {
    pub now: i64,
    pub workspace_listed: bool,
    pub wrapper_alive: bool,
    pub idle: Option<IdleProbe>,
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
    /// whose session is not open (`workspace_listed`: for a background
    /// wrapper, its handle's pid showing the start it recorded) is
    /// `closed`; an agent that exited in a session still open is `exited`;
    /// a wrapper in a workspace whose process is gone or whose heartbeat is
    /// older than [`HEARTBEAT_TIMEOUT_SECS`] without a recorded exit is
    /// `lost`, while a background wrapper is `lost` only once its process
    /// is gone, a late heartbeat being no sign of its death (ADR-t1404-1
    /// decisions 2 and 10); before the wrapper registers it is
    /// `opening`, and `lost` once [`PLANNER_STARTUP_SECS`] passed; a live
    /// wrapper that has not recorded its agent's pid is `opening` too. A
    /// live agent is `idle` once its `Stop` hook wrote the marker with no
    /// background work left, and `working` without one (or with one older
    /// than its last input).
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
        // A background wrapper is told by its process alone: a late
        // heartbeat is no sign of its death. Its handle may still look open
        // while a turn its dead wrapper left runs, so a dead pid is `lost`.
        let lost = if self.background() {
            !probe.wrapper_alive
        } else {
            heartbeat_stale(probe.wrapper_alive, age)
        };
        if lost {
            return PlannerState::Lost;
        }
        if self.agent_pid.is_none() {
            // The wrapper runs but has not recorded its agent yet: no idle
            // marker is the agent's (task 1329), and input sent now would
            // reach no agent.
            return PlannerState::Opening;
        }
        match probe.idle {
            Some(idle) if !idle.background_running => PlannerState::Idle,
            _ => PlannerState::Working,
        }
    }
}

impl PlannerSession {
    /// Whether the planner's row can be closed for good (a person's
    /// planner included): not closed yet, its session recorded and not open
    /// (`workspace_listed`: for a background wrapper, its handle's pid
    /// showing the start it recorded), and its wrapper done (its agent's
    /// exit recorded, its process gone, or, in a workspace, its heartbeat
    /// older than [`HEARTBEAT_TIMEOUT_SECS`]; or never registered within
    /// [`PLANNER_STARTUP_SECS`] of the record). A session not open alone is
    /// no evidence: a wrapper still alive keeps the row open, and a
    /// background one's late heartbeat closes nothing (ADR-t1404-1
    /// decisions 2 and 10).
    pub fn abandoned(&self, probe: &PlannerProbe) -> bool {
        if self.closed_at.is_some() || self.workspace_id.is_none() || probe.workspace_listed {
            return false;
        }
        if self.wrapper_pid.is_none() {
            return probe.now - self.created_at > PLANNER_STARTUP_SECS;
        }
        if self.background() {
            return self.exited_at.is_some() || !probe.wrapper_alive;
        }
        let age = self.heartbeat_at.map_or(i64::MAX, |at| probe.now - at);
        self.exited_at.is_some() || heartbeat_stale(probe.wrapper_alive, age)
    }

    /// Whether the row's session is a background wrapper's handle
    /// (ADR-t1404-1), judged by its process alone.
    pub fn background(&self) -> bool {
        self.workspace_id
            .as_deref()
            .is_some_and(super::background_wrapper::is_background)
    }
}

impl PlannerSession {
    /// Whether the planner's session is over, for the span its hook
    /// recorded (ADR-t2022-1): its row is closed, or its wrapper ran in the
    /// background and the handle's pid is dead (`alive`) or shows another
    /// start than the handle recorded (`start`, ADR-t1404-1 decision 2). A
    /// heartbeat's age is no evidence, nor is a start that cannot be read;
    /// a wrapper in a workspace is over only with its row.
    pub fn session_over(
        &self,
        alive: impl Fn(u32) -> bool,
        start: impl Fn(u32) -> Option<String>,
    ) -> bool {
        if self.closed_at.is_some() {
            return true;
        }
        let Some(handle) = self
            .workspace_id
            .as_deref()
            .and_then(super::background_wrapper::BackgroundHandle::parse)
        else {
            return false;
        };
        !alive(handle.pid)
            || start(handle.pid).is_some_and(|start| !handle.is(handle.pid, Some(&start)))
    }
}

impl PlannerSession {
    /// Whether the runtime closes this row as a person's planner opened
    /// before `dagq plan` was abolished (ADR-t1433-2 decision 5): opened by
    /// a person and not closed, alive or not. Its workspace is neither
    /// looked at nor closed; a person closes it in their own terminal.
    pub fn person_retired(&self) -> bool {
        self.origin == PlannerOrigin::Person && self.closed_at.is_none()
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

/// Whether the answer of a `planner_question` waits rather than going to
/// the planner of the runtime's it is for now (ADR-0041 decision 13): it
/// goes to one `idle` that does not wait for Claude after its turn failed
/// at the provider's wall (`at_wall`; it gets it after its retry,
/// ADR-t1394-2 decision 5), and, when the planner asked it
/// (`asked_by_planner`), only once it stopped after asking (`idle_since`
/// no earlier than `asked_at`); a question someone else opened waits only
/// for it to be idle.
pub fn answer_waits(
    state: PlannerState,
    at_wall: bool,
    asked_by_planner: bool,
    idle_since: Option<i64>,
    asked_at: i64,
) -> bool {
    state != PlannerState::Idle
        || at_wall
        || (asked_by_planner && idle_since.is_none_or(|since| since < asked_at))
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
            request_id: None,
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
        }
    }

    #[test]
    fn every_open_row_of_a_persons_planner_is_retired_alive_or_not() {
        let exited = PlannerSession {
            exit_code: Some(0),
            exited_at: Some(100),
            ..session()
        };
        // Alive, exited, lost, or never given a workspace or a wrapper: a
        // person's row not closed is retired.
        for open in [
            session(),
            exited.clone(),
            PlannerSession {
                wrapper_pid: None,
                ..session()
            },
            PlannerSession {
                workspace_id: None,
                ..session()
            },
        ] {
            assert!(open.person_retired(), "{open:?}");
        }
        // Closed already, or opened by the runtime: not by this rule.
        for other in [
            PlannerSession {
                closed_at: Some(105),
                ..exited.clone()
            },
            PlannerSession {
                origin: PlannerOrigin::Runtime,
                ..exited
            },
        ] {
            assert!(!other.person_retired(), "{other:?}");
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
        // idle marker shows (task 1329).
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

    // Moved here by task 1711 from the tests/it cases it removed:
    // planner_headless_turns::a_headless_finding_planner_takes_the_answer_as_its_next_turn_and_closes_with_its_finding
    // and a_headless_request_planners_answer_reaches_a_new_one_and_undecided_ends_exhaust_the_request.
    #[test]
    fn an_answer_goes_to_an_idle_planner_not_at_the_wall_once_it_stopped_after_asking() {
        // Idle since after the question: it goes now.
        assert!(!answer_waits(PlannerState::Idle, false, true, Some(20), 10));
        assert!(!answer_waits(PlannerState::Idle, false, true, Some(10), 10));
        // At work, opening or gone: it waits.
        for state in [
            PlannerState::Opening,
            PlannerState::Working,
            PlannerState::Exited,
            PlannerState::Lost,
            PlannerState::Closed,
        ] {
            assert!(answer_waits(state, false, true, Some(20), 10), "{state:?}");
            assert!(answer_waits(state, false, false, Some(20), 10), "{state:?}");
        }
        // Waiting for Claude after its turn met the wall: after the retry.
        assert!(answer_waits(PlannerState::Idle, true, true, Some(20), 10));
        assert!(answer_waits(PlannerState::Idle, true, false, Some(20), 10));
        // The planner that asked and has not stopped since asking waits.
        assert!(answer_waits(PlannerState::Idle, false, true, Some(9), 10));
        assert!(answer_waits(PlannerState::Idle, false, true, None, 10));
        // A question someone else opened waits only for it to be idle.
        assert!(!answer_waits(PlannerState::Idle, false, false, Some(9), 10));
        assert!(!answer_waits(PlannerState::Idle, false, false, None, 10));
    }

    /// A background planner is alive while its handle's pid shows the start
    /// it recorded (`workspace_listed`), however late its heartbeat
    /// (ADR-t1404-1 decisions 2 and 10): a heartbeat past the timeout makes
    /// it neither `lost` nor abandoned, so its row and its span stay open;
    /// a handle that is gone (dead, or its pid another process's) closes it.
    #[test]
    fn a_background_planner_lives_by_its_handle_not_its_heartbeat() {
        let planner = PlannerSession {
            origin: PlannerOrigin::Runtime,
            route: PlannerRoute::Headless,
            workspace_id: Some("background:10:Mon_Oct__5_10:00:00_2026".into()),
            ..session()
        };
        assert!(planner.background());
        assert!(!session().background());
        let silent = PlannerProbe {
            now: 100 + HEARTBEAT_TIMEOUT_SECS + 1,
            ..probe()
        };
        assert_eq!(planner.state(&silent), PlannerState::Working);
        assert!(!planner.abandoned(&silent));
        let gone = PlannerProbe {
            workspace_listed: false,
            wrapper_alive: false,
            ..silent
        };
        assert_eq!(planner.state(&gone), PlannerState::Closed);
        assert!(planner.abandoned(&gone));
        // A wrapper killed while a turn it left runs (its handle still
        // open) is lost by its pid.
        assert_eq!(
            planner.state(&PlannerProbe {
                wrapper_alive: false,
                ..silent
            }),
            PlannerState::Lost
        );
        // A recent heartbeat does not keep a gone wrapper's row open either.
        let gone_now = PlannerProbe {
            workspace_listed: false,
            wrapper_alive: false,
            ..probe()
        };
        assert!(planner.abandoned(&gone_now));
        // A handle that only failed to read once, its pid alive, is no
        // evidence: the row stays open.
        let unread = PlannerProbe {
            workspace_listed: false,
            ..silent
        };
        assert!(!planner.abandoned(&unread));
        // A wrapper in a workspace is still told by its heartbeat.
        assert_eq!(session().state(&silent), PlannerState::Lost);
    }

    /// ADR-t2022-1: a planner's session is over for its hook span once its
    /// row is closed, or once its background wrapper's pid is dead or
    /// shows another start; a live wrapper, a start that cannot be read,
    /// a late heartbeat and a wrapper in a workspace keep it running.
    #[test]
    fn a_planner_session_is_over_by_its_row_or_its_background_wrapper() {
        let start = |text: &'static str| move |_: u32| Some(text.to_owned());
        let unread = |_: u32| None;
        let mut workspace = session();
        workspace.heartbeat_at = Some(0);
        assert!(!workspace.session_over(|_| false, unread));
        workspace.closed_at = Some(120);
        assert!(workspace.session_over(|_| true, unread));

        let mut background = session();
        background.workspace_id = Some("background:42:Sat_Oct_3_10:00:01_2026".into());
        background.heartbeat_at = Some(0);
        assert!(!background.session_over(|pid| pid == 42, start("Sat Oct  3 10:00:01 2026")));
        assert!(!background.session_over(|_| true, unread));
        assert!(background.session_over(|_| false, unread));
        assert!(background.session_over(|_| true, start("Sun Oct  4 09:00:00 2026")));
    }
}
