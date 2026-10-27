//! The runs left awaiting integration with no lease and no open ask,
//! though their last review lets them land (ADR-t3049-1): a supervisor
//! that stopped gave the lease back as the run waited for its landing.
//! The fill pass takes each up again, as [`crate::domain::stranded_landing`]
//! judges with this supervisor's build, and puts it back in its landing's
//! line without a second review.

use super::*;
use crate::domain::stranded_landing::{self, Seen, Stranded};

impl Supervisor<'_> {
    /// Lease each run [`stranded_landing::judge`] takes up, while slots are
    /// free and this supervisor does not drain (one that stops would give
    /// it back at once), and go on with it as an adopter goes on with a
    /// passed review ([`Self::adopt_review`]): prechecked, its session
    /// asked to exit if it was not, then waiting in its slot for its
    /// landing, never reviewed again. The lease's transaction judges the
    /// run again, its events as they stand then included, so two
    /// supervisors take a run once and none takes one another's landing
    /// held for a person since this look.
    pub(super) fn take_up_stranded_landings(&mut self, parallel: usize) -> Result<()> {
        if self.registration.draining() {
            return Ok(());
        }
        for run in self
            .queue
            .runs_with_status(RunStatus::AwaitingIntegration)?
        {
            if self.used_slots() >= parallel {
                break;
            }
            if self.in_slot(run.id()) {
                continue;
            }
            let leased = self.queue.run_lease(run.id())?.is_some();
            if leased {
                continue;
            }
            let events = self.queue.run_events(run.id())?;
            let build = Some(self.layout.version.as_str());
            if stranded_landing::kept(&events, build).is_some() {
                continue;
            }
            let seen = Seen {
                status: run.status(),
                leased,
                ask_open: !self.queue.unclosed_run_asks(run.id())?.is_empty(),
                fenced: self.queue.turn_fence(run.task_id())?.is_some(),
                task: self.queue.show(run.task_id())?.task.status(),
            };
            if stranded_landing::judge(seen, &events, build) != Stranded::TakeUp {
                continue;
            }
            // Not while the cleanup job clears the run's build outputs.
            let cleaning = self.host.cleanup().cleaning();
            let mut guard = cleanup::lock_cleaning(&cleaning);
            if !guard.may_lease(run.id()) {
                self.host.defer_cleanup();
                continue;
            }
            let taken = self
                .queue
                .lease_stranded_landing(run.id(), self.registration.token())?;
            drop(guard);
            let Some(run) = taken else {
                info!(run_id = %run.id(), "run {} was not taken up: it changed or another supervisor took it", run.id());
                continue;
            };
            info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} is taken up again: it was left awaiting integration with no lease and no ask after its review passed", run.id(), run.task_id());
            self.issue_run_telemetry(&run);
            match self.adopt_review(&run) {
                Ok(phase) => self.claim.slots.admit_noting(
                    Slot::new(run, phase),
                    STRANDED_LANDING,
                    &*self.queue,
                ),
                // Given up with its lease released, the run is a person's:
                // the error is the inbox's attention, and no later pass takes
                // it up to fail again ([`stranded_landing::Keep::GivenUp`]).
                Err(error) => {
                    let message = format!("run {} could not be taken up: {error:#}", run.id());
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{}", message);
                    self.abandon(&run, message, &reason_of_error(&error, ReasonCode::Other));
                }
            }
        }
        Ok(())
    }
}

/// The cause of the phase a run taken up enters, and the `reason` of its
/// `lease_acquired`.
pub(super) const STRANDED_LANDING: &str = "stranded_landing";
