//! A headless session wrapper started as a background process instead of
//! in a cmux workspace (ADR-t1404-1): the `[headless] wrapper` setting of
//! `dagq.toml` that chooses it, and the handle the run records in place of
//! a workspace ID. The handle names the wrapper by its pid and the start
//! the system recorded for it, so a pid another process took later is
//! never taken for the wrapper (decision 2).

use std::fmt;

/// Where a headless session's wrapper runs (`[headless] wrapper`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeadlessWrapper {
    /// In a cmux workspace of its own (ADR-t813-1 decision 3), the default
    /// until the evaluation (ADR-t1404-1 decision 7).
    #[default]
    Workspace,
    /// As a process detached from the supervisor, without a workspace.
    Background,
}

impl HeadlessWrapper {
    pub const ALL: [Self; 2] = [Self::Workspace, Self::Background];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Background => "background",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|wrapper| wrapper.as_str() == text)
    }
}

/// What the handle of a background wrapper starts with.
pub const HANDLE_PREFIX: &str = "background:";

/// The flag of `session` that tells the wrapper it was started in the
/// background: it needs no terminal and waits for the supervisor's record
/// of its start instead of a workspace.
pub const BACKGROUND_FLAG: &str = "--background";

/// The background wrapper `pid`, started at `start` (the system's start
/// time of the process, as [`start_token`] writes it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundHandle {
    pub pid: u32,
    pub start: String,
}

impl BackgroundHandle {
    /// The handle of `pid`, whose start the system prints as `start`.
    pub fn new(pid: u32, start: &str) -> Self {
        Self {
            pid,
            start: start_token(start),
        }
    }

    /// The handle `id` names; `None` for a workspace ID.
    pub fn parse(id: &str) -> Option<Self> {
        let rest = id.strip_prefix(HANDLE_PREFIX)?;
        let (pid, start) = rest.split_once(':')?;
        let pid = pid.parse().ok().filter(|pid| *pid > 0)?;
        (!start.is_empty()).then(|| Self {
            pid,
            start: start.to_owned(),
        })
    }

    /// Whether the process the system shows as `pid` with the start
    /// `start` is this wrapper.
    pub fn is(&self, pid: u32, start: Option<&str>) -> bool {
        pid == self.pid && start.is_some_and(|start| start_token(start) == self.start)
    }
}

impl fmt::Display for BackgroundHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{HANDLE_PREFIX}{}:{}", self.pid, self.start)
    }
}

/// Whether `id`, recorded as a run's workspace, is a background wrapper's
/// handle.
pub fn is_background(id: &str) -> bool {
    id.starts_with(HANDLE_PREFIX)
}

/// A system's start time (`ps -o lstart=`, with its runs of spaces) as one
/// word.
pub fn start_token(start: &str) -> String {
    start.split_whitespace().collect::<Vec<_>>().join("_")
}

/// The session a run opened last, of its events `events` (oldest first):
/// the workspace ID or background handle of its last `workspace_created`
/// (the first session's, a resume's or a reopening's), else `first`, the
/// run's own record of its first session. The setting is read as each
/// session starts, so a run's sessions may be of either kind.
pub fn last_session<'a, I>(events: I, first: Option<&'a str>) -> Option<&'a str>
where
    I: DoubleEndedIterator<Item = &'a super::RunEvent>,
{
    events
        .rev()
        .find(|event| event.kind == super::event_kind::WORKSPACE_CREATED)
        .and_then(|event| event.payload["workspace_id"].as_str())
        .or(first)
}

/// Whether the run's events `events` (oldest first) record that the
/// session it opened last ended: the wrapper recorded its exit
/// (`session_exited`) or the session's workspace or handle was closed
/// after it opened. A background wrapper that is gone without either died.
pub fn last_session_ended<'a, I>(events: I) -> bool
where
    I: DoubleEndedIterator<Item = &'a super::RunEvent>,
{
    events
        .rev()
        .take_while(|event| event.kind != super::event_kind::WORKSPACE_CREATED)
        .any(|event| {
            matches!(
                event.kind.as_str(),
                super::event_kind::SESSION_EXITED | super::event_kind::WORKSPACE_CLOSED
            )
        })
}

