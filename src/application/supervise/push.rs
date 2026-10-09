//! The push of the KPIs (ADR-0051 decision 23): the messages the report
//! job made go to the host's push command one at a time on a job thread,
//! each bounded by the command's timeout. A failed message is tried again
//! twice, after the `push_retry` delays, then given up (recorded, and an
//! attention for the inbox). Nothing here stops a report, a claim nor a
//! landing. The messages wait in this process only: a supervisor that
//! stops or execs drops those not sent yet (it waits for the one being
//! sent).

use std::collections::VecDeque;

use super::*;
use crate::application::push::{self, PushOutcome};
use crate::domain::kpi::push::{PushConfig, PushMessage};

/// A message waiting for its `attempt`-th run of the command.
pub(super) struct Pending {
    config: PushConfig,
    message: PushMessage,
    attempt: usize,
    due: Instant,
}

/// The messages waiting and the one being sent.
#[derive(Default)]
pub(super) struct PushWatch {
    pending: VecDeque<Pending>,
    job: Option<(Pending, thread::JoinHandle<PushOutcome>)>,
}

impl PushWatch {
    /// A message is being sent.
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }

    /// A message is being sent or waits to be.
    pub(super) fn busy(&self) -> bool {
        self.job.is_some() || !self.pending.is_empty()
    }
}

impl ObservationState {
    /// Queue `messages` for the command of `config`, each for its first
    /// attempt now.
    pub(super) fn queue_pushes(&mut self, config: PushConfig, messages: Vec<PushMessage>) {
        let now = Instant::now();
        self.push
            .pending
            .extend(messages.into_iter().map(|message| Pending {
                config: config.clone(),
                message,
                attempt: 1,
                due: now,
            }));
    }

    /// Record the message sent once its command ended; start the next due
    /// message when `start`.
    pub(super) fn push_pass(&mut self, env: &mut ObservationEnv<'_>, start: bool) {
        let Some(port) = self.reports.clone() else {
            return;
        };
        if let Some((pending, job)) = self.push.job.take() {
            if !job.is_finished() {
                self.push.job = Some((pending, job));
                return;
            }
            let outcome = job
                .join()
                .unwrap_or_else(|_| PushOutcome::error("the push job panicked"));
            let kind = pending.message.kind.as_str();
            if outcome.success {
                info!(
                    "KPI push of the {kind} message of {} sent",
                    pending.message.period
                );
            } else {
                warn!(
                    "KPI push of the {kind} message of {} failed (attempt {}, exit code {:?}, timed out {})",
                    pending.message.period, pending.attempt, outcome.exit_code, outcome.timed_out
                );
            }
            let again = push::record_attempt(
                &*env.queue,
                &pending.config,
                &pending.message,
                pending.attempt,
                &outcome,
            )
            .unwrap_or_else(|error| {
                warn!(error = %format_args!("{error:#}"), "the KPI push could not be recorded: {error:#}");
                // Unrecorded, a failed message is still tried again.
                !outcome.success
                    && crate::domain::kpi::push::retry_after(pending.attempt).is_some()
            });
            if again {
                let delay = port.push_retry[(pending.attempt - 1).min(1)];
                self.push.pending.push_back(Pending {
                    attempt: pending.attempt + 1,
                    due: Instant::now() + delay,
                    ..pending
                });
            }
        }
        if !start {
            return;
        }
        let now = Instant::now();
        let Some(index) = self.push.pending.iter().position(|p| p.due <= now) else {
            return;
        };
        let Some(pending) = self.push.pending.remove(index) else {
            return;
        };
        let request = push::request(&pending.config, &port.push_target, &pending.message);
        let run = port.run_push;
        let job = spawn_traced(move || run(&request));
        self.push.job = Some((pending, job));
    }
}
