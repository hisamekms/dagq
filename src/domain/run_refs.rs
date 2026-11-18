//! Safe retention decisions for remote run refs. A successful worker receipt
//! is not a landing: only Integrated proves main contains the accepted head.
use super::RunStatus;

/// Retention facts from the queue, never from a runner's terminal report.
pub struct End {
    pub run: String,
    pub status: RunStatus,
    pub task_canceled: bool,
    pub generation: i64,
}

/// Whether all generations and landing candidates may be removed. Failure,
/// replacement and session interruption alone never release accepted history.
pub fn may_cleanup(status: RunStatus, task_canceled: bool) -> bool {
    status == RunStatus::Integrated || task_canceled
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_landing_or_cancellation_releases_history() {
        for status in [
            RunStatus::Claimed,
            RunStatus::Starting,
            RunStatus::Validating,
            RunStatus::Running,
            RunStatus::Succeeded,
            RunStatus::NeedsSession,
            RunStatus::Failed,
            RunStatus::Interrupted,
            RunStatus::Replaced,
            RunStatus::AwaitingIntegration,
            RunStatus::Integrating,
        ] {
            assert!(!may_cleanup(status, false), "{status:?}");
            assert!(may_cleanup(status, true));
        }
        assert!(may_cleanup(RunStatus::Integrated, false));
    }
}
