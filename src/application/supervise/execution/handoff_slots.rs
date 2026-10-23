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
    /// the run files: everything but a validation, the verification of an
    /// `already_satisfied` claim (ADR-t1818-1), an e2e (ADR-t1233-2) or a
    /// landing in progress, which a handoff waits for. A run waiting for
    /// the landing slot is rebuildable whether its landing is held or not:
    /// a handoff starts no landing and hands it over ([`Slot::rebuildable`],
    /// ADR-t3077-1). A headless review or triage is rebuildable because it
    /// is stopped and started again.
    pub(super) fn rebuildable(&self) -> bool {
        !matches!(
            self,
            Phase::Validating(..) | Phase::Satisfied(_) | Phase::E2e(_) | Phase::Landing(_)
        )
    }
}

impl Slot {
    /// Whether a handoff hands this slot's run over instead of waiting for
    /// it, and so does not step it while it drains: a rebuildable phase.
    /// A run awaiting the landing slot is one, held or not: the drain
    /// starts neither its landing nor its e2e (ADR-t3077-1), it keeps its
    /// lease, and the next process puts it back in the landing queue
    /// ([`takeover`]): an `awaiting_integration` run has its review
    /// adopted ([`Takeover::Review`]), a recorded pass going back to the
    /// landing queue after its `/exit` without a review, and a run a
    /// person's `land` queued goes back to the approved landings; an
    /// approved `needs_session` run queued after its resume waits for the
    /// slot again without a resume ([`Takeover::Land`]).
    pub(super) fn rebuildable(&self) -> bool {
        self.phase.rebuildable()
    }

    /// Whether a pass steps this slot, `handing_off` whether the
    /// supervisor drains for a handoff: one that does steps only what it
    /// waits for, so a run awaiting the landing slot starts no landing;
    /// any other pass, a stop's drain too, steps every slot.
    pub(super) fn stepped(&self, handing_off: bool) -> bool {
        !handing_off || !self.rebuildable()
    }

    /// What a pass does with this slot ([`Self::stepped`]), `landing_waited`
    /// whether a landing the handoff's drain waits for still runs: the
    /// drain observes a worker's receipt in its slot on each pass, and
    /// takes it on to validation only while that landing runs, so that the
    /// validation ends within the wait (ADR-t3077-1 decision 2). Once no
    /// landing runs, the next process validates it from the recorded
    /// `receipt_observed`, and the exec waits for no new validation.
    pub(super) fn pass(&self, handing_off: bool, landing_waited: bool) -> Pass {
        pass_of(
            self.stepped(handing_off),
            matches!(self.phase, Phase::Session(_)) && !self.out_of_slot(),
            landing_waited,
        )
    }

    /// Whether this slot's landing runs on: one that ended and is not
    /// joined yet runs no more.
    pub(super) fn landing_runs(&self) -> bool {
        matches!(&self.phase, Phase::Landing(Some(handle)) if !handle.is_finished())
    }
}

/// [`Slot::pass`] of a slot a pass steps when `stepped`, whose worker is
/// at work in its slot when `worker`, with `landing_waited` whether a
/// landing the drain waits for still runs.
fn pass_of(stepped: bool, worker: bool, landing_waited: bool) -> Pass {
    if stepped {
        Pass::Step
    } else if worker {
        Pass::Receipt {
            validate: landing_waited,
        }
    } else {
        Pass::Skip
    }
}

/// What a pass does with a slot ([`Slot::pass`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pass {
    /// Stepped as any pass steps it.
    Step,
    /// A handoff's drain observes its worker's receipt, and with
    /// `validate` takes a session idle after it on to validation.
    Receipt { validate: bool },
    /// Left as it is: handed over.
    Skip,
}

/// What the process that took a registration over after an exec does
/// with a run leased to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Takeover {
    /// Rebuilt from its `handoff.json`.
    Snapshot(handoff::SnapshotKind),
    /// A `needs_session` run without one whose resumed session lives on:
    /// its resume is rebuilt from its events, as an adopter does (task
    /// 640), and goes on watched.
    AdoptResume,
    /// A run before its review: rebuilt the way an adopted run's is
    /// (ADR-0039).
    Session,
    /// A run waiting for its review, which runs again.
    Review,
    /// An approved `needs_session` run queued to land after its resume
    /// resolved or was skipped ([`Queued::AfterResume`]): it waits for the
    /// landing slot again, its resume not run again.
    Land,
    /// Nothing to take over: its lease is given back, so the supervisor's
    /// resume or triage (or `recover`) picks it up, or, for a run queued to
    /// land, its start of the approved landings.
    GiveBack,
}