/// Whether the last background start a run recorded (`wrapper_launched`)
/// is of this process, `pid` started at `own_start` (as the system prints
/// it): what a background wrapper waits for before it registers, in place
/// of a workspace (ADR-t1404-1 decision 2). An earlier session's start,
/// with another pid, does not count, nor does a start recorded for the
/// same pid with another start time (a process that took the pid of a
/// wrapper that died). A record without a start is told by its pid alone.
pub fn launched_as(events: &[super::RunEvent], pid: u32, own_start: Option<&str>) -> bool {
    events
        .iter()
        .rfind(|event| event.kind == super::event_kind::WRAPPER_LAUNCHED)
        .is_some_and(|event| {
            event.payload["pid"].as_u64() == Some(u64::from(pid))
                && match event.payload["start"].as_str() {
                    Some(start) => own_start.is_some_and(|own| start_token(own) == start),
                    None => true,
                }
        })
}

/// The events of the session the run opened last: from its last
/// `workspace_created` (every session's first record, a workspace's or a
/// background wrapper's) on, or all of them when there is none. The run's
/// registered wrapper and agent are that session's: a resume or a
/// reopening clears the earlier session's.
fn last_session_events(events: &[super::RunEvent]) -> &[super::RunEvent] {
    let from = events
        .iter()
        .rposition(|event| event.kind == super::event_kind::WORKSPACE_CREATED)
        .unwrap_or(0);
    &events[from..]
}

/// The background start the run recorded for the wrapper `pid` of the
/// session it opened last (that session's `wrapper_launched` with the pid
/// and a start); `None` for a wrapper started in a workspace, the start of
/// an earlier session's wrapper whose pid a later one took included.
pub fn launch_of(events: &[super::RunEvent], pid: u32) -> Option<BackgroundHandle> {
    last_session_events(events)
        .iter()
        .rev()
        .filter(|event| event.kind == super::event_kind::WRAPPER_LAUNCHED)
        .find(|event| event.payload["pid"].as_u64() == Some(u64::from(pid)))
        .and_then(|event| {
            Some(BackgroundHandle {
                pid,
                start: event.payload["start"].as_str()?.to_owned(),
            })
        })
}

/// Whether the wrapper `pid`, whose process the system shows started at
/// `start_now` (`None`: no such process), is the one the run recorded: a
/// background wrapper is alive only with the start recorded at its launch
/// ([`launch_of`], ADR-t1404-1 decision 2), so that a process that took
/// the pid of a wrapper that died is not taken for it. A wrapper started
/// in a workspace is told by its pid, as before.
pub fn wrapper_is_recorded(events: &[super::RunEvent], pid: u32, start_now: Option<&str>) -> bool {
    match launch_of(events, pid) {
        Some(launch) => launch.is(pid, start_now),
        None => start_now.is_some(),
    }
}

/// The turn with the pid `pid` the session the run opened last recorded
/// starting, when that session is a background wrapper's (its last
/// `turn_started` with that pid and a start): a run's agent that is such a
/// turn is judged by its recorded start, and outlives a wrapper that died
/// (ADR-t1404-1 decisions 2 and 3). `None` for a turn of a session in a
/// workspace, an earlier background session's turn whose pid a later
/// session's took included.
pub fn background_turn(events: &[super::RunEvent], pid: u32) -> Option<BackgroundHandle> {
    let session = last_session_events(events);
    let background = session.first().is_some_and(|event| {
        event.kind == super::event_kind::WORKSPACE_CREATED
            && event.payload["workspace_id"]
                .as_str()
                .is_some_and(is_background)
    });
    if !background {
        return None;
    }
    session
        .iter()
        .rev()
        .filter(|event| event.kind == super::event_kind::TURN_STARTED)
        .find(|event| event.payload["pid"].as_u64() == Some(u64::from(pid)))
        .and_then(|event| {
            Some(BackgroundHandle {
                pid,
                start: event.payload["start"].as_str()?.to_owned(),
            })
        })
}

