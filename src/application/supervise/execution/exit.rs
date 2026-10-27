//! Worker wrapper shutdown through its exit request file.
use super::*;
use crate::domain::EventKind;
pub(super) use crate::domain::waiting::WaitPhase;

pub(super) struct ExitWatch {
    pub(super) session: Option<SessionRef>,
    /// Whether the wrapper was asked to exit (recorded and written once;
    /// the watch has no timeout, it waits for the wrapper to end).
    pub(super) requested: bool,
    pub(super) then: AfterExit,
}
impl ExitWatch {
    pub(super) fn new(session: Option<SessionRef>, then: AfterExit) -> Self {
        Self {
            session,
            requested: false,
            then,
        }
    }
    /// Whether the session ended. A wrapper that already recorded its exit
    /// (it ended by itself, or before an adoption) or is dead is not asked
    /// to exit (a dead one has its silence recorded); otherwise `exit_requested` is recorded before the request
    /// is written, and both happen once.
    pub(super) fn poll(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<bool> {
        let Some(session) = &self.session else {
            return Ok(true);
        };
        let processes = sv.queue.processes(run.id())?;
        let wrapper = processes.iter().find(|p| p.role == "wrapper");
        let Some(wrapper) = wrapper.filter(|w| w.exited_at.is_none()) else {
            return Ok(true);
        };
        if wrapper_dead(sv, wrapper, sv.generators.clock.now()) {
            let site = PulseSite::in_slot(WaitPhase::Exit);
            note_heartbeat_expired(sv, run, wrapper, &session.workspace, site, false)?;
            return Ok(true);
        }
        if !self.requested {
            sv.queue.record_runtime_event_now(
                run.id(),
                EventKind::ExitRequested,
                json!({"workspace_id": session.workspace}),
            )?;
            submit(sv, run, &session.workspace, Input::Exit, "exit request")?;
            self.requested = true;
        }
        Ok(false)
    }
}

pub(super) enum WrapperPulse {
    Fresh,
    /// The wrapper is still alive but no longer heartbeats; request its exit.
    Silent,
    /// The wrapper recorded its exit after this poll read its row: the next
    /// poll handles the exit.
    Exited,
}

/// Where a wrapper's expired heartbeat was seen: the watch (`phase`) and
/// whether the run waited out of its slot then. `wrapper_heartbeat_expired`
/// carries both.
#[derive(Debug, Clone, Copy)]
pub(super) struct PulseSite {
    pub(super) phase: WaitPhase,
    pub(super) waiting: bool,
}

impl PulseSite {
    pub(super) const fn in_slot(phase: WaitPhase) -> Self {
        Self {
            phase,
            waiting: false,
        }
    }
}

/// Records `wrapper_heartbeat_expired` once per silence. A watch clears
/// `noted` when a fresh heartbeat lets it wait again. If the process is
/// gone, read its row again so a concurrently recorded exit is `Exited`;
/// otherwise record the silence as one of a dead process (unless it was
/// recorded already, [`note_heartbeat_expired`]) and recover the lost
/// session through the caller's error path.
///
/// The wrapper writes its heartbeat, and the queue the events'
/// `created_at`, on the wall clock, while this compares them with the
/// injected `Clock`: a `supervise` test with a wrapper must not fix the
/// clock, or a live wrapper reads as silent or dead; fix it only on paths
/// without one. A time compared with a receipt's or an idle marker's mtime
/// is read from the wall clock as well, so that it agrees with the files'.
pub(super) fn wrapper_pulse(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    wrapper: &RunProcess,
    workspace: &str,
    noted: &mut bool,
    site: PulseSite,
    message: &str,
) -> Result<WrapperPulse> {
    let age = sv.generators.clock.now() - wrapper.heartbeat_at;
    if age <= HEARTBEAT_TIMEOUT_SECS {
        return Ok(WrapperPulse::Fresh);
    }
    if !sv.wrapper_lives(wrapper) {
        let exited = sv
            .queue
            .processes(run.id())?
            .iter()
            .any(|p| p.role == "wrapper" && p.pid == wrapper.pid && p.exited_at.is_some());
        if exited {
            return Ok(WrapperPulse::Exited);
        }
        note_heartbeat_expired(sv, run, wrapper, workspace, site, false)?;
        *noted = true;
        bail!("{message}");
    }
    if !*noted {
        // A watch rebuilt after a handoff or an adoption starts unnoted:
        // the events tell whether this silence was recorded already.
        note_heartbeat_expired(sv, run, wrapper, workspace, site, true)?;
        info!(run_id = %run.id(), "wrapper of {} (pid {}) stopped heartbeating {age}s ago but its process is alive; asking its session in workspace {workspace} to exit", run.id(), wrapper.pid);
        *noted = true;
    }
    Ok(WrapperPulse::Silent)
}

/// Records `wrapper_heartbeat_expired` for `wrapper`'s silence unless one
/// was recorded for it already: a silence is the wrapper's pid with its
/// last heartbeat, so a watch that sees it again (the next tick, the same
/// run back in its slot, another supervisor) records nothing, and a
/// silence after a heartbeat came back is a new one.
pub(super) fn note_heartbeat_expired(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    wrapper: &RunProcess,
    workspace: &str,
    site: PulseSite,
    process_alive: bool,
) -> Result<()> {
    if silence_recorded(&sv.queue.run_events(run.id())?, wrapper) {
        return Ok(());
    }
    record_heartbeat_expired(sv, run, wrapper, workspace, site, process_alive)?;
    if !process_alive {
        warn!(run_id = %run.id(), "wrapper of {} (pid {}) stopped heartbeating and its process is gone", run.id(), wrapper.pid);
    }
    Ok(())
}

fn record_heartbeat_expired(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    wrapper: &RunProcess,
    workspace: &str,
    site: PulseSite,
    process_alive: bool,
) -> Result<()> {
    let age = sv.generators.clock.now() - wrapper.heartbeat_at;
    sv.queue.record_runtime_event(
        run.id(),
        EventKind::WrapperHeartbeatExpired,
        json!({
            "code": ReasonCode::HeartbeatLost,
            "pid": wrapper.pid,
            "heartbeat_at": wrapper.heartbeat_at,
            "heartbeat_age_secs": age,
            "workspace_id": workspace,
            "process_alive": process_alive,
            "phase": site.phase.as_str(),
            "waiting": site.waiting,
        }),
    )?;
    Ok(())
}

/// Whether `events` hold the `wrapper_heartbeat_expired` of `wrapper`'s
/// current silence: one of its pid with its last heartbeat.
fn silence_recorded(events: &[RunEvent], wrapper: &RunProcess) -> bool {
    events.iter().any(|e| {
        e.kind == EventKind::WrapperHeartbeatExpired
            && e.payload["pid"].as_u64() == Some(u64::from(wrapper.pid))
            && e.payload["heartbeat_at"].as_i64() == Some(wrapper.heartbeat_at)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expired(pid: u32, heartbeat_at: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: "wrapper_heartbeat_expired".into(),
            payload: json!({"pid": pid, "heartbeat_at": heartbeat_at, "process_alive": true}),
            created_at: "2026-10-26T00:00:00.000Z".to_owned(),
            actor: None,
        }
    }

    fn wrapper(pid: u32, heartbeat_at: i64) -> RunProcess {
        RunProcess {
            run_id: RunId::new("run-1").unwrap(),
            role: "wrapper".to_owned(),
            pid,
            heartbeat_at,
            exited_at: None,
            exit_code: None,
        }
    }

    /// A silence is the wrapper's pid with its last heartbeat: the silence
    /// recorded while the process lived is the one it died in, and a
    /// silence after a heartbeat came back, or another wrapper's, is new.
    #[test]
    fn a_silence_is_recorded_once_per_pid_and_last_heartbeat() {
        let events = [expired(42, 100)];
        assert!(silence_recorded(&events, &wrapper(42, 100)));
        assert!(!silence_recorded(&events, &wrapper(42, 160)));
        assert!(!silence_recorded(&events, &wrapper(43, 100)));
        assert!(!silence_recorded(&[], &wrapper(42, 100)));
        // An older record without the heartbeat is not this silence's.
        let mut old = expired(42, 100);
        old.payload = json!({"pid": 42});
        assert!(!silence_recorded(&[old], &wrapper(42, 100)));
    }
}
