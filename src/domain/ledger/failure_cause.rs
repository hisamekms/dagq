//! What a run's row folds of its recovery jobs' judgement of what caused
//! its failure (ADR-t4163-1 decisions 1, 2 and 4). A value beside the
//! intervals, carried from the jobs' `recovery_finished` and never derived
//! from anything else.

use serde_json::Value;

use crate::domain::recovery::FailureCause;
use crate::domain::run::{RecoveryRecord, restore_payload};
use crate::domain::{EventKind, RunEvent};

/// The key of a run row's detail that holds its [`FailureCause`] as it is
/// spelled, for a run folded into intervals and a legacy one alike. A row
/// without it (one an older fold wrote) reads as unknown.
pub const DETAIL_KEY: &str = "failure_cause";

/// The cause of a run from its `events`, in any order: of each
/// `recovery_finished`, [`FailureCause::read`] of its `failure_cause`; a run
/// with none is unknown.
pub fn of(events: &[RunEvent]) -> FailureCause {
    of_rounds(
        events
            .iter()
            .filter(|event| event.kind == EventKind::RecoveryFinished)
            .map(|event| {
                FailureCause::read(restore_payload::<RecoveryRecord>(&event.payload).failure_cause)
            }),
    )
}

/// The cause of a run whose rounds judged `rounds`, in any order: the
/// environment when any round says so, since a later round judges its own
/// failure and takes nothing back, and a run that lands later has used what
/// the refusal cost all the same (decision 2); else the task when any round
/// says so, since a round that names a cause knows more than one that does
/// not; else unknown.
pub fn of_rounds(rounds: impl IntoIterator<Item = FailureCause>) -> FailureCause {
    rounds.into_iter().max().unwrap_or_default()
}

/// What a row's `detail` holds of it.
pub fn of_detail(detail: &Value) -> FailureCause {
    FailureCause::read(detail.get(DETAIL_KEY).and_then(Value::as_str))
}