/// The last turn the background session `handle` recorded starting
/// (`turn_started` with its `pid` and `start`, after the session's
/// `wrapper_launched` and before the next session's first record, its
/// `workspace_created`, which a session in a workspace records too), as a
/// handle of its process: a turn leads a process group of its own and outlives a wrapper
/// that died, so whatever stops the session stops it by this record
/// (ADR-t1404-1 decision 3).
pub fn last_turn_of(events: &[super::RunEvent], handle: &str) -> Option<BackgroundHandle> {
    let launched = events.iter().rposition(|event| {
        event.kind == super::event_kind::WRAPPER_LAUNCHED
            && event.payload["workspace_id"].as_str() == Some(handle)
    })?;
    events[launched + 1..]
        .iter()
        .take_while(|event| {
            !matches!(
                event.kind.as_str(),
                super::event_kind::WORKSPACE_CREATED | super::event_kind::WRAPPER_LAUNCHED
            )
        })
        .filter(|event| event.kind == super::event_kind::TURN_STARTED)
        .last()
        .and_then(|event| {
            let pid = u32::try_from(event.payload["pid"].as_u64()?).ok()?;
            Some(BackgroundHandle {
                pid,
                start: event.payload["start"].as_str()?.to_owned(),
            })
        })
}

/// The last turn a headless planner recorded starting (ADR-t1394-2), from
/// its `turn_*` events (`events`, the queue's that name it): a planner has
/// one session, so its last turn is its session's, as [`last_turn_of`]
/// finds a run's session's.
pub fn last_planner_turn(events: &[super::RunEvent]) -> Option<BackgroundHandle> {
    events
        .iter()
        .rfind(|event| event.kind == super::event_kind::TURN_STARTED)
        .and_then(|event| {
            let pid = u32::try_from(event.payload["pid"].as_u64()?).ok()?;
            Some(BackgroundHandle {
                pid,
                start: event.payload["start"].as_str()?.to_owned(),
            })
        })
}

/// A session wrapper started in the background, as a person reads it
/// (`show`, `status`, `planners`, ADR-t1404-1 decision 6): its handle, pid
/// and start, and the log its output goes to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BackgroundSession {
    pub handle: String,
    pub pid: u32,
    pub start: String,
    pub log: String,
}

impl BackgroundSession {
    /// The wrapper `handle`, writing to `log`; `None` for a workspace ID.
    pub fn of(handle: &str, log: String) -> Option<Self> {
        let parsed = BackgroundHandle::parse(handle)?;
        Some(Self {
            handle: handle.to_owned(),
            pid: parsed.pid,
            start: parsed.start,
            log,
        })
    }

    /// The process the handle names.
    pub fn wrapper(&self) -> BackgroundHandle {
        BackgroundHandle {
            pid: self.pid,
            start: self.start.clone(),
        }
    }
}

/// The background session a run started last, of its events `events`
/// (oldest first): the handle and log of its last `wrapper_launched`, an
/// ended one included, so that its log is read after the run ended.
pub fn last_background_session(events: &[super::RunEvent]) -> Option<BackgroundSession> {
    events
        .iter()
        .rfind(|event| event.kind == super::event_kind::WRAPPER_LAUNCHED)
        .and_then(|event| {
            BackgroundSession::of(
                event.payload["workspace_id"].as_str()?,
                event.payload["log"].as_str()?.to_owned(),
            )
        })
}

/// The session the run opened last when it was started in the background
/// ([`last_background_session`] with no later session's
/// `workspace_created`); `None` while a session in a workspace is the
/// last.
pub fn current_background_session(events: &[super::RunEvent]) -> Option<BackgroundSession> {
    let launched = events
        .iter()
        .rposition(|event| event.kind == super::event_kind::WRAPPER_LAUNCHED)?;
    if events[launched + 1..]
        .iter()
        .any(|event| event.kind == super::event_kind::WORKSPACE_CREATED)
    {
        return None;
    }
    last_background_session(events)
}

/// How the stop of a background wrapper ended (task 1657): the `signal`
/// of `wrapper_stopped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSignal {
    /// It exited within the grace after SIGTERM.
    Term,
    /// It was still running after the grace and was sent SIGKILL.
    Kill,
    /// It was not running when the stop began: nothing was signaled.
    Gone,
}

