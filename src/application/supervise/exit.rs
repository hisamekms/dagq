//! Worker wrapper shutdown through its exit request file.
use super::*;
use crate::domain::EventKind;

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
    pub(super) fn poll(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<bool> {
        let Some(session) = &self.session else {
            return Ok(true);
        };
        let processes = sv.queue.processes(run.id())?;
        let wrapper = processes.iter().find(|p| p.role == "wrapper");
        if wrapper
            .is_none_or(|w| w.exited_at.is_some() || wrapper_dead(sv, w, sv.generators.clock.now()))
        {
            return Ok(true);
        }
        if !self.requested {
            sv.queue.record_runtime_event(
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

/// Records `wrapper_heartbeat_expired` once per silence. A watch clears
/// `noted` when a fresh heartbeat lets it wait again. If the process is
/// gone, read its row again so a concurrently recorded exit is `Exited`;
/// otherwise recover the lost session through the caller's error path.
pub(super) fn wrapper_pulse(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    wrapper: &RunProcess,
    workspace: &str,
    noted: &mut bool,
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
        bail!("{message}");
    }
    if !*noted {
        sv.queue.record_runtime_event(
            run.id(),
            EventKind::WrapperHeartbeatExpired,
            json!({"code": ReasonCode::HeartbeatLost, "pid": wrapper.pid, "heartbeat_age_secs": age, "workspace_id": workspace}),
        )?;
        info!(run_id = %run.id(), "wrapper of {} (pid {}) stopped heartbeating {age}s ago but its process is alive; asking its session in workspace {workspace} to exit", run.id(), wrapper.pid);
        *noted = true;
    }
    Ok(WrapperPulse::Silent)
}
