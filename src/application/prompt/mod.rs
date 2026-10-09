//! The worker's prompt (`prompt.txt`): the task, its goal, the summaries
//! of its landed predecessors and of the goals it waited for, the tasks running alongside it and what the
//! receipt must hold. Built from what the queue returned at claim time.
//! Also the initial prompts of the inbox session `up` opens and of the
//! planner sessions a person or the runtime opens, and what the supervisor asks of an agent: the headless review and
//! triage, and the requests it types into a live session (a resume, a
//! revise, a receipt that does not match).

use crate::domain::PlannerHandover;
use crate::domain::event_kind;
use crate::domain::follow_up::FOLLOW_UP_ASK_DEPTH;
use crate::domain::headless_job::JobAccess;
use crate::domain::resume::ResumeConfig;
use crate::domain::review_reason;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::{
    RunFiles, TaskListItem, fenced,
    integrate::{integrate_logs, log_names},
    or_none,
    prompt_fit::{self, Fit, Keep, NOT_READABLE, left_out_note, shrink},
    tail,
};
use crate::domain::{
    Ask, BundleKey, CommitSha, DraftOrigin, DraftTarget, EvidenceCheck, FindingView, Goal, GoalId,
    GoalPredecessor, GoalTask, LintViolation, MAX_DRAFT_PLANNERS, MAX_FINDING_PLANNERS,
    MAX_PLAN_REVISES, MAX_RESUME_ATTEMPTS, MAX_REVISE_ATTEMPTS, Predecessor, Proposal, ProposalId,
    Provider, Receipt, RunEvent, RunId, RunStatus, TRIAGE_RETRY_FAILURES, Task, TaskDetail, TaskId,
    TaskRun,
    recovery::{ProcessInfo, RecoveryAlert},
    related::RelatedTask,
    required_of, resume,
    search::SearchHit,
    stats::conflicts::ConflictHotspot,
};

mod plan_review;
mod planners;
mod requests;
mod run_review;
mod worker;

pub use plan_review::*;
pub use planners::*;
pub use requests::*;
pub use run_review::*;
pub use worker::*;

/// What the plan review prompt takes, in bytes, section by section, and
/// what its limits left out (task 1561, ADR-t1566-1 decision 6): recorded
/// as `prompt_bytes` on `plan_review_finished` and `plan_review_failed`.
/// The sections add up to `total`; `omitted` counts, by section, the items
/// left out or replaced by how to read them; `over_limit` says why the
/// required sections were cut, when they were.
///
/// In the sections of a list (chosen by `Fit::lines`, and the planners'
/// `drafts`, `asks`, `goals`, `reasons`, `refs` and `handover`, whose
/// notes and draft lines are its items) and the draft
/// planner's `origin` and `revisit`, `omitted` counts each item once,
/// chosen after its parts are cut: left out, or kept with any part cut. A
/// goal of the planners' `goals` is one item with its task lines; `origin`
/// and `revisit` are each one item, whichever of their parts or the whole
/// was cut. The revise planner's `answer` is a list of the answers it
/// carries, each counted once. The recovery job's `task`, the finding
/// planner's `finding` and the other planners' `answer` are each one item,
/// counted once whichever of its fields was cut. The worker's `task`,
/// `goal` and `inherited` (ADR-t2072-1), whose prompt's bytes are recorded
/// on `wrapper_launched`, still count each cut field of their one item.
/// A next-turn message's (ADR-t2072-1) are recorded on the
/// `turn_requested` that wrote it, and each of its texts is one item.
/// The throughput review's `omitted` is keyed not by its sections but by
/// the names of the parts its `DROP_ORDER` left out, each counted once
/// ([`ReviewPrompt::record`](crate::application::throughput_review::ReviewPrompt::record)).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PromptBytes {
    pub total: usize,
    pub limit: usize,
    pub sections: BTreeMap<&'static str, usize>,
    pub omitted: BTreeMap<&'static str, usize>,
    pub over_limit: Option<String>,
}

/// Read back from what a process wrote for the next one (a resume's
/// `handoff.json`), so that a request sent after a handoff records what it
/// took when it was built.
impl<'de> serde::Deserialize<'de> for PromptBytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Written {
            total: usize,
            limit: usize,
            #[serde(default)]
            sections: BTreeMap<String, usize>,
            #[serde(default)]
            omitted: BTreeMap<String, usize>,
            #[serde(default)]
            over_limit: Option<String>,
        }
        let written = Written::deserialize(deserializer)?;
        let names = |map: BTreeMap<String, usize>| {
            map.into_iter()
                .map(|(name, bytes)| (section_name(name), bytes))
                .collect()
        };
        Ok(Self {
            total: written.total,
            limit: written.limit,
            sections: names(written.sections),
            omitted: names(written.omitted),
            over_limit: written.over_limit,
        })
    }
}

/// A section's name read back as the `&'static str` [`PromptBytes`] keys
/// by: the runtime's own few names, each kept once for the process.
fn section_name(name: String) -> &'static str {
    static NAMES: std::sync::Mutex<BTreeSet<&'static str>> = std::sync::Mutex::new(BTreeSet::new());
    let mut names = NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(known) = names.get(name.as_str()) {
        return known;
    }
    let kept: &'static str = Box::leak(name.into_boxed_str());
    names.insert(kept);
    kept
}

/// The prompt of a headless job or of a planner of the runtime's held to
/// its limits, and what it takes (task 1571, ADR-t1566-1 decisions 4 to
/// 6): goal review, run review, recovery job and the four planners', and
/// the worker's `prompt.txt` and its next-turn messages (ADR-t2072-1).
#[derive(Debug, Clone)]
pub struct FittedPrompt {
    pub text: String,
    pub bytes: PromptBytes,
}

#[cfg(test)]
mod tests;
