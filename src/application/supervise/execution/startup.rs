//! Failure policy between registration and the first pass. Registration
//! flags (mode, handoff acceptance and auto-update) retain startup's Err
//! behaviour; only max-load display and start diagnostics may be abandoned.
//! Saved telemetry revocations are taken once by the entry point, outside
//! the handoff attempts whose queue reads and reconstruction may retry.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    Registration,
    Diagnostics,
    Handoff,
}

/// Registration is mandatory even for a request known not to have run.
/// After registration and its required flags, diagnostics (max load and
/// start events, including the registration read for their
/// payload) are abandoned on outage, with the remaining sequence: an
/// uncertain write must not be replayed. Holds are read afresh every pass.
/// Handoff retries only conditional review interruptions and run records
/// checked against history; leases missing from slots are reconciled.
/// Both not_done and maybe_done outages use this policy; all other errors
/// retain their original propagation.
fn continues(stage: Stage, error: &anyhow::Error) -> bool {
    stage != Stage::Registration && crate::application::supervisor_service::unreachable(error)
}

pub(super) fn required<T>(result: Result<T>) -> Result<T> {
    // Registration never yields None.
    Ok(finish(Stage::Registration, result)?.expect("registration is required"))
}

pub(super) fn finish<T>(stage: Stage, result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if continues(stage, &error) => {
            warn!(?stage, error = %format_args!("{error:#}"),
                "the queue service did not answer startup; remaining diagnostics are abandoned, holds are read again and handoff is reconciled in the first pass");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

impl Supervisor<'_> {
    /// Continue takeover after `supervise` resumed the registration with
    /// its version and heartbeat updated, keeping its token, pid, leases
    /// and start time. Read the hold without applying its answers, then
    /// interrupt unfinished plan/goal
    /// reviews and rebuild the slots of this registration's leased runs.
    /// Answers wait until those slots exist. No new work precedes this
    /// reconstruction: `supervise` tries it at startup, and an unanswered
    /// request leaves the remainder for the first pass's reconciliation.
    /// The entry caller records a warning and the unreachable marker; a
    /// further outage is handled by `run_loop` before another pass.
    /// Review interruptions only close unfinished rows; handed-off records
    /// are compared with history before retrying an uncertain write.
    pub(super) fn finish_startup_handoff(&mut self) -> Result<()> {
        if let Some(previous) = self.startup_handoff.clone() {
            self.check_queue_hold(false)?;
            self.rebuild_own_runs(previous.as_deref())?;
            self.startup_handoff = None;
            self.startup_handoff_records.clear();
        }
        Ok(())
    }

    pub(super) fn startup_unanswered(&mut self, result: Result<()>) -> Result<()> {
        if finish(Stage::Handoff, result)?.is_none() {
            self.unanswered.lost();
            self.host.queue_service_unreached();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::supervisor_service::ServiceUnreachable;

    #[test]
    fn startup_policy_preserves_registration_and_non_outage_errors() {
        for (stage, may_continue) in [
            (Stage::Registration, false),
            (Stage::Diagnostics, true),
            (Stage::Handoff, true),
        ] {
            // Read/unsent write and a write whose answer was EOF.
            for error in [
                anyhow::Error::new(ServiceUnreachable::not_done("read or unsent write")),
                anyhow::Error::new(ServiceUnreachable::maybe_done("EOF after write")),
            ] {
                assert_eq!(continues(stage, &error), may_continue, "{stage:?}");
                let result = finish::<()>(stage, Err(error));
                assert_eq!(result.is_ok(), may_continue, "{stage:?}");
                if may_continue {
                    assert!(result.unwrap().is_none());
                }
            }
            assert!(!continues(stage, &anyhow::anyhow!("refused")));
            assert!(finish::<()>(stage, Err(anyhow::anyhow!("refused"))).is_err());
        }
    }
}
