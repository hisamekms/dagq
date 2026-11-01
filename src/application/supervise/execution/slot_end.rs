//! Giving a slot up and ending it: whether this process still drives a
//! run in a slot ([`Supervisor::in_slot`]), dropping a slot whose lease
//! another process holds ([`Supervisor::disown`]), abandoning a run with
//! the session it kept open asked to exit
//! ([`Supervisor::abandon_with_session`],
//! [`Supervisor::exit_abandoned_session`]), giving up a resume that failed
//! in itself ([`Supervisor::give_up_resume`]) and where the first
//! session's slot goes once its supervision finished
//! ([`Supervisor::supervision_finished`]).
//!
//! The methods are part of the one `impl Supervisor<'_>` split across the
//! submodules of [`super`]; they read and change the supervisor's state
//! through the fields their module already sees, so splitting them out
//! widens no field's visibility (ADR-t2391-1 decision 1, C3). What they
//! share with the rest of [`super`] (`Supervisor::record_rest`,
//! `resumes_exhausted`, `session_alive`) stays in `execution.rs`.

use super::*;

impl Supervisor<'_> {
    /// Whether this process still drives `id` in a slot. Such a run whose
    /// lease another token holds now, stale or not, is adopted, resumed or
    /// triaged by none of the claims of a pass: its slot is dropped on its
    /// next step ([`Self::disown`]), and only a later pass may take the run
    /// like any other, never into a second slot (task 1361). A pass adopts
    /// before it steps the slots, so a lease that moved after the last
    /// step of the pass before would otherwise be adopted back while its
    /// slot still drives the run.
    pub(super) fn in_slot(&self, id: &RunId) -> bool {
        self.claim.slots.holds(id)
    }
    /// Drop a slot whose lease another process holds now, writing nothing
    /// about the run: the new owner's record is the record.
    pub(super) fn disown(&mut self, slot: &Slot) {
        // The lease went to another process or away: the telemetry token
        // issued under this one ends with it (ADR-t1662-3 decision 11).
        self.revoke_lost_telemetry(slot.run.id());
        let mut message = format!(
            "lease of run {} is held by another process; this supervisor stopped watching it",
            slot.run.id()
        );
        if matches!(slot.phase, Phase::Validating(Some(_), _)) {
            // Its checks finish on their own; the new owner runs its own.
            message.push_str("; a validation already in progress runs to completion unrecorded");
        }
        warn!(run_id = %slot.run.id(), "{}", message);
        self.errors.push(RunError {
            run_id: slot.run.id().clone(),
            task_id: slot.run.task_id(),
            message,
        });
    }
    pub(super) fn abandon(&mut self, run: &TaskRun, message: String, reason: &Reason) {
        self.abandon_with_session(run, message, reason, None);
    }
    /// [`abandon`](Self::abandon) a run whose live session the supervisor
    /// kept open, recording what became of it (`exit_abandoned_session`).
    pub(super) fn abandon_with_session(
        &mut self,
        run: &TaskRun,
        message: String,
        reason: &Reason,
        session: Option<&Value>,
    ) {
        if let Err(error) = self.queue.abandon_run(
            run.id(),
            self.registration.token(),
            &message,
            reason,
            session,
        ) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not record the error: {error:#}", run.id());
        }
        self.record_rest(run.id(), None, EventKind::RuntimeError.as_str());
        self.errors.push(RunError {
            run_id: run.id().clone(),
            task_id: run.task_id(),
            message,
        });
    }
    /// Ask the live session of a slot being given up on to end (task 237):
    /// the session kept open through validation, review, revise or its
    /// `/exit` would otherwise be left open with nobody watching it, the
    /// run without a lease. `None` when the slot holds no such session, it
    /// ended already, its background wrapper is gone, or the lease is not
    /// known to be this supervisor's; else the `session` of the
    /// `runtime_error`:
    /// `exit` is `sent`, `requested_before` (the `/exit` was typed already
    /// and is never typed twice) or `failed` with the `error`, which
    /// leaves the session to a person (`AttentionNext::ExitSession`).
    pub(super) fn exit_abandoned_session(&mut self, slot: &Slot) -> Option<Value> {
        let (workspace, requested) = open_session(&slot.phase)?;
        let workspace = workspace.to_owned();
        // Nothing is asked of a session this supervisor may no longer own
        // (the lease check itself failed), nor of one that ended already; a
        // workspace from before ADR-t1433-3 is not asked of cmux, and its
        // wrapper's registration alone says whether it ended
        // (`run_session_gone`).
        if !matches!(
            self.queue
                .holds_lease(slot.run.id(), self.registration.token()),
            Ok(true)
        ) || matches!(session_alive(self, slot.run.id()), Ok(false))
            || matches!(self.run_session_gone(&workspace), Ok(true))
        {
            return None;
        }
        let (exit, error) = if requested {
            (ABANDON_EXIT_REQUESTED_BEFORE, None)
        } else {
            let failed = |why: String| (ABANDON_EXIT_FAILED, Some(why));
            match submit(self, &slot.run, &workspace, Input::Exit, "/exit") {
                Ok(_) => (ABANDON_EXIT_SENT, None),
                Err(error) => failed(format!("{error:#}")),
            }
        };
        match &error {
            Some(error) => {
                warn!(run_id = %slot.run.id(), "run {}: /exit could not be sent to its session in {workspace}: {error}; the session is left to a person", slot.run.id());
            }
            None => {
                info!(run_id = %slot.run.id(), "run {}: its session in {workspace} is asked to exit ({exit})", slot.run.id());
            }
        }
        let mut session = json!({"workspace_id": workspace, "exit": exit});
        if let Some(error) = error {
            session["error"] = json!(error);
        }
        Some(session)
    }
    /// A resume that failed in itself (not the session's verdict): record
    /// `resume_finished` with outcome `error` and give the lease back; the
    /// run stays `needs_session` with its reason, and the attempt counts.
    /// The resume workspace is closed only when no session of this resume
    /// can be alive: its wrapper never registered (and, the lease gone, no
    /// longer can) or already exited, by the run's `processes` as the
    /// caller read them while it held the lease, or as they are read once
    /// the resume is recorded when the caller read none. `workspace` is
    /// `None` when opening it failed before cmux returned its ID;
    /// workspaces are never looked up by title (ADR-0026).
    pub(super) fn give_up_resume(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        workspace: Option<&str>,
        message: String,
        reason: &Reason,
        processes: Option<Vec<RunProcess>>,
    ) {
        let workspace = workspace.map(str::to_owned);
        // The workspace is recorded so a later pass knows it for this run's
        // own once its session has ended (`close_left_resume_workspaces`).
        let payload = reason.on(json!({
            "attempt": attempt,
            "outcome": "error",
            "error": message,
            "workspace_id": workspace,
            "exhausted": resumes_exhausted(&*self.queue, run.id(), self.resume.config).ok(),
        }));
        if let Err(error) = self.queue.finish_resume(
            run.id(),
            self.registration.token(),
            None,
            None,
            false,
            payload,
        ) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not record the resume error: {error:#}", run.id());
        }
        self.record_rest(run.id(), None, EventKind::ResumeFinished.as_str());
        let session_may_live = processes
            .map_or_else(|| self.queue.processes(run.id()), Ok)
            .map_or(true, |processes| {
                processes
                    .iter()
                    .any(|p| p.role == "wrapper" && p.exited_at.is_none())
            });
        if let Some(workspace) = workspace {
            if session_may_live {
                info!(run_id = %run.id(), "run {}: resume workspace {workspace} is kept; its session may still run", run.id());
            } else if let Err(error) =
                stop_run_session(self.sessions, &workspace, StopRoute::Resume)
            {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the resume's session {workspace} could not be stopped: {error:#}", run.id());
            }
        }
        self.errors.push(RunError {
            run_id: run.id().clone(),
            task_id: run.task_id(),
            message,
        });
    }
    /// The first session's supervision finished as `run`: its worker's
    /// run goes to validation with the session open, a run its recovery
    /// job parked to `/exit`, any other to rest.
    pub(super) fn supervision_finished(&mut self, slot: &mut Slot, run: TaskRun) -> Result<Step> {
        let Phase::Session(watch) = &slot.phase else {
            unreachable!("the session's slot is still in it");
        };
        // Parked by its recovery job's `resume` (task 442): the
        // session is asked to exit and its workspace closed, as for
        // a run parked by validation, and the lease goes.
        if run.status() == RunStatus::NeedsSession {
            let session = SessionRef {
                workspace: watch.workspace.clone(),
                resume: None,
            };
            slot.run = run;
            slot.transition(
                Phase::Exiting(ExitWatch::new(
                    Some(session),
                    AfterExit::Rest { close: true },
                )),
                EventKind::SupervisionFinished.as_str(),
                &*self.queue,
            );
            return Ok(Step::Continue);
        }
        if run.status() != RunStatus::Validating {
            self.queue
                .release_lease(run.id(), self.registration.token())?;
            return Ok(Step::Done(Box::new(run)));
        }
        let session = SessionRef {
            workspace: watch.workspace.clone(),
            resume: None,
        };
        let handle = self.validate(run.clone());
        slot.run = run;
        slot.transition(
            Phase::Validating(Some(handle), Some(session)),
            EventKind::SupervisionFinished.as_str(),
            &*self.queue,
        );
        Ok(Step::Continue)
    }
}

/// The live session a slot keeps open through validation, review, revise
/// or its `/exit` (ADR-0027), and whether the `/exit` was typed into it
/// already.
fn open_session(phase: &Phase) -> Option<(&str, bool)> {
    match phase {
        Phase::Validating(_, Some(session)) => Some((&session.workspace, false)),
        Phase::Review(watch) => watch
            .session
            .as_ref()
            .map(|session| (session.workspace.as_str(), false)),
        Phase::ReviewPrograms(watch) => watch
            .programs
            .session
            .as_ref()
            .map(|session| (session.workspace.as_str(), false)),
        Phase::Revise(watch) => Some((&watch.session.workspace, false)),
        Phase::Exiting(watch) => watch.session.as_ref().map(|session| {
            // Whether the wrapper has already been asked to exit.
            let typed = watch.requested;
            (session.workspace.as_str(), typed)
        }),
        _ => None,
    }
}