impl StopSignal {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Term => "sigterm",
            Self::Kill => "sigkill",
            Self::Gone => "gone",
        }
    }
}

/// What the stop of a background wrapper did: how the wrapper ended and to
/// how many of the processes it had started SIGKILL was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapperStop {
    pub signal: StopSignal,
    pub children_killed: usize,
}

/// The stop of a wrapper that was `running` when the stop began, that
/// `exited_after_term` within the grace after SIGTERM, and whose started
/// processes were sent SIGKILL `children_killed` times. A wrapper that
/// was not running was sent nothing, its children neither.
pub fn wrapper_stop(running: bool, exited_after_term: bool, children_killed: usize) -> WrapperStop {
    match (running, exited_after_term) {
        (false, _) => WrapperStop {
            signal: StopSignal::Gone,
            children_killed: 0,
        },
        (true, true) => WrapperStop {
            signal: StopSignal::Term,
            children_killed,
        },
        (true, false) => WrapperStop {
            signal: StopSignal::Kill,
            children_killed,
        },
    }
}

/// Which path of the runtime stopped a background wrapper: the `route` of
/// `wrapper_stopped` (task 1657).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopRoute {
    /// The session ended after its review (or its rebase or revise) and
    /// its handle is stopped before the run lands or asks.
    AfterReview,
    /// A landed (or succeeded) run's sessions still running.
    Landed,
    /// A failed run's sessions, stopped by its triage: a `stop` answer to
    /// `stalled`, a cancel answered on a recovery ask, a recovery job that
    /// gave up. A cancel answered on a landing ask finds the session
    /// stopped after the review, and a run ended outside the supervisor is
    /// stopped by the sweep.
    Triage,
    /// The sweep of ended runs the triage never takes.
    Sweep,
    /// A resume's session: the end of the resume, or one given up on.
    Resume,
    /// The session lost while the run waited, before it is opened again.
    Reopen,
    /// A wrapper whose start could not be recorded.
    Unrecorded,
    /// A headless planner's wrapper.
    Planner,
    /// Any other close of a background handle.
    Close,
}

impl StopRoute {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AfterReview => "after_review",
            Self::Landed => "landed",
            Self::Triage => "triage",
            Self::Sweep => "sweep",
            Self::Resume => "resume",
            Self::Reopen => "reopen",
            Self::Unrecorded => "unrecorded",
            Self::Planner => "planner",
            Self::Close => "close",
        }
    }
}

