//! The raises of worker sessions after failures the task caused (ADR-0079
//! decision 5), over the runs of the page: how many resumes and revises
//! raised a session, why and by which step, and whether each raised
//! resume resolved the run in that one attempt.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{
    RunStats,
    retries::{ResumeSummary, resume_summary},
};

/// The raises of a set of runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Escalations {
    /// Raised resumes and switched revises.
    pub count: usize,
    /// Per reason: the code that parked the run (`verification_failed`,
    /// `sent_back`) or `revise`.
    pub by_reason: BTreeMap<String, usize>,
    /// Per step, `<model/effort> -> <model/effort>`.
    pub by_step: BTreeMap<String, usize>,
    /// The raised resumes, and how many resolved the run in one attempt.
    pub resumes: ResumeSummary,
    /// The revises that switched the live session a step up.
    pub revises: usize,
    /// The revises that could not switch it and went on as it was.
    pub revises_not_switched: usize,
}

/// The raises of `runs`.
pub fn escalations(runs: &[&RunStats]) -> Escalations {
    let mut by_reason: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_step: BTreeMap<String, usize> = BTreeMap::new();
    let mut step = |reason: &str, from: &str, to: &str| {
        *by_reason.entry(reason.to_owned()).or_default() += 1;
        *by_step.entry(format!("{from} -> {to}")).or_default() += 1;
    };
    let mut raised = Vec::new();
    let (mut revises, mut revises_not_switched) = (0, 0);
    for run in runs {
        for attempt in &run.retries.resume_attempts {
            if let (Some(from), Some(to)) = (&attempt.escalated_from, &attempt.escalated_to) {
                step(&attempt.reason, from, to);
                raised.push(attempt);
            }
        }
        for revise in &run.retries.revise_escalations {
            if revise.switched {
                step(
                    crate::domain::worker_model::REVISE,
                    &revise.from,
                    &revise.to,
                );
                revises += 1;
            } else {
                revises_not_switched += 1;
            }
        }
    }
    Escalations {
        count: raised.len() + revises,
        by_reason,
        by_step,
        resumes: resume_summary(&raised),
        revises,
        revises_not_switched,
    }
}
