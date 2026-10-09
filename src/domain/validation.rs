//! The supervisor's validation of a run's receipt (ADR-0019, ADR-0029):
//! which facts from Git and the worktree it needs, in which order, and
//! whether they accept the receipt or reject it and why. [`judge`] is pure:
//! it asks for the next [`Fact`] it needs, and the application gathers it
//! and asks again, so no Git call is made for a check an earlier rejection
//! already settled. [`Validation`] is the outcome the store records.

use serde::Serialize;

use super::{
    CommitSha, DomainError, EvidenceCheck, ReasonCode, Receipt, RunId, evidence_missing_reason,
    execution_class::{missing_spike_result, spike_result_missing_reason},
    measure::LoadSummary,
    scope::{glob_matches, out_of_scope, scope_violation_reason},
};

/// Why a run needs the runtime's e2e (ADR-t963-1 decision 2, ADR-t1233-2
/// decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum E2eSource {
    /// The task's `required_evidence` (`add --evidence e2e`).
    Task,
    /// The run's diff touches `[e2e] paths` of `dagq.toml`.
    Paths,
}

impl E2eSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Paths => "paths",
        }
    }
}

/// Whether a run needs the e2e, and why: recorded with
/// `validation_finished` (as `e2e_requirement`), read by the runtime's e2e
/// after the review (ADR-t1233-2) and by `stats`. The worker's receipt
/// never backs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct E2eRequirement {
    pub required: bool,
    /// `None` when it is not required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<E2eSource>,
    /// The changed paths `[e2e] paths` matched, for [`E2eSource::Paths`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

impl E2eRequirement {
    /// `task` holds `e2e` when the task asks for it; otherwise the run
    /// needs it when a path of `changes` matches a glob of `globs`. No
    /// globs, no requirement from the diff.
    pub fn of(task: bool, globs: &[String], changes: &[String]) -> Self {
        if task {
            return Self {
                required: true,
                source: Some(E2eSource::Task),
                paths: Vec::new(),
            };
        }
        let paths: Vec<String> = changes
            .iter()
            .filter(|path| globs.iter().any(|glob| glob_matches(glob, path)))
            .cloned()
            .collect();
        Self {
            required: !paths.is_empty(),
            source: (!paths.is_empty()).then_some(E2eSource::Paths),
            paths,
        }
    }
}

/// Outcome of supervisor-side receipt validation. `result_commit` is kept on
/// rejection too when the commit itself was verified, so inspection can start there.
/// A rejection for nothing but `evidence_missing` (the task's required checks
/// the receipt does not back, ADR-0019 decision 5) parks the run as
/// `needs_session` instead of failing it, and so does one for a diff that
/// changes `scope_violation`, paths none of the task's `allowed_paths`
/// match (ADR-0029).
#[derive(Debug, Serialize)]
pub struct Validation {
    pub accepted: bool,
    pub result_commit: Option<CommitSha>,
    pub reason: Option<String>,
    /// The code of `reason` (ADR-0034); `None` for an accepted run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<ReasonCode>,
    pub receipt: serde_json::Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence_missing: Vec<EvidenceCheck>,
    /// The fields of a Spike's result its receipt lacks (ADR-t1487-1
    /// decision 3); never set for an implementation's run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spike_result_missing: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scope_violation: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_paths: Vec<String>,
    /// Whether `e2e` was required and why (ADR-t963-1 decision 2); `None`
    /// when validation stopped before it could tell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e2e_requirement: Option<E2eRequirement>,
    /// The load average from `receipt_observed` to this validation (task
    /// 197): the supervisor's samples, filled in when it records the result.
    #[serde(flatten)]
    pub load: LoadSummary,
}

impl Validation {
    /// Whether a rejection parks the run for a session instead of failing
    /// it: only required evidence or a Spike's result is missing, or the
    /// diff leaves the task's paths.
    pub fn resumable(&self) -> bool {
        !self.evidence_missing.is_empty()
            || !self.spike_result_missing.is_empty()
            || !self.scope_violation.is_empty()
    }
}

/// The receipt file as the application found it.
#[derive(Debug)]
pub enum ReceiptFile {
    /// Nothing at the receipt path.
    Missing,
    /// The file does not parse.
    Invalid(DomainError),
    Parsed(Receipt),
}

impl ReceiptFile {
    /// The file read as `text`, `None` when it does not exist.
    pub fn of(text: Option<&str>) -> Self {
        match text.map(Receipt::parse) {
            None => Self::Missing,
            Some(Ok(receipt)) => Self::Parsed(receipt),
            Some(Err(error)) => Self::Invalid(error),
        }
    }
}

