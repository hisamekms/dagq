//! The holds of tasks in the fill pass (ADR-t1879-1): a held task leaves
//! the one line of resumes, recovery jobs and claims (ADR-t1850-1) before
//! it is ordered, recorded once per hold and step as `hold_deferred`; the
//! start of each (the claim's transaction, `begin_resume`, `skip_resume`,
//! `begin_triage`, `exhaust_resumes`) reads the hold again in its own
//! transaction, so a hold that comes after the line was made still keeps
//! the task. A recovery job's verdict and a person's answer that reach a
//! held task are kept instead of applied (`finish_triage`,
//! [`RunRecovery::keep_triage_answer`], [`RunRecovery::keep_live_verdict`]);
//! once the hold is released, the first pass applies or discards each
//! here, before the answers and the recovery jobs of the pass.

use super::*;
use crate::application::KeptOutcome;
use crate::domain::task_replan::GuardedStep;

impl Supervisor<'_> {
    /// Whether `task`'s open hold keeps `step` (of its run `run`) out of
    /// the pass, recorded once per hold and step.
    pub(super) fn held_out(
        &mut self,
        task: TaskId,
        run: Option<&RunId>,
        step: GuardedStep,
    ) -> Result<bool> {
        let Some(hold) = self.queue.hold_guard(task)? else {
            return Ok(false);
        };
        if self
            .queue
            .note_hold_deferred(hold, run, step, self.registration.token())?
        {
            info!(task_id = %task, hold_id = %hold.hold, "task {task} is held (hold {}): no {} until it is released", hold.hold, step.as_str());
        }
        Ok(true)
    }

    /// Record the claims the pass leaves out for their task's hold.
    pub(super) fn note_held_claims(&mut self) -> Result<()> {
        for hold in self.queue.held_claims()? {
            if self.queue.note_hold_deferred(
                hold,
                None,
                GuardedStep::Claim,
                self.registration.token(),
            )? {
                info!(task_id = %hold.task, hold_id = %hold.hold, "task {} is held (hold {}): not claimed until it is released", hold.task, hold.hold);
            }
        }
        Ok(())
    }

    /// Apply or discard each result a hold kept whose hold is released.
    pub(super) fn settle_kept_results(&mut self) -> Result<()> {
        for kept in self.queue.released_kept_results()? {
            match self.queue.settle_kept_result(kept) {
                Ok(KeptOutcome::Applied(run)) => {
                    info!(run_id = %run.id(), task_id = %run.task_id(), "run {}: the result kept while task {} was held (event {kept}) is applied; the run is {}", run.id(), run.task_id(), run.status().as_str());
                    self.request_cleanup(Some(run.task_id()));
                }
                Ok(KeptOutcome::Discarded(reason)) => {
                    info!(
                        "the result kept in event {kept} is discarded: {}",
                        reason.as_str()
                    );
                }
                Ok(KeptOutcome::Untouched) => {}
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the result kept in event {kept} could not be settled: {error:#}");
                }
            }
        }
        Ok(())
    }
}
