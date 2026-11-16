//! The supervisor's share of the audit (category G of ADR-t4840-1): at
//! every pass it reads the settings no command changes
//! ([`crate::application::Verifier::settings_snapshot`]) and sends them to
//! the queue service, which records them `observed` when they differ from
//! the last record of them, whichever supervisor wrote it
//! ([`crate::application::EventStore::observe_settings`]): a supervisor
//! keeps no copy of what it sent, which another's record would make stale;
//! and every
//! [`CHAIN_VERIFY_PERIOD`] it has the service verify the audit's chain,
//! which opens the inbox's attention of a mismatch
//! ([`crate::application::EventStore::check_audit_chain`]). A failure is
//! warned of and tried again at a later pass.

use super::*;
use crate::domain::audit::administration::verify_due;

#[cfg(doc)]
use crate::domain::audit::administration::CHAIN_VERIFY_PERIOD;

/// What the supervisor knows of its audit's passes.
#[derive(Debug, Default)]
pub(in crate::application::supervise) struct AuditWatch {
    /// When the last check of the chain started, as the wall clock's time
    /// since the epoch.
    checked: Option<Duration>,
    /// One verification off the pass; it owns its own service connection.
    job: Option<std::thread::JoinHandle<anyhow::Result<Value>>>,
}

impl AuditWatch {
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }

    fn reap(&mut self, job: std::thread::JoinHandle<anyhow::Result<Value>>) {
        match job.join() {
            Ok(Ok(found)) if found["audit_verified"] == false => warn!(
                "the audit's chain does not verify at seq {} ({}): a change of the queue's DB made around the CLI",
                found["seq"], found["kind"]
            ),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                self.checked = None;
                warn!(error = %format_args!("{error:#}"), "the audit's chain could not be checked: {error:#}");
            }
            Err(_) => {
                self.checked = None;
                warn!("the audit's verification panicked");
            }
        }
    }

    pub(super) fn finish(
        &mut self,
        deadline: &super::EndBy,
        clock: &dyn crate::application::Clock,
    ) {
        if let Some(job) = self.job.take() {
            if super::finished_by(&job, deadline, clock) {
                self.reap(job);
            } else {
                warn!("the audit's verification still runs as the supervisor ends");
            }
        }
    }
}

impl HostOpsState {
    /// The audit's pass ([`AuditWatch`]).
    pub(in crate::application::supervise) fn audit_pass(
        &mut self,
        pass: &mut HostEnv<'_>,
        start: bool,
    ) {
        if let Some(job) = self.audit.job.take_if(|job| job.is_finished()) {
            self.audit.reap(job);
        }
        match pass.verifier.settings_snapshot() {
            Ok(Some(read)) => match pass.queue.observe_settings(&read) {
                Ok(true) => {
                    info!("the settings changed since the audit's last record of them: recorded")
                }
                Ok(false) => {}
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the settings could not be sent to the audit: {error:#}")
                }
            },
            Ok(_) => {}
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the settings could not be read for the audit: {error:#}")
            }
        }
        let now = pass
            .generators
            .clock
            .system_time()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        if self.audit.running() || !start || !verify_due(self.audit.checked, now) {
            return;
        }
        self.audit.checked = Some(now);
        let queues = self.queues.clone();
        self.audit.job = Some(spawn_traced(move || queues.open()?.check_audit_chain()));
    }
}