/// The branch checked out in the run's worktree.
#[derive(Debug, Clone)]
pub struct CheckedOut {
    /// The run branch's name, such as `dagq/<run id>`.
    pub branch: String,
    /// The full ref the worktree is on, `None` for a detached HEAD.
    pub current: Option<String>,
}

/// A fact [`judge`] needs next, in the order it asks for them; each line
/// names the reason code a rejection on that fact carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fact {
    /// Read the receipt file: `receipt_missing` when there is none, else
    /// the code of the parse or check error.
    Receipt,
    /// The run branch and what the worktree has checked out:
    /// `commit_mismatch` when it is not on the run branch.
    CheckedOut,
    /// The head commit of the worktree: `commit_mismatch` when the receipt
    /// does not name it or it is still the base.
    Head,
    /// Whether the head descends from the base commit: `commit_mismatch`.
    Descends,
    /// `git status` of the worktree: `worktree_dirty`.
    Status,
    /// The paths changed from where the head forked from main to the head,
    /// asked only when the task declares paths or the diff decides the
    /// e2e: `scope_violation` for a path outside the task's.
    Changes,
}

/// What the application gathered so far for one validation, and what it
/// knew up front: the run, its base commit, the task's required evidence
/// and paths, whether the task asks for the e2e and the repository's `[e2e]
/// paths` (ADR-t963-1 decision 2), and where the receipt should be.
#[derive(Debug)]
pub struct ReceiptFacts<'a> {
    pub run_id: &'a RunId,
    pub base: &'a CommitSha,
    /// The checks the receipt must back ([`super::required_of`]): never
    /// `e2e`, which the runtime runs itself (ADR-t1233-2 decision 1).
    pub required: &'a [EvidenceCheck],
    /// The task asks for the e2e (`add --evidence e2e`).
    pub task_e2e: bool,
    /// The task is a Spike: its receipt must carry the result
    /// (ADR-t1487-1 decision 3).
    pub spike: bool,
    pub paths: &'a [String],
    pub e2e_paths: &'a [String],
    pub receipt_path: &'a str,
    pub receipt: Option<ReceiptFile>,
    pub checked_out: Option<CheckedOut>,
    pub head: Option<CommitSha>,
    pub descends: Option<bool>,
    pub status: Option<String>,
    pub changes: Option<Vec<String>>,
}

impl<'a> ReceiptFacts<'a> {
    /// Nothing gathered yet.
    pub fn new(
        run_id: &'a RunId,
        base: &'a CommitSha,
        required: &'a [EvidenceCheck],
        paths: &'a [String],
        receipt_path: &'a str,
    ) -> Self {
        Self {
            run_id,
            base,
            required,
            task_e2e: false,
            spike: false,
            paths,
            e2e_paths: &[],
            receipt_path,
            receipt: None,
            checked_out: None,
            head: None,
            descends: None,
            status: None,
            changes: None,
        }
    }

    /// With the repository's `[e2e] paths` (ADR-t963-1 decision 2).
    pub fn with_e2e_paths(mut self, e2e_paths: &'a [String]) -> Self {
        self.e2e_paths = e2e_paths;
        self
    }

    /// With whether the task asks for the e2e (`add --evidence e2e`).
    pub fn with_task_e2e(mut self, task_e2e: bool) -> Self {
        self.task_e2e = task_e2e;
        self
    }

    /// With whether the task is a Spike (ADR-t1487-1 decision 3).
    pub fn with_spike(mut self, spike: bool) -> Self {
        self.spike = spike;
        self
    }

    /// Whether the diff decides `e2e`: the repository names paths and the
    /// task does not require it anyway.
    fn diff_decides_e2e(&self) -> bool {
        !self.e2e_paths.is_empty() && !self.task_e2e
    }

    /// Whether `e2e` is required and why, once the facts tell; `None`
    /// while the diff that decides it is not read.
    pub fn e2e_requirement(&self) -> Option<E2eRequirement> {
        if self.diff_decides_e2e() {
            let changes = self.changes.as_deref()?;
            Some(E2eRequirement::of(self.task_e2e, self.e2e_paths, changes))
        } else {
            Some(E2eRequirement::of(self.task_e2e, &[], &[]))
        }
    }

    /// The parsed receipt, to hand on with the verdict.
    pub fn into_receipt(self) -> Option<Receipt> {
        match self.receipt {
            Some(ReceiptFile::Parsed(receipt)) => Some(receipt),
            _ => None,
        }
    }
}

/// Why the receipt was not accepted, with the commit when it was verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub reason: String,
    /// The code of `reason` (ADR-0034).
    pub code: ReasonCode,
    pub commit: Option<CommitSha>,
    /// The task's required checks the receipt does not back, when that is
    /// all that is wrong: the run waits for a session instead of failing.
    pub evidence_missing: Vec<EvidenceCheck>,
    /// The fields of a Spike's result the receipt lacks, likewise.
    pub spike_result_missing: Vec<String>,
    /// The changed paths outside the task's `paths` (ADR-0029), when the
    /// run is otherwise sound: it waits for a session to take them out.
    pub scope_violation: Vec<String>,
}

