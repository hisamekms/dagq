//! The handoff's side of the slots (ADR-0045 decision 10): what of a slot
//! the next process rebuilds, the stop and snapshot of the slots before the
//! exec and their rebuild after it, the way an adoption rebuilds a slot
//! (ADR-0039). The slots are 実行と着地's; the registration and the request
//! are host運用's ([`handoff`]), and the plan and goal reviews 計画管理's,
//! whose own operations stop and close them. Like `handoff.rs`, this file
//! is a boundary with the real processes, which the e2e covers (the `[e2e]`
//! paths of `dagq.toml`).

use super::*;
use crate::domain::EventKind;

impl Phase {
    /// Whether the next process can rebuild this phase from the queue and
    /// the run files: everything but a validation, an e2e (ADR-t1233-2) or a
    /// landing in progress (and a run waiting for the landing slot, which
    /// starts one), which a handoff waits for. A headless review or triage is rebuildable because
    /// it is stopped and started again.
    pub(super) fn rebuildable(&self) -> bool {
        !matches!(
            self,
            Phase::Validating(..) | Phase::AwaitingSlot | Phase::E2e(_) | Phase::Landing(_)
        )
    }
}

impl Supervisor<'_> {
    /// The last step before the exec: stop the observer and every headless
    /// job (their runs start them again) but a plan or goal review that
    /// already ended, whose verdict is applied, give a triaged run's lease back
    /// (the next process triages it again), and write what a resumed
    /// session or a rejected run's `/exit` needs. Returns how many runs the
    /// next process takes over.
    pub(super) fn prepare_handoff(&mut self) -> usize {
        self.stop_observer("for the handoff; it runs again when due");
        // The throughput review goes on through the exec and records its
        // own finish; its start keeps the next process from starting it
        // again (a weekly review may take longer than the time between two
        // updates).
        self.on_planning(|planning, env| planning.stop_for_handoff(env));
        let mut kept = 0;
        for mut slot in self.claim.slots.take_all() {
            let run = slot.run.clone();
            // A live session's recovery job does not outlive this process;
            // its alert starts another once the run is rebuilt.
            stop_recovery(&mut slot);
            let snapshot = match &mut slot.phase {
                Phase::Review(watch) => {
                    watch.job.abandon();
                    info!(run_id = %run.id(), "run {}: review {} stopped for the handoff; it is reviewed again", run.id(), watch.attempt);
                    None
                }
                Phase::Recovery(watch) => {
                    watch.job.abandon();
                    info!(run_id = %run.id(), "run {}: recovery round {} stopped for the handoff; it is taken again", run.id(), watch.round);
                    if let Err(error) = self.queue.release_lease(run.id(), &self.registration.token)
                    {
                        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not release the lease: {error:#}", run.id());
                    }
                    continue;
                }
                Phase::Resume(watch) => Some(handoff::Snapshot::Resume {
                    workspace: watch.workspace.clone(),
                    attempt: watch.attempt,
                    started_at: handoff::seconds(watch.started_at),
                    message: watch.message.clone(),
                    message_bytes: Some(watch.message_bytes.clone()),
                    message_sent_at: watch.message_sent.map(|(_, at)| handoff::seconds(at)),
                    not_ready_asked: false,
                    exit_requested: watch.exit_requested.is_some(),
                    exit_typed: watch.exit_requested.is_some(),
                    exit_for_silence: false,
                    approved: watch.approved,
                }),
                Phase::Exiting(watch) if run.status() != RunStatus::AwaitingIntegration => {
                    match watch.then {
                        AfterExit::Rest { close } => Some(handoff::Snapshot::Exit {
                            workspace: watch.session.as_ref().map(|s| s.workspace.clone()),
                            resume: watch.session.as_ref().and_then(|s| s.resume),
                            close,
                            requested: watch.requested,
                            timed_out: false,

                            exit_for_silence: false,
                        }),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(snapshot) = snapshot {
                let written = run
                    .run_dir()
                    .context("missing run directory")
                    .and_then(|dir| {
                        handoff::write_snapshot(
                            &*self.files,
                            &self.registration.token,
                            Path::new(dir),
                            snapshot,
                        )
                    });
                if let Err(error) = written {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its handoff state could not be written; the next supervisor gives its lease back: {error:#}", run.id());
                }
            }
            kept += 1;
        }
        kept
    }

    /// The first step after the exec: a slot for every run whose lease
    /// carries this process's token, rebuilt the way an adopted run's is
    /// (ADR-0039) or from its `handoff.json`. A `needs_session` run without
    /// one whose resumed session lives on is rebuilt from its events like an
    /// adopted one (task 640). A run that cannot be rebuilt
    /// gives its lease back, so the supervisor's resume or triage (or
    /// `recover`) picks it up; one whose rebuild fails is abandoned like
    /// any other runtime error.
    pub(super) fn rebuild_own_runs(&mut self, previous_version: Option<&str>) -> Result<()> {
        self.on_planning(|planning, env| planning.interrupt_for_handoff(env))?;
        for run in self.queue.runs_leased_by(&self.registration.token)? {
            let snapshot = handoff::take_snapshot(&*self.files, &self.registration.token, &run);
            self.queue.record_runtime_event(
                run.id(),
                EventKind::SupervisorHandedOff,
                json!({
                    "supervisor": self.registration.token,
                    "pid": self.layout.pid,
                    "previous_version": previous_version,
                    "version": self.layout.version,
                    "status": run.status().as_str(),
                    "state": snapshot.as_ref().map(|s| match s {
                        handoff::Snapshot::Resume { .. } => "resume",
                        handoff::Snapshot::Exit { .. } => "exit",
                    }),
                }),
            )?;
            // A resume whose `handoff.json` could not be written (or is
            // gone) is rebuilt from its events while its session lives on,
            // as an adopter does (task 640).
            // A lookup that fails gives this run's lease back rather than
            // stopping the takeover of the others.
            let adopted_resume = snapshot.is_none()
                && run.status() == RunStatus::NeedsSession
                && {
                    let adoptable = self.queue.processes(run.id()).and_then(|processes| {
                        let wrapper = processes.into_iter().find(|p| p.role == "wrapper");
                        self.resume_adoptable(&run, wrapper.as_ref())
                    });
                    adoptable.unwrap_or_else(|error| {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: whether its resumed session lives could not be read; its lease is given back: {error:#}", run.id());
                    false
                })
                };
            let resumed =
                adopted_resume || matches!(snapshot, Some(handoff::Snapshot::Resume { .. }));
            let phase = match (run.status(), snapshot) {
                (_, Some(snapshot)) => self.rebuild_from(&run, snapshot),
                (RunStatus::NeedsSession, None) if adopted_resume => {
                    self.adopt_resume(&run).map(Phase::Resume)
                }
                (
                    RunStatus::Claimed
                    | RunStatus::Starting
                    | RunStatus::Running
                    | RunStatus::Validating,
                    None,
                ) => self.resume(&run),
                (RunStatus::AwaitingIntegration, None) => self.adopt_review(&run),
                (status, None) => {
                    info!(run_id = %run.id(), "run {} ({}) has nothing to take over after the handoff; its lease is given back", run.id(), status.as_str());
                    self.queue
                        .release_lease(run.id(), &self.registration.token)?;
                    continue;
                }
            };
            match phase {
                Ok(phase) => {
                    info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} taken over after the handoff ({})", run.id(), run.task_id(), run.status().as_str());
                    // A resumed session goes on watched instead of being
                    // resumed again (ADR-0047 decision 24).
                    if let (true, Phase::Resume(watch)) = (resumed, &phase) {
                        self.note_resume_adopted(&run, watch, Some(previous_version));
                    }
                    let mut slot = Slot::new(run, phase);
                    // Kept as it was, even past the limit (ADR-0062
                    // decision 7).
                    self.restore_waiting(&mut slot, true)?;
                    self.claim.slots.admit_noting(slot, "handoff", &*self.queue);
                }
                Err(error) => {
                    let message = format!(
                        "run {} could not be taken over after the handoff: {error:#}",
                        run.id()
                    );
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{}", message);
                    self.abandon(&run, message, &reason_of_error(&error, ReasonCode::Other));
                }
            }
        }
        Ok(())
    }

    fn rebuild_from(&mut self, run: &TaskRun, snapshot: handoff::Snapshot) -> Result<Phase> {
        Ok(match snapshot {
            handoff::Snapshot::Resume {
                workspace,
                attempt,
                started_at,
                message,
                message_bytes,
                message_sent_at,
                not_ready_asked: _,
                exit_requested,
                exit_typed: _,
                exit_for_silence,
                approved,
            } => Phase::Resume(self.rebuilt_resume(
                run,
                ResumeState {
                    workspace,
                    attempt,
                    started_at: handoff::time(started_at),
                    message,
                    message_bytes,
                    message_sent_at: message_sent_at.map(handoff::time),
                    exit_requested,

                    exit_for_silence,
                    approved,
                },
            )?),
            handoff::Snapshot::Exit {
                workspace,
                resume,
                close,
                requested,
                timed_out: _,

                exit_for_silence: _,
            } => {
                let session = workspace.map(|workspace| SessionRef { workspace, resume });
                let mut watch = ExitWatch::new(session, AfterExit::Rest { close });

                // Never a second exit request (task 894); the watch has no
                // timeout since task 1437.
                watch.requested = requested;

                Phase::Exiting(watch)
            }
        })
    }
}
