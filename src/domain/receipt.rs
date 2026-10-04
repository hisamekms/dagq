//! The completion receipt a session writes. [`Receipt::parse`] is the only
//! way to build one: its fields are private and it is not `Deserialize`, so
//! every receipt the runtime holds has the shape parsing verified. Whether
//! it backs the run it names is the query [`Receipt::check_requiring`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    CheckStatus, CommitSha, DomainError, EvidenceCheck, Provider, ReceiptResult, RunId, require,
};

/// Completion receipt written by the agent. Its claims are cross-checked by
/// the supervisor; the receipt alone never marks a run successful.
///
/// `run_id` and `commit` stay the strings the agent wrote: they are checked
/// by [`Self::check_requiring`] in its fixed order, not at parse time, so a
/// `failed` receipt can still be read for its summary.
#[derive(Debug, Clone, Serialize)]
pub struct Receipt {
    run_id: String,
    result: ReceiptResult,
    commit: String,
    tests: ReceiptCheck,
    e2e: ReceiptCheck,
    subagent_review: ReceiptCheck,
    summary: String,
    /// Follow-up tasks the agent proposes, as `{"title", "description",
    /// "category"}` objects (`category`: the worker's code for the kind of
    /// work, ADR-t947-3; a missing or unknown one never rejects the
    /// receipt). Only its shape (an array) is checked here; `integrate`
    /// registers each entry with a title as a draft task once the run lands,
    /// with its category, and a planner the runtime opens for the drafts
    /// (ADR-0044 decision 16) submits it for plan review, cancels it or asks
    /// a person.
    #[serde(skip_serializing_if = "Option::is_none")]
    follow_ups: Option<Value>,
}

/// One of the receipt's `tests`, `e2e` and `subagent_review` claims.
#[derive(Debug, Clone, Serialize)]
pub struct ReceiptCheck {
    status: CheckStatus,
    evidence_or_reason: String,
}

/// The receipt file's JSON, only ever turned into a [`Receipt`] by
/// [`Receipt::parse`].
#[derive(Deserialize)]
struct ReceiptFile {
    run_id: String,
    result: ReceiptResult,
    commit: String,
    tests: ReceiptCheckFile,
    e2e: ReceiptCheckFile,
    subagent_review: ReceiptCheckFile,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    follow_ups: Option<Value>,
}

#[derive(Deserialize)]
struct ReceiptCheckFile {
    status: CheckStatus,
    #[serde(default)]
    evidence_or_reason: String,
}

impl From<ReceiptCheckFile> for ReceiptCheck {
    fn from(file: ReceiptCheckFile) -> Self {
        Self {
            status: file.status,
            evidence_or_reason: file.evidence_or_reason,
        }
    }
}

impl ReceiptCheck {
    pub fn status(&self) -> CheckStatus {
        self.status
    }

    pub fn evidence_or_reason(&self) -> &str {
        &self.evidence_or_reason
    }

    /// `passed` with evidence: what a required check must be.
    fn backed(&self) -> bool {
        self.status == CheckStatus::Passed && !self.evidence_or_reason.trim().is_empty()
    }
}