/// The verdict on the facts so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgement {
    /// Gather this fact and judge again.
    Need(Fact),
    /// The receipt names this commit, the clean head of the run branch.
    Accept(CommitSha),
    Reject(Rejection),
}

fn reject(code: ReasonCode, reason: String, commit: Option<&CommitSha>) -> Judgement {
    Judgement::Reject(Rejection {
        reason,
        code,
        commit: commit.cloned(),
        evidence_missing: Vec::new(),
        spike_result_missing: Vec::new(),
        scope_violation: Vec::new(),
    })
}

/// Judge the receipt on `facts`, in this order: the receipt exists, parses
/// and passes its own checks; the worktree is on the run branch and the
/// receipt names its head; the head is a new commit that descends from the
/// base; the worktree is clean; the diff stays in the task's paths; the
/// required evidence is there. Only a run sound in everything else waits
/// for a session over its paths or its evidence (ADR-0029, ADR-0019
/// decision 5). The evidence required is the task's but `e2e`, which the
/// runtime runs itself after the review (ADR-t1233-2 decision 1); whether
/// the run needs it is read from the task and, when the repository names
/// `[e2e] paths`, from the diff, and recorded with the verdict.
pub fn judge(facts: &ReceiptFacts<'_>) -> Judgement {
    let receipt = match &facts.receipt {
        None => return Judgement::Need(Fact::Receipt),
        Some(ReceiptFile::Missing) => {
            return reject(
                ReasonCode::ReceiptMissing,
                format!("receipt was not submitted at {}", facts.receipt_path),
                None,
            );
        }
        Some(ReceiptFile::Invalid(error)) => {
            return reject(
                ReasonCode::of_receipt_error(error),
                format!("{error:#}"),
                None,
            );
        }
        Some(ReceiptFile::Parsed(receipt)) => receipt,
    };
    if let Err(error) = receipt.check_requiring(facts.run_id, facts.required) {
        return reject(
            ReasonCode::of_receipt_error(&error),
            format!("{error:#}"),
            None,
        );
    }
    // The commit must be the head of the run branch, checked out in the
    // worktree, and new work on top of the base commit.
    let Some(checked_out) = &facts.checked_out else {
        return Judgement::Need(Fact::CheckedOut);
    };
    let expected_ref = format!("refs/heads/{}", checked_out.branch);
    if checked_out.current.as_deref() != Some(expected_ref.as_str()) {
        return reject(
            ReasonCode::CommitMismatch,
            format!(
                "worktree is on {} instead of {expected_ref}",
                checked_out.current.as_deref().unwrap_or("a detached HEAD")
            ),
            None,
        );
    }
    let Some(head) = &facts.head else {
        return Judgement::Need(Fact::Head);
    };
    if !receipt.names_commit(head.as_str()) {
        return reject(
            ReasonCode::CommitMismatch,
            format!(
                "receipt commit {} is not the head of {} ({head})",
                receipt.commit(),
                checked_out.branch
            ),
            None,
        );
    }
    if head == facts.base {
        return reject(
            ReasonCode::CommitMismatch,
            format!("no commit was made on top of base {}", facts.base),
            Some(head),
        );
    }
    let Some(descends) = facts.descends else {
        return Judgement::Need(Fact::Descends);
    };
    if !descends {
        return reject(
            ReasonCode::CommitMismatch,
            format!("commit {head} does not descend from base {}", facts.base),
            Some(head),
        );
    }
    let Some(status) = &facts.status else {
        return Judgement::Need(Fact::Status);
    };
    if !status.trim().is_empty() {
        return reject(
            ReasonCode::WorktreeDirty,
            format!("worktree is not clean:\n{}", status.trim_end()),
            Some(head),
        );
    }
    // A task without paths may change anything, so its diff is not read
    // unless it decides `e2e`.
    if (!facts.paths.is_empty() || facts.diff_decides_e2e()) && facts.changes.is_none() {
        return Judgement::Need(Fact::Changes);
    }
    if let Some(changes) = &facts.changes {
        let outside = out_of_scope(facts.paths, changes);
        if !outside.is_empty() {
            return Judgement::Reject(Rejection {
                reason: scope_violation_reason(&outside),
                code: ReasonCode::ScopeViolation,
                commit: Some(head.clone()),
                evidence_missing: Vec::new(),
                spike_result_missing: Vec::new(),
                scope_violation: outside,
            });
        }
    }
    let missing = receipt.missing_evidence(facts.required);
    let spike_missing = if facts.spike {
        missing_spike_result(receipt.spike_result())
    } else {
        Vec::new()
    };
    if !missing.is_empty() || !spike_missing.is_empty() {
        let (code, reason) = missing_reason(&missing, &spike_missing);
        return Judgement::Reject(Rejection {
            reason,
            code,
            commit: Some(head.clone()),
            evidence_missing: missing,
            spike_result_missing: spike_missing,
            scope_violation: Vec::new(),
        });
    }
    Judgement::Accept(head.clone())
}