impl Takeover {
    /// Whether the run's resumed session goes on watched instead of being
    /// resumed again, recorded as `resume_adopted` (ADR-0047 decision 24).
    pub(super) const fn resume_adopted(self) -> bool {
        matches!(
            self,
            Self::Snapshot(handoff::SnapshotKind::Resume) | Self::AdoptResume
        )
    }
}

/// Whether the takeover of a run in `status` with `snapshot` reads whether
/// its resumed session lives: only a `needs_session` run without
/// `handoff.json` (which could not be written, or is gone) can go on.
pub(super) fn reads_resumed_session(
    status: RunStatus,
    snapshot: Option<handoff::SnapshotKind>,
) -> bool {
    snapshot.is_none() && status == RunStatus::NeedsSession
}

/// How the events of a run without `handoff.json` queue it to land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Queued {
    /// Not queued, or not read.
    No,
    /// A person's `land` or a recovered landing
    /// ([`crate::domain::RunHistory::queued_to_land`]).
    ToLand,
    /// Its resume resolved or was skipped and the approved run was queued
    /// ([`crate::domain::RunHistory::queued_after_resume`]).
    AfterResume,
}

/// [`Takeover`] of a run in `status` with `snapshot`, `resume_lives`
/// whether its resumed session lives on (read only when
/// [`reads_resumed_session`]) and `queued` how it was queued to land
/// (read only for an `awaiting_integration` or `needs_session` run
/// without `handoff.json`). An `awaiting_integration` run a person's
/// `land` or a recovered landing queued (one handed over while it awaited
/// the landing slot, ADR-t3077-1) gives its lease back to the approved
/// landings rather than having its review adopted, which would ask or
/// review it again over the answer. A `needs_session` run queued after
/// its resume waits for the landing slot again rather than giving its
/// lease back, for the resume would run again.
pub(super) fn takeover(
    status: RunStatus,
    snapshot: Option<handoff::SnapshotKind>,
    resume_lives: bool,
    queued: Queued,
) -> Takeover {
    match (status, snapshot) {
        (_, Some(kind)) => Takeover::Snapshot(kind),
        (RunStatus::NeedsSession, None) if queued == Queued::AfterResume => Takeover::Land,
        (RunStatus::NeedsSession, None) if resume_lives => Takeover::AdoptResume,
        (
            RunStatus::Claimed | RunStatus::Starting | RunStatus::Running | RunStatus::Validating,
            None,
        ) => Takeover::Session,
        (RunStatus::AwaitingIntegration, None) if queued != Queued::ToLand => Takeover::Review,
        _ => Takeover::GiveBack,
    }
}

