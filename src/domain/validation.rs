//! The supervisor's validation of a run's receipt (ADR-0019, ADR-0029):
//! which facts from Git and the worktree it needs, in which order, and
//! whether they accept the receipt or reject it and why. [`judge`] is pure:
//! it asks for the next [`Fact`] it needs, and the application gathers it
//! and asks again, so no Git call is made for a check an earlier rejection
//! already settled. [`Validation`] is the outcome the store records.

use serde::Serialize;

use super::{
    CommitSha, DomainError, EvidenceCheck, ReasonCode, Receipt, RunId, evidence_missing_reason,
    measure::LoadSummary,
    scope::{out_of_scope, scope_violation_reason},
};

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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scope_violation: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_paths: Vec<String>,
    /// The load average from `receipt_observed` to this validation (task
    /// 197): the supervisor's samples, filled in when it records the result.
    #[serde(flatten)]
    pub load: LoadSummary,
}

impl Validation {
    /// Whether a rejection parks the run for a session instead of failing
    /// it: only required evidence is missing, or the diff leaves the
    /// task's paths.
    pub fn resumable(&self) -> bool {
        !self.evidence_missing.is_empty() || !self.scope_violation.is_empty()
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

/// A fact [`judge`] needs next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fact {
    /// Read the receipt file.
    Receipt,
    /// The run branch and what the worktree has checked out.
    CheckedOut,
    /// The head commit of the worktree.
    Head,
    /// Whether the head descends from the base commit.
    Descends,
    /// `git status` of the worktree.
    Status,
    /// The paths changed from where the head forked from main to the head.
    Changes,
}

/// What the application gathered so far for one validation, and what it
/// knew up front: the run, its base commit, the task's required evidence
/// and paths, and where the receipt should be.
#[derive(Debug)]
pub struct ReceiptFacts<'a> {
    pub run_id: &'a RunId,
    pub base: &'a CommitSha,
    pub required: &'a [EvidenceCheck],
    pub paths: &'a [String],
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
            paths,
            receipt_path,
            receipt: None,
            checked_out: None,
            head: None,
            descends: None,
            status: None,
            changes: None,
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
        scope_violation: Vec::new(),
    })
}

/// Judge the receipt on `facts`, in this order: the receipt exists, parses
/// and passes its own checks; the worktree is on the run branch and the
/// receipt names its head; the head is a new commit that descends from the
/// base; the worktree is clean; the diff stays in the task's paths; the
/// required evidence is there. Only a run sound in everything else waits
/// for a session over its paths or its evidence (ADR-0029, ADR-0019
/// decision 5).
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
    // A task without paths may change anything, so its diff is not read.
    if !facts.paths.is_empty() {
        let Some(changes) = &facts.changes else {
            return Judgement::Need(Fact::Changes);
        };
        let outside = out_of_scope(facts.paths, changes);
        if !outside.is_empty() {
            return Judgement::Reject(Rejection {
                reason: scope_violation_reason(&outside),
                code: ReasonCode::ScopeViolation,
                commit: Some(head.clone()),
                evidence_missing: Vec::new(),
                scope_violation: outside,
            });
        }
    }
    let missing = receipt.missing_evidence(facts.required);
    if !missing.is_empty() {
        return Judgement::Reject(Rejection {
            reason: evidence_missing_reason(&missing),
            code: ReasonCode::EvidenceMissing,
            commit: Some(head.clone()),
            evidence_missing: missing,
            scope_violation: Vec::new(),
        });
    }
    Judgement::Accept(head.clone())
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
        let id = RunId::new("r1").unwrap();
        let base = sha(BASE);
        let mut facts = ReceiptFacts::new(&id, &base, required, paths, "/runs/r1/receipt.json");
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
                verdict => return (asked, verdict),
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
        let world = World {
            receipt: Some(receipt_text("r1", "succeeded", "not_applicable")),
            ..sound()
        };
        let paths = ["src/**".to_owned()];
        let (asked, verdict) = run(&world, &[EvidenceCheck::E2e], &paths);
        assert_eq!(asked, ALL);
        let rejection = rejection(verdict);
        assert_eq!(rejection.code, ReasonCode::EvidenceMissing);
        assert_eq!(rejection.evidence_missing, [EvidenceCheck::E2e]);
        assert_eq!(rejection.reason, "evidence missing: e2e");
        assert!(rejection.scope_violation.is_empty());
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
                scope_violation,
                allowed_paths: Vec::new(),
                load: LoadSummary::default(),
            };
        assert!(!validation(Vec::new(), Vec::new()).resumable());
        assert!(validation(vec![EvidenceCheck::E2e], Vec::new()).resumable());
        assert!(validation(Vec::new(), vec!["x".into()]).resumable());
    }
}