/// The code and the reason of a park for what the receipt lacks: the
/// required checks, then a Spike's result. The code is `evidence_missing`
/// when a check is missing, else `spike_result_missing`; the reason names
/// both.
pub fn missing_reason(evidence: &[EvidenceCheck], spike: &[String]) -> (ReasonCode, String) {
    match (evidence.is_empty(), spike.is_empty()) {
        (false, true) => (
            ReasonCode::EvidenceMissing,
            evidence_missing_reason(evidence),
        ),
        (true, _) => (
            ReasonCode::SpikeResultMissing,
            spike_result_missing_reason(spike),
        ),
        (false, false) => (
            ReasonCode::EvidenceMissing,
            format!(
                "{}; {}",
                evidence_missing_reason(evidence),
                spike_result_missing_reason(spike)
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "0000000000000000000000000000000000000001";
    const HEAD: &str = "0000000000000000000000000000000000000002";

    fn sha(text: &str) -> CommitSha {
        CommitSha::parse(text, "commit").unwrap()
    }

    fn receipt_text(run_id: &str, result: &str, e2e: &str) -> String {
        serde_json::json!({
            "run_id": run_id,
            "result": result,
            "commit": HEAD,
            "tests": {"status": "passed", "evidence_or_reason": "cargo test"},
            "e2e": {"status": e2e, "evidence_or_reason": if e2e == "passed" { "e2e ok" } else { "no cmux" }},
            "subagent_review": {"status": "not_applicable", "evidence_or_reason": "small"},
            "summary": "done",
        })
        .to_string()
    }

    struct World {
        receipt: Option<String>,
        current: Option<String>,
        head: CommitSha,
        descends: bool,
        status: String,
        changes: Vec<String>,
    }

    fn sound() -> World {
        World {
            receipt: Some(receipt_text("r1", "succeeded", "passed")),
            current: Some("refs/heads/dagq/r1".into()),
            head: sha(HEAD),
            descends: true,
            status: String::new(),
            changes: vec!["src/lib.rs".into()],
        }
    }

    /// Judge `world` for a task with `required` evidence and `paths`,
    /// gathering each fact the judge asks for; the facts it asked for, in
    /// order, come back with the verdict.
    fn run(world: &World, required: &[EvidenceCheck], paths: &[String]) -> (Vec<Fact>, Judgement) {
        run_with_e2e(world, required, paths, &[]).0
    }

    /// [`run`] for a repository whose `[e2e] paths` are `e2e_paths`, with
    /// the requirement the facts told.
    fn run_with_e2e(
        world: &World,
        required: &[EvidenceCheck],
        paths: &[String],
        e2e_paths: &[String],
    ) -> ((Vec<Fact>, Judgement), Option<E2eRequirement>) {
        run_as(world, required, paths, e2e_paths, false)
    }

    /// [`run_with_e2e`] for a Spike's run when `spike`.
    fn run_as(
        world: &World,
        required: &[EvidenceCheck],
        paths: &[String],
        e2e_paths: &[String],
        spike: bool,
    ) -> ((Vec<Fact>, Judgement), Option<E2eRequirement>) {
        let id = RunId::new("r1").unwrap();
        let base = sha(BASE);
        // As the supervisor gives them: the receipt backs the task's checks
        // but `e2e`, which only says whether the run needs the e2e.
        let checks = super::super::required_of(required, crate::domain::Provider::Claude);
        let mut facts = ReceiptFacts::new(&id, &base, &checks, paths, "/runs/r1/receipt.json")
            .with_e2e_paths(e2e_paths)
            .with_task_e2e(required.contains(&EvidenceCheck::E2e))
            .with_spike(spike);
        let mut asked = Vec::new();
        loop {
            match judge(&facts) {
                Judgement::Need(fact) => {
                    assert!(!asked.contains(&fact), "{fact:?} asked twice");
                    asked.push(fact);
                    match fact {
                        Fact::Receipt => {
                            facts.receipt = Some(ReceiptFile::of(world.receipt.as_deref()));
                        }
                        Fact::CheckedOut => {
                            facts.checked_out = Some(CheckedOut {
                                branch: "dagq/r1".into(),
                                current: world.current.clone(),
                            });
                        }
                        Fact::Head => facts.head = Some(world.head.clone()),
                        Fact::Descends => facts.descends = Some(world.descends),
                        Fact::Status => facts.status = Some(world.status.clone()),
                        Fact::Changes => facts.changes = Some(world.changes.clone()),
                    }
                }
                verdict => return ((asked, verdict), facts.e2e_requirement()),
            }
        }
    }

    fn rejection(verdict: Judgement) -> Rejection {
        match verdict {
            Judgement::Reject(rejection) => rejection,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    const ALL: [Fact; 6] = [
        Fact::Receipt,
        Fact::CheckedOut,
        Fact::Head,
        Fact::Descends,
        Fact::Status,
        Fact::Changes,
    ];

    #[test]
    fn a_sound_receipt_is_accepted_after_every_fact() {
        let paths = ["src/**".to_owned()];
        let (asked, verdict) = run(&sound(), &[EvidenceCheck::E2e], &paths);
        assert_eq!(asked, ALL);
        assert_eq!(verdict, Judgement::Accept(sha(HEAD)));
    }

    #[test]
    fn a_task_without_paths_never_reads_the_diff() {
        let (asked, verdict) = run(&sound(), &[], &[]);
        assert_eq!(asked, ALL[..5]);
        assert_eq!(verdict, Judgement::Accept(sha(HEAD)));
    }

    #[test]
    fn a_missing_receipt_is_rejected_before_git_is_read() {
        let world = World {
            receipt: None,
            ..sound()
        };
        let (asked, verdict) = run(&world, &[], &[]);
        assert_eq!(asked, [Fact::Receipt]);
        let rejection = rejection(verdict);
        assert_eq!(rejection.code, ReasonCode::ReceiptMissing);
        assert_eq!(
            rejection.reason,
            "receipt was not submitted at /runs/r1/receipt.json"
        );
        assert_eq!(rejection.commit, None);
    }

    #[test]
    fn a_receipt_that_does_not_parse_or_check_is_rejected_before_git_is_read() {
        let world = World {
            receipt: Some("{".into()),
            ..sound()
        };
        let (asked, verdict) = run(&world, &[], &[]);
        assert_eq!(asked, [Fact::Receipt]);
        assert_eq!(rejection(verdict).code, ReasonCode::ReceiptInvalid);

        let world = World {
            receipt: Some(receipt_text("r1", "failed", "passed")),
            ..sound()
        };
        let (asked, verdict) = run(&world, &[], &[]);
        assert_eq!(asked, [Fact::Receipt]);
        assert_eq!(rejection(verdict).code, ReasonCode::WorkerFailed);

        let world = World {
            receipt: Some(receipt_text("other", "succeeded", "passed")),
            ..sound()
        };
        assert_eq!(
            rejection(run(&world, &[], &[]).1).code,
            ReasonCode::ReceiptInvalid
        );
    }

    #[test]
    fn a_worktree_off_the_run_branch_is_a_commit_mismatch() {
        let world = World {
            current: None,
            ..sound()
        };
        let (asked, verdict) = run(&world, &[], &[]);
        assert_eq!(asked, ALL[..2]);
        let rejection = rejection(verdict);
        assert_eq!(rejection.code, ReasonCode::CommitMismatch);
        assert_eq!(
            rejection.reason,
            "worktree is on a detached HEAD instead of refs/heads/dagq/r1"
        );
        let world = World {
            current: Some("refs/heads/main".into()),
            ..sound()
        };
        assert_eq!(
            rejection_reason(&world),
            "worktree is on refs/heads/main instead of refs/heads/dagq/r1"
        );
    }

    fn rejection_reason(world: &World) -> String {
        rejection(run(world, &[], &[]).1).reason
    }

    #[test]
    fn a_receipt_that_does_not_name_the_head_is_a_commit_mismatch() {
        let other = "0000000000000000000000000000000000000003";
        let world = World {
            head: sha(other),
            ..sound()
        };
        let (asked, verdict) = run(&world, &[], &[]);
        assert_eq!(asked, ALL[..3]);
        let rejection = rejection(verdict);
        assert_eq!(rejection.code, ReasonCode::CommitMismatch);
        assert_eq!(rejection.commit, None);
        assert_eq!(
            rejection.reason,
            format!("receipt commit {HEAD} is not the head of dagq/r1 ({other})")
        );
    }

    #[test]
    fn no_new_commit_is_rejected_with_the_commit_before_the_ancestry() {
        let mut text = receipt_text("r1", "succeeded", "passed");
        text = text.replace(HEAD, BASE);
        let world = World {
            receipt: Some(text),
            head: sha(BASE),
            ..sound()
        };
        let (asked, verdict) = run(&world, &[], &[]);
        assert_eq!(asked, ALL[..3]);
        let rejection = rejection(verdict);
        assert_eq!(rejection.code, ReasonCode::CommitMismatch);
        assert_eq!(rejection.commit, Some(sha(BASE)));
        assert_eq!(
            rejection.reason,
            format!("no commit was made on top of base {BASE}")
        );
    }

    #[test]
    fn a_head_off_the_base_is_a_commit_mismatch() {
        let world = World {
            descends: false,
            ..sound()
        };
        let (asked, verdict) = run(&world, &[], &[]);
        assert_eq!(asked, ALL[..4]);
        let rejection = rejection(verdict);
        assert_eq!(rejection.code, ReasonCode::CommitMismatch);
        assert_eq!(rejection.commit, Some(sha(HEAD)));
        assert_eq!(
            rejection.reason,
            format!("commit {HEAD} does not descend from base {BASE}")
        );
    }

    #[test]
    fn a_dirty_worktree_is_rejected_before_the_scope_and_the_evidence() {
        let world = World {
            receipt: Some(receipt_text("r1", "succeeded", "not_applicable")),
            status: " M src/lib.rs\n".into(),
            changes: vec!["docs/x.md".into()],
            ..sound()
        };
        let paths = ["src/**".to_owned()];
        let (asked, verdict) = run(&world, &[EvidenceCheck::E2e], &paths);
        assert_eq!(asked, ALL[..5]);
        let rejection = rejection(verdict);
        assert_eq!(rejection.code, ReasonCode::WorktreeDirty);
        assert_eq!(rejection.reason, "worktree is not clean:\n M src/lib.rs");
        assert_eq!(rejection.commit, Some(sha(HEAD)));
        assert!(rejection.scope_violation.is_empty());
        assert!(rejection.evidence_missing.is_empty());
    }

    #[test]
    fn a_path_outside_the_task_is_rejected_before_missing_evidence() {
        let world = World {
            receipt: Some(receipt_text("r1", "succeeded", "not_applicable")),
            changes: vec!["src/lib.rs".into(), "docs/x.md".into()],
            ..sound()
        };
        let paths = ["src/**".to_owned()];
        let (asked, verdict) = run(&world, &[EvidenceCheck::E2e], &paths);
        assert_eq!(asked, ALL);
        let rejection = rejection(verdict);
        assert_eq!(rejection.code, ReasonCode::ScopeViolation);
        assert_eq!(rejection.scope_violation, ["docs/x.md"]);
        assert_eq!(
            rejection.reason,
            scope_violation_reason(&["docs/x.md".into()])
        );
        assert!(rejection.evidence_missing.is_empty());
        assert_eq!(rejection.commit, Some(sha(HEAD)));
    }

    #[test]
    fn missing_required_evidence_is_rejected_last() {
        let paths = ["src/**".to_owned()];
        let (asked, verdict) = run(&sound(), &[EvidenceCheck::SubagentReview], &paths);
        assert_eq!(asked, ALL);
        let rejection = rejection(verdict);
        assert_eq!(rejection.code, ReasonCode::EvidenceMissing);
        assert_eq!(rejection.evidence_missing, [EvidenceCheck::SubagentReview]);
        assert_eq!(rejection.reason, "evidence missing: subagent_review");
        assert!(rejection.scope_violation.is_empty());
    }

    /// A required check the receipt reports as `failed` is missing
    /// evidence, which parks the run for a session to add it, not a
    /// failure; the same receipt for a task that does not require it fails.
    #[test]
    fn a_required_check_reported_failed_is_missing_evidence() {
        let receipt = serde_json::json!({
            "run_id": "r1",
            "result": "succeeded",
            "commit": HEAD,
            "tests": {"status": "failed", "evidence_or_reason": "a test failed"},
            "e2e": {"status": "not_applicable", "evidence_or_reason": "no cmux"},
            "subagent_review": {"status": "not_applicable", "evidence_or_reason": "small"},
            "summary": "done",
        })
        .to_string();
        let world = World {
            receipt: Some(receipt),
            ..sound()
        };
        let (asked, verdict) = run(&world, &[EvidenceCheck::Tests], &[]);
        assert_eq!(asked, ALL[..5]);
        let parked = rejection(verdict);
        assert_eq!(parked.code, ReasonCode::EvidenceMissing);
        assert_eq!(parked.evidence_missing, [EvidenceCheck::Tests]);
        assert_eq!(parked.reason, "evidence missing: tests");
        assert_eq!(parked.commit, Some(sha(HEAD)));
        let (_, verdict) = run(&world, &[], &[]);
        assert_eq!(rejection(verdict).code, ReasonCode::EvidenceFailed);
    }

    /// A receipt with the required `tests` evidence is accepted as it would
    /// be without a requirement, and a task's `e2e` asks no evidence of it:
    /// it is recorded as the task's requirement for the runtime to run.
    #[test]
    fn required_evidence_present_is_accepted_and_the_task_e2e_recorded() {
        let world = World {
            receipt: Some(receipt_text("r1", "succeeded", "not_applicable")),
            ..sound()
        };
        let ((asked, verdict), requirement) = run_with_e2e(
            &world,
            &[EvidenceCheck::E2e, EvidenceCheck::Tests],
            &[],
            &[],
        );
        assert_eq!(asked, ALL[..5]);
        assert_eq!(verdict, Judgement::Accept(sha(HEAD)));
        assert_eq!(
            serde_json::to_value(requirement.unwrap()).unwrap(),
            serde_json::json!({"required": true, "source": "task"})
        );
    }

    #[test]
    fn the_receipt_is_handed_on_only_when_it_parsed() {
        let id = RunId::new("r1").unwrap();
        let base = sha(BASE);
        let mut facts = ReceiptFacts::new(&id, &base, &[], &[], "p");
        facts.receipt = Some(ReceiptFile::of(None));
        assert!(facts.into_receipt().is_none());
        let mut facts = ReceiptFacts::new(&id, &base, &[], &[], "p");
        facts.receipt = Some(ReceiptFile::of(Some(&receipt_text(
            "r1",
            "succeeded",
            "passed",
        ))));
        assert_eq!(facts.into_receipt().unwrap().summary(), "done");
    }

    #[test]
    fn resumable_rejections_are_scope_and_evidence() {
        let validation =
            |evidence_missing: Vec<EvidenceCheck>, scope_violation: Vec<String>| Validation {
                accepted: false,
                result_commit: None,
                reason: None,
                code: None,
                receipt: serde_json::Value::Null,
                evidence_missing,
                spike_result_missing: Vec::new(),
                scope_violation,
                allowed_paths: Vec::new(),
                e2e_requirement: None,
                load: LoadSummary::default(),
            };
        assert!(!validation(Vec::new(), Vec::new()).resumable());
        assert!(validation(vec![EvidenceCheck::E2e], Vec::new()).resumable());
        assert!(validation(Vec::new(), vec!["x".into()]).resumable());
        let mut spike = validation(Vec::new(), Vec::new());
        spike.spike_result_missing = vec!["spike_result".into()];
        assert!(spike.resumable());
    }

    /// A Spike's receipt: [`receipt_text`] with `spike_result`.
    fn spike_world(result: Option<serde_json::Value>) -> World {
        let mut receipt: serde_json::Value =
            serde_json::from_str(&receipt_text("r1", "succeeded", "passed")).unwrap();
        if let Some(result) = result {
            receipt["spike_result"] = result;
        }
        World {
            receipt: Some(receipt.to_string()),
            ..sound()
        }
    }

    fn spike_result(verdict: &str) -> serde_json::Value {
        serde_json::json!({
            "verdict": verdict,
            "grounds": "measured twice",
            "evidence": "docs/plans/spike.md",
            "conditions": {
                "commit": HEAD,
                "tools": "cargo 1.90",
                "provider": "claude headless worker",
                "environment": "macOS arm64",
            },
        })
    }

    #[test]
    fn a_spike_without_its_result_is_rejected_last_for_a_session() {
        let ((asked, verdict), _) = run_as(&spike_world(None), &[], &[], &[], true);
        assert_eq!(asked, ALL[..5]);
        let missing = rejection(verdict);
        assert_eq!(missing.code, ReasonCode::SpikeResultMissing);
        assert_eq!(missing.spike_result_missing, ["spike_result"]);
        assert_eq!(missing.reason, "spike result missing: spike_result");
        assert!(missing.evidence_missing.is_empty());
        assert_eq!(missing.commit, Some(sha(HEAD)));
        // Missing evidence as well: the code is the evidence's, the reason
        // names both.
        let mut partial = spike_result("holds");
        partial["grounds"] = serde_json::json!("");
        let ((_, verdict), _) = run_as(
            &spike_world(Some(partial)),
            &[EvidenceCheck::SubagentReview],
            &[],
            &[],
            true,
        );
        let both = rejection(verdict);
        assert_eq!(both.code, ReasonCode::EvidenceMissing);
        assert_eq!(both.evidence_missing, [EvidenceCheck::SubagentReview]);
        assert_eq!(both.spike_result_missing, ["spike_result.grounds"]);
        assert_eq!(
            both.reason,
            "evidence missing: subagent_review; spike result missing: spike_result.grounds"
        );
    }

    #[test]
    fn a_spike_with_any_verdict_and_its_fields_is_accepted() {
        for verdict in ["holds", "does_not_hold", "unresolved"] {
            let world = spike_world(Some(spike_result(verdict)));
            let ((_, judged), _) = run_as(&world, &[], &[], &[], true);
            assert_eq!(judged, Judgement::Accept(sha(HEAD)), "{verdict}");
        }
    }

    #[test]
    fn an_implementation_is_never_asked_for_a_spike_result() {
        let ((_, verdict), _) = run_as(&spike_world(None), &[], &[], &[], false);
        assert_eq!(verdict, Judgement::Accept(sha(HEAD)));
    }

    fn globs(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn a_diff_touching_the_e2e_paths_needs_the_e2e_but_no_evidence() {
        let world = World {
            receipt: Some(receipt_text("r1", "succeeded", "not_applicable")),
            changes: vec!["docs/a.md".into(), "src/infrastructure/process.rs".into()],
            ..sound()
        };
        let e2e = globs(&["src/infrastructure/process.rs", "tests/e2e.rs"]);
        let ((asked, verdict), requirement) = run_with_e2e(&world, &[], &[], &e2e);
        assert_eq!(asked, ALL);
        // The runtime runs the e2e after the review (ADR-t1233-2).
        assert_eq!(verdict, Judgement::Accept(sha(HEAD)));
        let requirement = requirement.unwrap();
        assert!(requirement.required);
        assert_eq!(requirement.source, Some(E2eSource::Paths));
        assert_eq!(requirement.paths, ["src/infrastructure/process.rs"]);

        // A receipt's `e2e` is held to what any check that is not required
        // is: not `failed`, and explained.
        let world = World {
            receipt: Some(receipt_text("r1", "succeeded", "failed")),
            ..world
        };
        let ((asked, verdict), _) = run_with_e2e(&world, &[], &[], &e2e);
        assert_eq!(asked, [Fact::Receipt]);
        assert_eq!(rejection(verdict).code, ReasonCode::EvidenceFailed);
    }

    #[test]
    fn a_diff_outside_the_e2e_paths_lands_without_e2e() {
        let world = World {
            receipt: Some(receipt_text("r1", "succeeded", "not_applicable")),
            changes: vec!["src/domain/stats.rs".into()],
            ..sound()
        };
        let e2e = globs(&["src/infrastructure/**", "tests/e2e.rs"]);
        let ((asked, verdict), requirement) = run_with_e2e(&world, &[], &[], &e2e);
        assert_eq!(asked, ALL);
        assert_eq!(verdict, Judgement::Accept(sha(HEAD)));
        assert_eq!(
            requirement,
            Some(E2eRequirement {
                required: false,
                source: None,
                paths: Vec::new(),
            })
        );
    }

    #[test]
    fn a_task_that_requires_e2e_keeps_it_whatever_the_diff() {
        let world = World {
            receipt: Some(receipt_text("r1", "succeeded", "not_applicable")),
            changes: vec!["docs/a.md".into()],
            ..sound()
        };
        let e2e = globs(&["src/**"]);
        let ((asked, verdict), requirement) =
            run_with_e2e(&world, &[EvidenceCheck::E2e], &[], &e2e);
        // The task decides, so a task without paths does not read the diff,
        // and the receipt backs no e2e.
        assert_eq!(asked, ALL[..5]);
        assert_eq!(verdict, Judgement::Accept(sha(HEAD)));
        assert_eq!(requirement.unwrap().source, Some(E2eSource::Task));
    }

    #[test]
    fn without_e2e_paths_the_diff_requires_nothing() {
        let world = World {
            receipt: Some(receipt_text("r1", "succeeded", "not_applicable")),
            ..sound()
        };
        let ((asked, verdict), requirement) = run_with_e2e(&world, &[], &[], &[]);
        assert_eq!(asked, ALL[..5]);
        assert_eq!(verdict, Judgement::Accept(sha(HEAD)));
        assert!(!requirement.unwrap().required);
    }

    #[test]
    fn the_requirement_names_its_source_and_paths() {
        let paths = E2eRequirement::of(false, &globs(&["src/*.rs"]), &globs(&["src/a.rs"]));
        assert_eq!(E2eSource::Paths.as_str(), "paths");
        assert_eq!(E2eSource::Task.as_str(), "task");
        assert_eq!(
            serde_json::to_value(&paths).unwrap(),
            serde_json::json!({"required": true, "source": "paths", "paths": ["src/a.rs"]})
        );
        assert_eq!(
            serde_json::to_value(E2eRequirement::of(true, &[], &globs(&["src/a.rs"]))).unwrap(),
            serde_json::json!({"required": true, "source": "task"})
        );
        assert_eq!(
            serde_json::to_value(E2eRequirement::of(false, &[], &[])).unwrap(),
            serde_json::json!({"required": false})
        );
    }
}
