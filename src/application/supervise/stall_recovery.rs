//! Turn stalls are nudged, recovered or escalated; a resume repair parks the run.

use super::*;
use crate::domain::recovery::HEADLESS_STALLED_ACTIONS;

impl SessionWatch {
    pub(super) fn at_prompt(&self, sv: &Supervisor<'_>, run: &TaskRun) -> bool {
        sv.queue
            .run_events(run.id())
            .is_ok_and(|events| between_turns(sv, &self.idle_marker, &events))
    }

    /// One look at the first session's idle without a receipt: the nudge,
    /// then its recovery job and the `stalled` ask ([`StallWatch::poll`]).
    /// A job whose detection ended (the session moved on) is stopped.
    /// Returns what was typed into the session, for the check that it was
    /// taken.
    pub(super) fn watch_stall(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<SystemTime>> {
        let live = Live {
            workspace: &self.workspace,
            run_dir: &self.run_dir,
            // A headless session has no dialog to answer (ADR-t813-1).
            allowed: &HEADLESS_STALLED_ACTIONS,

            at_prompt: self.at_prompt(sv, run),

            park: self.input_at.is_none(),
        };
        let start = self.stall.poll(
            sv,
            run,
            &self.workspace,
            &self.idle_marker,
            StallRecovery {
                recovery: &mut self.recovery,
                live: &live,
            },
        )?;
        let followed = self.stall.followed();
        self.stop_idle_job(sv, run, followed);
        Ok(start)
    }

    /// Stop the recovery job of the idle once its detection ended (the
    /// session wrote its receipt, asked a question or moved on), or when the
    /// last observation did not follow it (`followed` false: a dialog, a
    /// question, a hold or an input came first), which leaves nobody to act
    /// on its verdict while it holds the one job of the session.
    pub(super) fn stop_idle_job(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun, followed: bool) {
        let outcome = if !self.stall.recovering() {
            "session_moved"
        } else if !followed {
            "alert_cleared"
        } else {
            return;
        };
        for reason in IDLE_REASONS {
            self.recovery
                .stop_reason(sv, run, RecoveryAlert::Stalled, reason, outcome);
        }
    }

    /// Park the run for a session of its own, as a recovery job's `resume`
    /// asked (ADR-0047 decision 40, task 442): the stalled detections end
    /// with the repair, their asks close, the jobs stop, and the run
    /// becomes `needs_session` with the instruction, which the resume's
    /// request carries (`recovery_parked`). The phase then asks the session
    /// to exit and lets the lease go; the supervisor resumes the run.
    pub(super) fn park_for_resume(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        instruction: &str,
    ) -> Result<TaskRun> {
        self.stall.parked(sv, run)?;
        self.stall.ended(sv, run)?;
        self.recovery.stop(sv, run);
        let reason = format!(
            "the supervisor's recovery job sent the stalled session back to a session of its own: {instruction}"
        );
        let parked = sv.queue.park_live(
            run.id(),
            &sv.token,
            &reason,
            json!({
                "instruction": instruction,
                "alert": RecoveryAlert::Stalled,
                "workspace_id": self.workspace,
            }),
        )?;
        info!(run_id = %run.id(), "run {} is parked for a resume by its recovery job; its session is asked to exit", run.id());
        Ok(parked)
    }
}