/// The log in the run dir the background wrapper of a session writes its
/// output to: `session.log` for the worker's session, one per resume and
/// per reopening (`resume` is the attempt).
pub fn session_log_name(resume: Option<usize>, reopen: bool) -> String {
    match (resume, reopen) {
        (Some(attempt), true) => format!("session-reopen-{attempt}.log"),
        (Some(attempt), false) => format!("session-resume-{attempt}.log"),
        (None, _) => "session.log".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wrapper that exits after SIGTERM is `sigterm`, one still running
    /// after the grace `sigkill`, each with the SIGKILLs sent to what it
    /// started; one that was not running is `gone`, nothing sent.
    #[test]
    fn a_stop_is_told_by_the_wait_after_sigterm_and_the_kills_sent() {
        assert_eq!(
            wrapper_stop(true, true, 0),
            WrapperStop {
                signal: StopSignal::Term,
                children_killed: 0
            }
        );
        assert_eq!(
            wrapper_stop(true, true, 2),
            WrapperStop {
                signal: StopSignal::Term,
                children_killed: 2
            }
        );
        assert_eq!(
            wrapper_stop(true, false, 1),
            WrapperStop {
                signal: StopSignal::Kill,
                children_killed: 1
            }
        );
        for exited in [true, false] {
            assert_eq!(
                wrapper_stop(false, exited, 3),
                WrapperStop {
                    signal: StopSignal::Gone,
                    children_killed: 0
                }
            );
        }
        assert_eq!(
            [StopSignal::Term, StopSignal::Kill, StopSignal::Gone].map(StopSignal::as_str),
            ["sigterm", "sigkill", "gone"]
        );
    }

    #[test]
    fn a_handle_names_the_pid_and_start_and_reads_back() {
        let handle = BackgroundHandle::new(4242, "Sat Oct  3 10:00:01 2026");
        let id = handle.to_string();
        assert_eq!(id, "background:4242:Sat_Oct_3_10:00:01_2026");
        assert!(is_background(&id));
        assert_eq!(BackgroundHandle::parse(&id), Some(handle.clone()));
        assert!(handle.is(4242, Some("Sat Oct 3 10:00:01  2026")));
        assert!(!handle.is(4242, Some("Sat Oct  3 10:00:02 2026")));
        assert!(!handle.is(4243, Some("Sat Oct  3 10:00:01 2026")));
        assert!(!handle.is(4242, None));
    }

    #[test]
    fn a_workspace_id_or_a_broken_handle_is_no_handle() {
        for id in [
            "3aa21145-c873-4cec-aee3-ee7f07f52e4a",
            "background:",
            "background:x:start",
            "background:0:start",
            "background:12:",
            "background:12",
        ] {
            assert_eq!(BackgroundHandle::parse(id), None, "{id}");
        }
        assert!(!is_background("workspace:1"));
    }

    #[test]
    fn the_setting_reads_its_two_values() {
        assert_eq!(HeadlessWrapper::default(), HeadlessWrapper::Workspace);
        for wrapper in HeadlessWrapper::ALL {
            assert_eq!(HeadlessWrapper::parse(wrapper.as_str()), Some(wrapper));
        }
        assert_eq!(HeadlessWrapper::parse("cmux"), None);
    }

    fn event(kind: &str, payload: serde_json::Value) -> super::super::RunEvent {
        super::super::RunEvent {
            id: crate::domain::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    const START: &str = "Sat Oct  3 10:00:01 2026";
    const OTHER: &str = "Sat Oct  3 11:00:00 2026";

    #[test]
    fn only_the_last_start_names_the_wrapper() {
        let pid = |pid: u32| event("wrapper_launched", serde_json::json!({"pid": pid}));
        assert!(!launched_as(&[], 7, Some(START)));
        let events = [
            pid(7),
            event("wrapper_started", serde_json::json!({"pid": 9})),
        ];
        assert!(launched_as(&events, 7, Some(START)));
        assert!(!launched_as(&events, 9, Some(START)));
        let resumed = [events[0].clone(), pid(8)];
        assert!(!launched_as(&resumed, 7, Some(START)));
        assert!(launched_as(&resumed, 8, Some(START)));
    }

    /// A start recorded with the pid tells the wrapper from a process that
    /// took the pid later (ADR-t1404-1 decision 2).
    #[test]
    fn a_recorded_start_must_be_the_process_own() {
        let handle = BackgroundHandle::new(7, START);
        let events = [event(
            "wrapper_launched",
            serde_json::json!({"pid": 7, "start": handle.start, "workspace_id": handle.to_string()}),
        )];
        assert!(launched_as(&events, 7, Some(START)));
        assert!(!launched_as(&events, 7, Some(OTHER)));
        assert!(!launched_as(&events, 7, None));
        assert_eq!(launch_of(&events, 7), Some(handle));
        assert_eq!(launch_of(&events, 8), None);
        assert!(wrapper_is_recorded(&events, 7, Some(START)));
        assert!(!wrapper_is_recorded(&events, 7, Some(OTHER)));
        assert!(!wrapper_is_recorded(&events, 7, None));
        // A wrapper in a workspace: its pid alone.
        assert!(wrapper_is_recorded(&events, 8, Some(OTHER)));
        assert!(!wrapper_is_recorded(&events, 8, None));
    }

    #[test]
    fn a_turn_of_a_background_session_is_found_by_its_pid() {
        let turn = |pid: u32| {
            event(
                "turn_started",
                serde_json::json!({"pid": pid, "start": start_token(START)}),
            )
        };
        let created =
            |id: &str| event("workspace_created", serde_json::json!({"workspace_id": id}));
        let handle = BackgroundHandle::new(7, START).to_string();
        let events = [
            created("3aa21145-c873-4cec-aee3-ee7f07f52e4a"),
            turn(70),
            created(&handle),
            turn(71),
            event("turn_started", serde_json::json!({"pid": 72})),
        ];
        // A turn of a workspace's session is not one.
        assert_eq!(background_turn(&events, 70), None);
        let found = background_turn(&events, 71).unwrap();
        assert!(found.is(71, Some(START)));
        assert!(!found.is(71, Some(OTHER)));
        // Nor one whose start was not recorded.
        assert_eq!(background_turn(&events, 72), None);
        assert_eq!(background_turn(&events, 73), None);
    }

    #[test]
    fn the_last_turn_of_a_session_is_its_own() {
        let first = BackgroundHandle::new(7, START).to_string();
        let second = BackgroundHandle::new(8, OTHER).to_string();
        let launched = |handle: &str| {
            event(
                "wrapper_launched",
                serde_json::json!({"workspace_id": handle}),
            )
        };
        let turn = |pid: u32| {
            event(
                "turn_started",
                serde_json::json!({"pid": pid, "start": start_token(START)}),
            )
        };
        let events = [
            launched(&first),
            turn(70),
            turn(71),
            launched(&second),
            turn(80),
            event("turn_started", serde_json::json!({"pid": 81})),
        ];
        assert_eq!(last_turn_of(&events, &first).unwrap().pid, 71);
        // A turn whose start was not recorded is not one to signal.
        assert_eq!(last_turn_of(&events, &second), None);
        assert_eq!(last_turn_of(&events[..5], &second).unwrap().pid, 80);
        assert_eq!(last_turn_of(&events, "background:9:x"), None);
        assert_eq!(last_turn_of(&events[..1], &first), None);
    }

    /// A run resumed in a workspace after a background session (the
    /// setting changed between them): the workspace session records its
    /// `workspace_created` but no `wrapper_launched`, and its turns are not
    /// the background session's, which stopping that session must never
    /// reach.
    #[test]
    fn a_later_session_in_a_workspace_ends_the_background_sessions_turns() {
        let background = BackgroundHandle::new(7, START).to_string();
        let turn = |pid: u32| {
            event(
                "turn_started",
                serde_json::json!({"pid": pid, "start": start_token(OTHER)}),
            )
        };
        let mut events = vec![
            event(
                "workspace_created",
                serde_json::json!({"workspace_id": background}),
            ),
            event(
                "wrapper_launched",
                serde_json::json!({"workspace_id": background}),
            ),
            turn(70),
            event(
                "workspace_created",
                serde_json::json!({"workspace_id": "WS-RESUME", "resume_attempt": 1}),
            ),
            turn(90),
        ];
        assert_eq!(last_turn_of(&events, &background).unwrap().pid, 70);
        // The background session started no turn: the resume's is not its.
        events.remove(2);
        assert_eq!(last_turn_of(&events, &background), None);
    }

    /// A run resumed in a workspace after a background session whose
    /// wrapper's and turn's pids the resume's wrapper and turn took: the
    /// earlier session's starts are not the new processes', so the
    /// wrapper is told by its pid as any wrapper in a workspace, and the
    /// agent is no background turn.
    #[test]
    fn a_workspace_session_after_a_background_one_is_judged_as_a_workspace() {
        let handle = BackgroundHandle::new(7, START);
        let turn = |start: &str| {
            event(
                "turn_started",
                serde_json::json!({"pid": 70, "start": start_token(start)}),
            )
        };
        let mut events = vec![
            event(
                "workspace_created",
                serde_json::json!({"workspace_id": handle.to_string()}),
            ),
            event(
                "wrapper_launched",
                serde_json::json!({"pid": 7, "start": handle.start, "workspace_id": handle.to_string()}),
            ),
            turn(START),
        ];
        // The background session itself.
        assert_eq!(launch_of(&events, 7), Some(handle.clone()));
        assert!(!wrapper_is_recorded(&events, 7, Some(OTHER)));
        assert_eq!(
            background_turn(&events, 70).unwrap().start,
            start_token(START)
        );
        // Resumed in a workspace, whose wrapper and turn took pids 7 and 70.
        events.push(event(
            "workspace_created",
            serde_json::json!({"workspace_id": "WS-RESUME", "resume_attempt": 1}),
        ));
        events.push(turn(OTHER));
        assert_eq!(launch_of(&events, 7), None);
        assert!(wrapper_is_recorded(&events, 7, Some(OTHER)));
        assert!(!wrapper_is_recorded(&events, 7, None));
        assert_eq!(background_turn(&events, 70), None);
        // Back to the background: the new session's own records count.
        let next = BackgroundHandle::new(8, OTHER);
        events.push(event(
            "workspace_created",
            serde_json::json!({"workspace_id": next.to_string()}),
        ));
        events.push(event(
            "wrapper_launched",
            serde_json::json!({"pid": 8, "start": next.start, "workspace_id": next.to_string()}),
        ));
        assert_eq!(launch_of(&events, 7), None);
        assert_eq!(launch_of(&events, 8), Some(next));
        assert_eq!(background_turn(&events, 70), None);
    }

    #[test]
    fn each_session_has_its_own_log() {
        assert_eq!(session_log_name(None, false), "session.log");
        assert_eq!(session_log_name(Some(2), false), "session-resume-2.log");
        assert_eq!(session_log_name(Some(1), true), "session-reopen-1.log");
    }

    #[test]
    fn a_planners_last_turn_is_its_last_turn_started() {
        assert_eq!(last_planner_turn(&[]), None);
        let started = |pid: u32, start: &str| {
            event(
                "turn_started",
                serde_json::json!({"planner_id": 1, "pid": pid, "start": start_token(start)}),
            )
        };
        let events = [
            started(70, START),
            event("turn_finished", serde_json::json!({"planner_id": 1})),
            started(71, OTHER),
            event("turn_finished", serde_json::json!({"planner_id": 1})),
        ];
        assert_eq!(
            last_planner_turn(&events),
            Some(BackgroundHandle::new(71, OTHER))
        );
        // A turn whose agent did not start records no pid.
        let events = [
            started(70, START),
            event(
                "turn_started",
                serde_json::json!({"planner_id": 1, "pid": null}),
            ),
        ];
        assert_eq!(last_planner_turn(&events), None);
    }

    #[test]
    fn a_runs_background_session_is_its_last_wrapper_launched() {
        let created =
            |id: &str| event("workspace_created", serde_json::json!({"workspace_id": id}));
        let launched = |handle: &BackgroundHandle, log: &str| {
            event(
                "wrapper_launched",
                serde_json::json!({"pid": handle.pid, "start": handle.start, "workspace_id": handle.to_string(), "log": log}),
            )
        };
        let first = BackgroundHandle::new(40, START);
        let resume = BackgroundHandle::new(41, OTHER);
        assert_eq!(last_background_session(&[created("WS-1")]), None);
        let events = [
            created(&first.to_string()),
            launched(&first, "/r/session.log"),
            created(&resume.to_string()),
            launched(&resume, "/r/session-resume-1.log"),
        ];
        let session = current_background_session(&events).unwrap();
        assert_eq!(
            session,
            BackgroundSession {
                handle: resume.to_string(),
                pid: 41,
                start: resume.start.clone(),
                log: "/r/session-resume-1.log".into(),
            }
        );
        assert_eq!(session.wrapper(), resume);
        assert_eq!(last_background_session(&events), Some(session.clone()));
        // A later session in a workspace is the run's current one; the
        // background log stays readable.
        let mut later = events.to_vec();
        later.push(created("WS-2"));
        assert_eq!(current_background_session(&later), None);
        assert_eq!(last_background_session(&later), Some(session));
        // A record without its log, or of a workspace ID, names none.
        let bare = event(
            "wrapper_launched",
            serde_json::json!({"pid": 40, "workspace_id": first.to_string()}),
        );
        assert_eq!(last_background_session(&[bare]), None);
        assert_eq!(BackgroundSession::of("WS-1", "/x".into()), None);
    }
}