impl Receipt {
    /// Reads a receipt file's text: a JSON object with the receipt's fields
    /// and known `result` and check statuses.
    pub fn parse(text: &str) -> Result<Self, DomainError> {
        let file: ReceiptFile =
            serde_json::from_str(text).map_err(|error| DomainError::MalformedReceipt {
                reason: error.to_string(),
            })?;
        Ok(Self {
            run_id: file.run_id,
            result: file.result,
            commit: file.commit,
            tests: file.tests.into(),
            e2e: file.e2e.into(),
            subagent_review: file.subagent_review.into(),
            summary: file.summary,
            follow_ups: file.follow_ups,
        })
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn result(&self) -> ReceiptResult {
        self.result
    }

    /// The commit as the agent wrote it; compare it lowercased with Git's.
    pub fn commit(&self) -> &str {
        &self.commit
    }

    /// Whether the receipt names `head` (a lowercase Git object ID).
    pub fn names_commit(&self, head: &str) -> bool {
        self.commit.to_ascii_lowercase() == head
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn tests(&self) -> &ReceiptCheck {
        &self.tests
    }

    pub fn e2e(&self) -> &ReceiptCheck {
        &self.e2e
    }

    pub fn subagent_review(&self) -> &ReceiptCheck {
        &self.subagent_review
    }

    pub fn follow_ups(&self) -> Option<&Value> {
        self.follow_ups.as_ref()
    }

    pub fn into_follow_ups(self) -> Option<Value> {
        self.follow_ups
    }

    /// Structural consistency only; Git state and verification commands are checked by the supervisor.
    pub fn check(&self, run_id: &RunId) -> Result<(), DomainError> {
        self.check_requiring(run_id, &[])
    }

    /// [`Self::check`], except that a `required` check reported `failed` or
    /// with a blank `evidence_or_reason` is left to
    /// [`Self::missing_evidence`]: the run then waits for a session instead
    /// of failing (ADR-0019 decision 5).
    pub fn check_requiring(
        &self,
        run_id: &RunId,
        required: &[EvidenceCheck],
    ) -> Result<(), DomainError> {
        require(self.run_id == run_id.as_str(), || {
            DomainError::ReceiptRunMismatch {
                receipt_run_id: self.run_id.clone(),
                run_id: run_id.clone(),
            }
        })?;
        require(self.result == ReceiptResult::Succeeded, || {
            DomainError::AgentReportedResult {
                result: self.result,
                summary: self.summary.clone(),
            }
        })?;
        for evidence in [
            EvidenceCheck::Tests,
            EvidenceCheck::E2e,
            EvidenceCheck::SubagentReview,
        ] {
            let name = evidence.as_str();
            let check = self.evidence(evidence);
            if required.contains(&evidence) {
                continue;
            }
            require(check.status != CheckStatus::Failed, || {
                DomainError::ReceiptCheckFailed {
                    check: name,
                    evidence_or_reason: check.evidence_or_reason.clone(),
                }
            })?;
            require(!check.evidence_or_reason.trim().is_empty(), || {
                DomainError::ReceiptCheckUnexplained {
                    check: name,
                    status: check.status,
                }
            })?;
        }
        CommitSha::parse(self.commit.as_str(), "receipt commit")?;
        require(
            self.follow_ups.as_ref().is_none_or(|f| f.is_array()),
            || DomainError::FollowUpsNotArray,
        )
    }

    fn evidence(&self, check: EvidenceCheck) -> &ReceiptCheck {
        match check {
            EvidenceCheck::Tests => &self.tests,
            EvidenceCheck::E2e => &self.e2e,
            EvidenceCheck::SubagentReview => &self.subagent_review,
        }
    }

    /// The `required` checks this receipt does not back: a status other
    /// than `passed`, or no evidence.
    pub fn missing_evidence(&self, required: &[EvidenceCheck]) -> Vec<EvidenceCheck> {
        required
            .iter()
            .copied()
            .filter(|check| !self.evidence(*check).backed())
            .collect()
    }
}

/// The checks of a task's `required` evidence that a run whose worker is
/// `provider` must back in its receipt. No worker backs `e2e`: the runtime
/// runs it on the host after the review (ADR-t1233-2 decision 1), and the
/// task's `e2e` only says that the run needs it. A Codex worker has no
/// subagent to review its change, so a required `subagent_review` does not
/// hold its run either: its receipt reports the check `not_applicable` with
/// a reason, as for any check that is not required, and the supervisor's
/// review job reviews the commit before it lands. Claude's workers keep
/// every other required check.
pub fn required_of(required: &[EvidenceCheck], provider: Provider) -> Vec<EvidenceCheck> {
    required
        .iter()
        .copied()
        .filter(|check| *check != EvidenceCheck::E2e)
        .filter(|check| provider != Provider::Codex || *check != EvidenceCheck::SubagentReview)
        .collect()
}

/// The `last_error` of a run parked for `missing` evidence, such as
/// `evidence missing: e2e`.
pub fn evidence_missing_reason(missing: &[EvidenceCheck]) -> String {
    let names: Vec<&str> = missing.iter().map(|c| c.as_str()).collect();
    format!("evidence missing: {}", names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(e2e: (&str, &str), subagent_review: (&str, &str)) -> Receipt {
        let check = |(status, evidence): (&str, &str)| serde_json::json!({"status": status, "evidence_or_reason": evidence});
        Receipt::parse(
            &serde_json::json!({
                "run_id": "r",
                "result": "succeeded",
                "commit": "0".repeat(40),
                "tests": check(("passed", "ran")),
                "e2e": check(e2e),
                "subagent_review": check(subagent_review),
            })
            .to_string(),
        )
        .unwrap()
    }

    fn run() -> RunId {
        RunId::new("r").unwrap()
    }

    #[test]
    fn missing_evidence_is_a_required_check_not_passed_with_evidence() {
        let receipt = receipt(("not_applicable", "no surface"), ("passed", " "));
        assert!(receipt.missing_evidence(&[]).is_empty());
        assert!(receipt.missing_evidence(&[EvidenceCheck::Tests]).is_empty());
        // A blank or failed check fails the receipt unless it is required;
        // a required one is left to missing_evidence.
        assert!(receipt.check(&run()).is_err());
        assert!(
            receipt
                .check_requiring(&run(), &[EvidenceCheck::SubagentReview])
                .is_ok()
        );
        let failed = self::receipt(("failed", "no surface"), ("passed", "reviewed"));
        assert_eq!(
            failed.check(&run()).unwrap_err().to_string(),
            "receipt reports e2e as failed: no surface"
        );
        assert!(
            failed
                .check_requiring(&run(), &[EvidenceCheck::Tests])
                .is_err()
        );
        assert!(
            failed
                .check_requiring(&run(), &[EvidenceCheck::E2e])
                .is_ok()
        );
        assert_eq!(
            failed.missing_evidence(&[EvidenceCheck::E2e]),
            [EvidenceCheck::E2e]
        );
        let missing = receipt.missing_evidence(&[
            EvidenceCheck::SubagentReview,
            EvidenceCheck::Tests,
            EvidenceCheck::E2e,
        ]);
        assert_eq!(missing, [EvidenceCheck::SubagentReview, EvidenceCheck::E2e]);
        assert_eq!(
            evidence_missing_reason(&missing),
            "evidence missing: subagent_review, e2e"
        );
    }

    #[test]
    fn no_run_backs_e2e_and_a_codex_run_no_subagent_review() {
        let all = [
            EvidenceCheck::Tests,
            EvidenceCheck::E2e,
            EvidenceCheck::SubagentReview,
        ];
        assert_eq!(
            required_of(&all, Provider::Claude),
            [EvidenceCheck::Tests, EvidenceCheck::SubagentReview]
        );
        assert_eq!(required_of(&all, Provider::Codex), [EvidenceCheck::Tests]);
        // A Codex receipt reports it not_applicable with its reason, and
        // passes with the task's required subagent_review.
        let receipt = receipt(
            ("passed", "cargo test --test e2e"),
            ("not_applicable", "codex worker: no subagent"),
        );
        let required = required_of(&[EvidenceCheck::SubagentReview], Provider::Codex);
        assert!(receipt.check_requiring(&run(), &required).is_ok());
        assert!(receipt.missing_evidence(&required).is_empty());
        assert_eq!(
            receipt.missing_evidence(&[EvidenceCheck::SubagentReview]),
            [EvidenceCheck::SubagentReview]
        );
    }

    /// A follow_up's membership proposal (ADR-t1504-2 decision 11) is
    /// optional: with it, without it, or of an unknown shape, the receipt is
    /// accepted and keeps it as written.
    #[test]
    fn a_follow_up_with_or_without_a_membership_proposal_is_accepted() {
        let proposal = serde_json::json!({"classification": "required", "acceptance_items": ["(1)"], "reason": "r"});
        for follow_ups in [
            serde_json::json!([{"title": "t", "description": "d", "category": "defect"}]),
            serde_json::json!([{"title": "t", "description": "d", "membership_proposal": proposal}]),
            serde_json::json!([{"title": "t", "description": "d", "membership_proposal": "unsure"}]),
        ] {
            let mut file = serde_json::to_value(receipt(
                ("not_applicable", "no surface"),
                ("passed", "reviewed"),
            ))
            .unwrap();
            file["follow_ups"] = follow_ups.clone();
            let parsed = Receipt::parse(&file.to_string()).unwrap();
            assert!(parsed.check(&run()).is_ok(), "{follow_ups}");
            assert_eq!(parsed.follow_ups(), Some(&follow_ups));
        }
    }

    #[test]
    fn parse_reads_the_fields_and_serializes_them_back_in_the_file_shape() {
        let text = r#"{"run_id":"r","result":"failed","commit":"ABC",
            "tests":{"status":"passed","evidence_or_reason":"ran"},
            "e2e":{"status":"not_applicable"},
            "subagent_review":{"status":"failed","evidence_or_reason":"bug"},
            "follow_ups":[{"title":"next"}]}"#;
        let receipt = Receipt::parse(text).unwrap();
        assert_eq!(receipt.run_id(), "r");
        assert_eq!(receipt.result(), ReceiptResult::Failed);
        assert_eq!(receipt.commit(), "ABC");
        assert!(receipt.names_commit("abc"));
        assert!(!receipt.names_commit("abd"));
        assert_eq!(receipt.summary(), "");
        assert_eq!(receipt.tests().status(), CheckStatus::Passed);
        assert_eq!(receipt.tests().evidence_or_reason(), "ran");
        assert_eq!(receipt.e2e().evidence_or_reason(), "");
        assert_eq!(receipt.subagent_review().status(), CheckStatus::Failed);
        assert_eq!(receipt.follow_ups().unwrap()[0]["title"], "next");
        assert_eq!(
            serde_json::to_value(&receipt).unwrap(),
            serde_json::json!({
                "run_id": "r",
                "result": "failed",
                "commit": "ABC",
                "tests": {"status": "passed", "evidence_or_reason": "ran"},
                "e2e": {"status": "not_applicable", "evidence_or_reason": ""},
                "subagent_review": {"status": "failed", "evidence_or_reason": "bug"},
                "summary": "",
                "follow_ups": [{"title": "next"}],
            })
        );
        assert_eq!(
            receipt.into_follow_ups(),
            Some(serde_json::json!([{"title": "next"}]))
        );
        assert!(matches!(
            Receipt::parse(r#"{"run_id":"r"}"#),
            Err(DomainError::MalformedReceipt { .. })
        ));
    }
}