/// `supervisor_handed_off`'s payload for a run in `status` that the
/// process `pid` of `token`, running `version`, took over from
/// `previous_version`, with the `state` its `handoff.json` held.
pub(super) fn handed_off(
    token: &crate::domain::LeaseToken,
    pid: u32,
    previous_version: Option<&str>,
    version: &str,
    status: RunStatus,
    state: Option<handoff::SnapshotKind>,
) -> Value {
    json!({
        "supervisor": token,
        "pid": pid,
        "previous_version": previous_version,
        "version": version,
        "status": status.as_str(),
        "state": state.map(handoff::SnapshotKind::as_str),
    })
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
        self.register_stopping_sessions();
        let mut kept = 0;
        for mut slot in self.claim.slots.take_all() {
            let run = slot.run.clone();
            // A live session's recovery job does not outlive this process;
            // its alert starts another once the run is rebuilt.
            stop_recovery(&mut slot);
            let snapshot = match &mut slot.phase {
                Phase::Review(watch) => {
                    watch.abandon();
                    info!(run_id = %run.id(), "run {}: review {} stopped for the handoff; it is reviewed again", run.id(), watch.attempt);
                    None
                }
                // A validating run is validated again by the next process,
                // which verifies and judges its claim again.
                Phase::SatisfiedReview(watch) => {
                    watch.abandon();
                    info!(run_id = %run.id(), "run {}: the judgment {} of its already_satisfied claim stopped for the handoff; it is verified and judged again", run.id(), watch.attempt);
                    None
                }
                Phase::ReviewPrograms(watch) => {
                    watch.job.abandon();
                    info!(run_id = %run.id(), "run {}: the program reviews before review {} stopped for the handoff; it is reviewed again", run.id(), watch.programs.attempt);
                    None
                }
                Phase::Recovery(watch) => {
                    watch.job.abandon();
                    info!(run_id = %run.id(), "run {}: recovery round {} stopped for the handoff; it is taken again", run.id(), watch.round);
                    if let Err(error) = self
                        .queue
                        .release_lease(run.id(), self.registration.token())
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
                            self.registration.token(),
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

    /// A handoff's drain's look at a worker's slot ([`Pass::Receipt`]):
    /// its receipt is observed, and with `validate` a session idle after
    /// it goes to validation as a pass's step takes it. A run whose task is
    /// being replanned, held or fenced is not validated: its park is the
    /// next process's, as the drain parks nothing it hands over.
    pub(super) fn observe_receipt_handing_off(
        &mut self,
        slot: &mut Slot,
        validate: bool,
    ) -> Result<Step> {
        if !self
            .queue
            .holds_lease(slot.run.id(), self.registration.token())?
        {
            return Ok(Step::Disowned);
        }
        let validate = validate && self.park_for(&slot.run)?.is_none();
        let Phase::Session(watch) = &mut slot.phase else {
            return Ok(Step::Continue);
        };
        match watch.poll_handing_off(self, &slot.run, validate)? {
            Some(run) => self.supervision_finished(slot, run),
            None => Ok(Step::Continue),
        }
    }

    /// A pass of a supervisor whose owner is gone (a test's process,
    /// [`LoopSettings::owner`]): nobody answers its runs' asks or reads
    /// their work now, so it waits for nothing that has not started. Every
    /// slot a handoff could rebuild (a run at work or waiting for a person,
    /// its review, resume, recovery or `/exit`) and a run waiting for the
    /// landing slot stop their jobs and leave the slots as for a handoff
    /// whose next process never comes: the leases stay and go stale with
    /// this process, so a later supervisor adopts the runs (a run without a
    /// lease is never adopted), and only a recovery's lease is given back
    /// for the next triage. A validation, a landing, an e2e and the
    /// verification of an `already_satisfied` claim in progress keep their
    /// slots until they end or the loop does
    /// ([`handoff::owner_gone_ends`]).
    pub(super) fn let_go_for_owner(&mut self) {
        for mut slot in self.claim.slots.take_all() {
            if !slot.rebuildable() {
                self.claim.slots.admit(slot);
                continue;
            }
            self.stop_slot_job(&mut slot);
            let run = slot.run.id().clone();
            if matches!(slot.phase, Phase::Recovery(_))
                && let Err(error) = self.queue.release_lease(&run, self.registration.token())
            {
                warn!(run_id = %run, error = %format_args!("{error:#}"), "run {run}: could not release the lease: {error:#}");
            }
            info!(run_id = %run, "run {run}: its supervisor's owner is gone; it is left for another supervisor");
        }
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
        for run in self.queue.runs_leased_by(self.registration.token())? {
            let snapshot = handoff::take_snapshot(&*self.files, self.registration.token(), &run);
            let kind = snapshot.as_ref().map(handoff::Snapshot::kind);
            self.queue.record_runtime_event(
                run.id(),
                EventKind::SupervisorHandedOff,
                handed_off(
                    self.registration.token(),
                    self.layout.pid,
                    previous_version,
                    &self.layout.version,
                    run.status(),
                    kind,
                ),
            )?;
            // A lookup that fails gives this run's lease back rather than
            // stopping the takeover of the others.
            let resume_lives = reads_resumed_session(run.status(), kind)
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
            // Likewise a lookup that fails adopts its review, as before, or
            // gives a `needs_session` run's lease back.
            let queued = if kind.is_none()
                && matches!(
                    run.status(),
                    RunStatus::AwaitingIntegration | RunStatus::NeedsSession
                ) {
                self.queue
                    .run_events(run.id())
                    .map(|events| {
                        let history = RunHistory::from_events(&events);
                        match run.status() {
                            RunStatus::NeedsSession if history.queued_after_resume().is_some() => {
                                Queued::AfterResume
                            }
                            RunStatus::AwaitingIntegration if history.queued_to_land().is_some() => {
                                Queued::ToLand
                            }
                            _ => Queued::No,
                        }
                    })
                    .unwrap_or_else(|error| {
                        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: whether it is queued to land could not be read: {error:#}", run.id());
                        Queued::No
                    })
            } else {
                Queued::No
            };
            let takeover = takeover(run.status(), kind, resume_lives, queued);
            let phase = match (takeover, snapshot) {
                (_, Some(snapshot)) => self.rebuild_from(&run, snapshot),
                (Takeover::AdoptResume, None) => self.adopt_resume(&run).map(Phase::Resume),
                (Takeover::Session, None) => self.resume(&run),
                (Takeover::Review, None) => self.adopt_review(&run),
                (Takeover::Land, None) => Ok(Phase::AwaitingSlot),
                (Takeover::Snapshot(_) | Takeover::GiveBack, None) => {
                    info!(run_id = %run.id(), "run {} ({}) has nothing to take over after the handoff; its lease is given back", run.id(), run.status().as_str());
                    self.queue
                        .release_lease(run.id(), self.registration.token())?;
                    continue;
                }
            };
            match phase {
                Ok(phase) => {
                    info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} taken over after the handoff ({})", run.id(), run.task_id(), run.status().as_str());
                    // A resumed session goes on watched instead of being
                    // resumed again (ADR-0047 decision 24).
                    if let (true, Phase::Resume(watch)) = (takeover.resume_adopted(), &phase) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::LeaseToken;
    use handoff::SnapshotKind;

    /// After the exec, a run with its `handoff.json` is rebuilt from it; a
    /// `needs_session` run without one goes on watching its resumed session
    /// only while that session lives; a run before its review is rebuilt as
    /// an adopted one, one waiting for its review is reviewed again, and
    /// any other (a run resting, one whose resumed session ended) gives its
    /// lease back. A resume rebuilt either way is `resume_adopted`.
    #[test]
    fn a_run_taken_over_after_a_handoff_is_rebuilt_or_gives_its_lease_back() {
        for status in [RunStatus::NeedsSession, RunStatus::Failed] {
            for kind in [SnapshotKind::Resume, SnapshotKind::Exit] {
                assert!(!reads_resumed_session(status, Some(kind)));
                assert_eq!(
                    takeover(status, Some(kind), false, Queued::No),
                    Takeover::Snapshot(kind)
                );
            }
        }
        assert!(reads_resumed_session(RunStatus::NeedsSession, None));
        assert!(!reads_resumed_session(RunStatus::Running, None));
        assert_eq!(
            takeover(RunStatus::NeedsSession, None, true, Queued::No),
            Takeover::AdoptResume
        );
        assert_eq!(
            takeover(RunStatus::NeedsSession, None, false, Queued::No),
            Takeover::GiveBack
        );
        for status in [
            RunStatus::Claimed,
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::Validating,
        ] {
            assert_eq!(takeover(status, None, false, Queued::No), Takeover::Session);
        }
        assert_eq!(
            takeover(RunStatus::AwaitingIntegration, None, false, Queued::No),
            Takeover::Review
        );
        // A person's `land` queued it: the approved landings take it.
        assert_eq!(
            takeover(RunStatus::AwaitingIntegration, None, false, Queued::ToLand),
            Takeover::GiveBack
        );
        for status in [
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Interrupted,
            RunStatus::Integrated,
        ] {
            assert_eq!(
                takeover(status, None, false, Queued::No),
                Takeover::GiveBack
            );
        }
        assert!(Takeover::AdoptResume.resume_adopted());
        assert!(Takeover::Snapshot(SnapshotKind::Resume).resume_adopted());
        for other in [
            Takeover::Snapshot(SnapshotKind::Exit),
            Takeover::Session,
            Takeover::Review,
            Takeover::Land,
            Takeover::GiveBack,
        ] {
            assert!(!other.resume_adopted());
        }
    }

    /// A run awaiting the landing slot is handed over leased, its landing
    /// held or not, and a handoff's drain does not step it (a stop's does);
    /// a landing in progress is waited for, and an e2e waiting to start is
    /// handed over. The next process puts an `awaiting_integration` run
    /// back through its adopted review, and an approved `needs_session` one
    /// queued after its resume back in the landing slot, its resume not run
    /// again, whether its resumed session's wrapper looks alive or not.
    #[test]
    fn a_handoff_hands_over_a_run_awaiting_the_landing_slot() {
        let run = |status| super::super::recovery::test_run(status, None);
        let awaiting = Slot::new(run(RunStatus::AwaitingIntegration), Phase::AwaitingSlot);
        assert!(awaiting.rebuildable());
        assert!(!awaiting.stepped(true));
        assert!(awaiting.stepped(false));
        assert_eq!(
            takeover(awaiting.run.status(), None, false, Queued::No),
            Takeover::Review
        );
        let resolved = Slot::new(run(RunStatus::NeedsSession), Phase::AwaitingSlot);
        assert!(resolved.rebuildable());
        assert!(!resolved.stepped(true));
        for resume_lives in [false, true] {
            assert_eq!(
                takeover(
                    resolved.run.status(),
                    None,
                    resume_lives,
                    Queued::AfterResume
                ),
                Takeover::Land
            );
        }
        let landing = Slot::new(run(RunStatus::Integrating), Phase::Landing(None));
        assert!(!landing.rebuildable());
        assert!(landing.stepped(true));
        assert!(landing.stepped(false));
        let e2e_waits = Slot::new(run(RunStatus::AwaitingIntegration), Phase::AwaitingE2e);
        assert!(e2e_waits.rebuildable());
        assert!(!e2e_waits.stepped(true));
    }

    /// ADR-t3077-1 decision 2: a handoff's drain observes the receipt of a
    /// worker in its slot, and takes it on to validation only while a
    /// landing it waits for runs; a worker waiting for a person outside
    /// the slots and the other rebuildable phases are left as they are,
    /// and any other pass, a stop's drain too, steps every slot. Whether
    /// a landing's thread still runs is the integration tests' boundary
    /// (`runtime_handoff`).
    #[test]
    fn a_handoff_observes_a_workers_receipt_and_validates_it_only_while_a_landing_runs() {
        for landing_waited in [false, true] {
            assert_eq!(
                pass_of(false, true, landing_waited),
                Pass::Receipt {
                    validate: landing_waited
                }
            );
            // A worker out of the slots, or a run handed over.
            assert_eq!(pass_of(false, false, landing_waited), Pass::Skip);
            for worker in [false, true] {
                assert_eq!(pass_of(true, worker, landing_waited), Pass::Step);
            }
        }
        let run = |status| super::super::recovery::test_run(status, None);
        let awaiting = Slot::new(run(RunStatus::AwaitingIntegration), Phase::AwaitingSlot);
        for landing_waited in [false, true] {
            assert_eq!(awaiting.pass(true, landing_waited), Pass::Skip);
            assert_eq!(awaiting.pass(false, landing_waited), Pass::Step);
        }
        assert!(!awaiting.landing_runs());
        // A landing is waited for, and stepped; one joined runs no more.
        let landing = Slot::new(run(RunStatus::Integrating), Phase::Landing(None));
        assert_eq!(landing.pass(true, true), Pass::Step);
        assert!(!landing.landing_runs());
    }

    /// `supervisor_handed_off` names the run's status and the state its
    /// `handoff.json` held, `null` without one.
    #[test]
    fn the_handed_off_event_names_the_status_and_the_state_taken_over() {
        let token = LeaseToken::new("t");
        assert_eq!(
            handed_off(
                &token,
                7,
                Some("0.1.0"),
                "0.2.0",
                RunStatus::NeedsSession,
                None
            ),
            json!({"supervisor": "t", "pid": 7, "previous_version": "0.1.0", "version": "0.2.0",
                   "status": "needs_session", "state": null})
        );
        let resume = handed_off(
            &token,
            7,
            None,
            "0.2.0",
            RunStatus::NeedsSession,
            Some(SnapshotKind::Resume),
        );
        assert_eq!(resume["state"], "resume");
        assert_eq!(resume["previous_version"], Value::Null);
        let exit = handed_off(
            &token,
            7,
            None,
            "0.2.0",
            RunStatus::Failed,
            Some(SnapshotKind::Exit),
        );
        assert_eq!(
            (&exit["status"], &exit["state"]),
            (&json!("failed"), &json!("exit"))
        );
    }
}
