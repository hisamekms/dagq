//! The facts of the administration and the settings (category G of
//! ADR-t4840-1) as the audit service records them, and the judgments the
//! control side makes before it sends them, all without side effects:
//!
//! - [`binary_installed`]: an automatic or release update (and a person's
//!   `install`) put a binary in place: the versions and the binaries'
//!   SHA-256 digests before and after, the result of its checks, and the
//!   changes of the protected paths between the two builds
//!   ([`BinaryChange::protected_paths`], left empty until the comparison
//!   of the protected paths fills it).
//! - [`schema_migrated`]: the queue's schema version before and after a
//!   migration and the versions it applied.
//! - [`service_lifecycle`]: `up` or `down` (who is the authenticated
//!   principal of the record), with the hash of `dagq.toml` it ran with
//!   (and of the `dagq.local.toml` used over it).
//! - [`settings_changed`]: the settings no command changes, so the
//!   supervisor observes them every pass ([`SettingsSnapshot`]) and the
//!   record is `observed`: the hash of each observed path before and
//!   after, and the paths that changed. The observed paths are
//!   [`SETTINGS_FILE`], every file under [`AGENTS_DIR`], the compiled
//!   authorization policy ([`POLICY_KEY`]) and, while it is used,
//!   [`LOCAL_SETTINGS_FILE`].
//! - [`verify_due`]: a change of the DB that went around the CLI (and so
//!   around the audit service) is found afterwards by the supervisor's
//!   periodic check of the chain, every [`CHAIN_VERIFY_PERIOD`];
//!   [`mismatch_is_new`] keeps one standing mismatch to one attention.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::AuditAction;
use super::record::{
    Authority, Change, Correlation, Outcome, OutcomeStatus, Resource, SenderInput,
};

/// How often a supervisor at work checks the whole chain of the audit, as
/// `dagq audit verify` does: one hour. Landings come a few an hour, and a
/// check every hour shows a change made around the CLI within the same
/// interval as the hourly review of the watch. A check reads every record
/// of the chain, so a shorter period would cost the queue reads for no
/// earlier person's look.
pub const CHAIN_VERIFY_PERIOD: Duration = Duration::from_secs(60 * 60);

/// A verification's bounded result and the latest check event in its
/// snapshot. The short write phase rejects results superseded since then.
pub type AuditCheck = (Value, Option<i64>);

/// Whether the periodic check of the chain is due at `now`, the last one
/// having started at `last` (`None`: none since the supervisor started,
/// so the first pass checks). A clock that went back checks now.
pub fn verify_due(last: Option<Duration>, now: Duration) -> bool {
    last.is_none_or(|last| now < last || now - last >= CHAIN_VERIFY_PERIOD)
}

/// The records of the periodic check, newest first deciding: the
/// attention [`crate::domain::AttentionNext::VerifyAudit`] stands while the
/// newest of them is a mismatch.
pub const CHECK_KINDS: [crate::domain::EventKind; 2] = [
    crate::domain::EventKind::SupervisorAuditMismatch,
    crate::domain::EventKind::SupervisorAuditVerified,
];

/// Whether a mismatch `found` (`dagq audit verify`'s answer) opens an
/// attention, `standing` being the mismatch of the attention that stands
/// (`None` when none does): a mismatch of the same record and kind is the
/// same, already shown; a chain that verifies opens none.
pub fn mismatch_is_new(standing: Option<&Value>, found: &Value) -> bool {
    if found["audit_verified"] != json!(false) {
        return false;
    }
    standing
        .is_none_or(|standing| standing["seq"] != found["seq"] || standing["kind"] != found["kind"])
}

/// Whether a check's answer may be applied (open or close the attention),
/// `seen` being the id of the newest of the check's records
/// ([`CHECK_KINDS`]) in the snapshot the check verified and `newest` that
/// id when the answer is written: a check that another wrote after has an
/// older snapshot's answer, which neither closes the newer's mismatch nor
/// opens one over it.
pub fn check_applies(seen: Option<i64>, newest: Option<i64>) -> bool {
    seen == newest
}

/// The repository's settings file the supervisor observes.
pub const SETTINGS_FILE: &str = "dagq.toml";
/// The uncommitted layer over [`SETTINGS_FILE`] (ADR-t5108-1), in a
/// [`SettingsSnapshot`] only while it is used, so its hash names the
/// values in effect.
pub const LOCAL_SETTINGS_FILE: &str = "dagq.local.toml";
/// The directory of the review agents' definitions the supervisor
/// observes, every file under it.
pub const AGENTS_DIR: &str = ".dagq/agents";
/// The key of the compiled authorization policy's hash in a
/// [`SettingsSnapshot`] (`crate::application::audit::policy_version`).
pub const POLICY_KEY: &str = "authorization_policy";

/// The settings as one pass observed them: each observed path (relative
/// to the repository, or [`POLICY_KEY`]) with the SHA-256 of its content.
/// A path missing from it was not there.
pub type SettingsSnapshot = BTreeMap<String, String>;

/// The record of the settings that changed from `before` to `after`, or
/// `None` when they are the same. `before` is the snapshot of the last
/// such record (`None` before the first, whose record starts the
/// observation with no `before`). The record names each path whose hash
/// changed, appeared or went (`paths`), with both snapshots.
pub fn settings_changed(
    before: Option<&SettingsSnapshot>,
    after: &SettingsSnapshot,
) -> Option<SenderInput> {
    if before == Some(after) {
        return None;
    }
    let empty = SettingsSnapshot::new();
    let old = before.unwrap_or(&empty);
    let paths: BTreeSet<&String> = old
        .keys()
        .chain(after.keys())
        .filter(|path| old.get(*path) != after.get(*path))
        .collect();
    Some(input(
        AuditAction::ConfigChanged,
        Resource {
            kind: "settings".into(),
            id: "repository".into(),
        },
        Change {
            before: before.map(|before| json!({"hashes": before})),
            after: Some(json!({"hashes": after, "paths": paths})),
            digest: None,
        },
    ))
}

/// The snapshot a [`settings_changed`] record holds after the change.
pub fn snapshot_of(change: Option<&Change>) -> Option<SettingsSnapshot> {
    let hashes = change?.after.as_ref()?.get("hashes")?;
    serde_json::from_value(hashes.clone()).ok()
}

/// A binary put in place, as its update or install found it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryChange {
    /// What put it: `commit` (main's build), `release` or `install`.
    pub source: String,
    /// The commit or the release it was built from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<String>,
    pub version_before: Option<String>,
    pub version_after: Option<String>,
    /// The SHA-256 of the binary it replaced, `None` when there was none.
    pub digest_before: Option<String>,
    pub digest_after: Option<String>,
    /// The checks it passed before it was put in place (the e2e gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
    /// The protected paths that changed between the two builds; empty
    /// until the comparison of the protected paths records them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protected_paths: Vec<String>,
    /// How an install that failed left the binary in place: a partial
    /// handoff, a failed restore, an unsettled handoff or another failure
    /// after replacement. `None` when the install succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

/// The audit's allowlisted gate information. Paths, test names, cleanup
/// output and free-form errors stay in operational reports/logs. Only a
/// recognized status, elapsed seconds and fixed warning codes are sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    pub status: VerificationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<VerificationIssue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Passed,
    Skipped,
    NotApplicable,
    Unrecognized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationIssue {
    InvalidQuarantine,
    RerunError,
}

impl Verification {
    fn of(report: &Value) -> Option<Self> {
        if report.is_null() {
            return None;
        }
        let status = match report["status"].as_str() {
            Some("passed") => VerificationStatus::Passed,
            Some("skipped") => VerificationStatus::Skipped,
            Some("not_applicable") => VerificationStatus::NotApplicable,
            _ => VerificationStatus::Unrecognized,
        };
        let mut issues = Vec::new();
        if !report["quarantine"]["error"].is_null() {
            issues.push(VerificationIssue::InvalidQuarantine);
        }
        if !report["rerun"]["error"].is_null() {
            issues.push(VerificationIssue::RerunError);
        }
        Some(Self {
            status,
            secs: report["secs"].as_u64(),
            issues,
        })
    }
}

/// The [`BinaryChange`] of an update's `update_installed` payload or of an
/// install's report (`version`, `digest`, `replaced_binary`, `e2e`,
/// `commit` or `release`, and `kept_outcome` of one that stayed in place
/// though not every supervisor took it); `None` for one that put only the
/// plugin in place, which changes no binary. The version and the digest
/// before are both of the one binary it replaced (`replaced_binary`, read
/// from the same file); a report without it gives none, never the version
/// of one file with the digest of another.
pub fn binary_change_of_update(payload: &Value) -> Option<BinaryChange> {
    if payload["plugin_only"] == json!(true) {
        return None;
    }
    let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_owned);
    let replaced = |key: &str| {
        payload["replaced_binary"]
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let (source, of) = match (text("commit"), text("release")) {
        (Some(commit), _) => ("commit", Some(commit)),
        (None, Some(release)) => ("release", Some(release)),
        (None, None) => ("install", None),
    };
    Some(BinaryChange {
        source: source.into(),
        of,
        version_before: replaced("version"),
        version_after: text("version"),
        digest_before: replaced("digest"),
        digest_after: text("digest"),
        verification: Verification::of(&payload["e2e"]),
        protected_paths: Vec::new(),
        outcome: text("kept_outcome"),
    })
}

/// The record of a binary put in place: succeeded, with the reason of
/// one that not every supervisor took ([`BinaryChange::outcome`]).
pub fn binary_installed(change: &BinaryChange) -> SenderInput {
    let after = serde_json::to_value(change).unwrap_or(Value::Null);
    let mut record = input(
        AuditAction::BinaryInstalled,
        Resource {
            kind: "binary".into(),
            id: "dagq".into(),
        },
        Change {
            before: Some(json!({
                "version": change.version_before,
                "digest": change.digest_before,
            })),
            after: Some(after),
            digest: None,
        },
    );
    record.outcome.reason = change.outcome.clone();
    record
}

/// The record of a migration of the queue's schema from `before` to
/// `after`, the versions `applied`.
pub fn schema_migrated(before: i64, after: i64, applied: &[i64]) -> SenderInput {
    input(
        AuditAction::SchemaMigrated,
        Resource {
            kind: "queue".into(),
            id: "schema".into(),
        },
        Change {
            before: Some(json!({"schema_version": before})),
            after: Some(json!({"schema_version": after, "applied": applied})),
            digest: None,
        },
    )
}

/// What a lifecycle command of the service did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleOperation {
    Up,
    Down,
}

/// Safe lifecycle failure reasons sent to the audit. Detailed errors may
/// contain a child program's credentials and belong only in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleFailure {
    OperationFailed,
}

impl LifecycleFailure {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OperationFailed => "operation_failed",
        }
    }
}

/// The record of `up` once it ended, or `down` before its receiver may
/// stop; a later failure goes only to the log, never a conflicting second
/// record. `error` is a fixed failure code, never a child's stderr or an
/// error chain. Uses settings whose `dagq.toml` hashes to `config_hash`
/// (`None` when the bound repository has no main checkout to read it
/// from), and, while the main checkout's `dagq.local.toml` is used over it
/// (ADR-t5108-1), that file's hash as `local_config_hash`.
pub fn service_lifecycle(
    operation: LifecycleOperation,
    error: Option<LifecycleFailure>,
    config_hash: Option<&str>,
    local_config_hash: Option<&str>,
) -> SenderInput {
    let mut record = input(
        AuditAction::ServiceLifecycle,
        Resource {
            kind: "service".into(),
            id: "queue".into(),
        },
        Change {
            before: None,
            after: Some(json!({"operation": operation, "config_hash": config_hash})),
            digest: None,
        },
    );
    if let Some(local) = local_config_hash
        && let Some(after) = record
            .change
            .as_mut()
            .and_then(|change| change.after.as_mut())
    {
        after["local_config_hash"] = json!(local);
    }
    if let Some(error) = error {
        record.outcome = Outcome {
            status: OutcomeStatus::Failed,
            reason: Some(error.as_str().to_owned()),
        };
    }
    record
}

/// A fact the control side sends to the audit service of the
/// administration: what the service admits as the authenticated
/// principal's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdministrationFact {
    /// A binary put in place: a person's `install`, and an update's that
    /// not every supervisor took but stayed (an update every supervisor
    /// took is recorded with its `update_installed`).
    Installed(Box<BinaryChange>),
    /// A lifecycle fact with a fixed failure code (`error`); details stay
    /// in the sender's log.
    Lifecycle {
        operation: LifecycleOperation,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<LifecycleFailure>,
    },
}

/// A succeeded record of `action` on `resource` with `change`; the
/// sender's counter and time are the store's to set.
fn input(action: AuditAction, resource: Resource, change: Change) -> SenderInput {
    SenderInput {
        sender_seq: 0,
        at: String::new(),
        action,
        resource,
        authority: Authority::default(),
        outcome: Outcome {
            status: OutcomeStatus::Succeeded,
            reason: None,
        },
        change: Some(change),
        correlation: Correlation::default(),
        answered_via: None,
        landing: None,
        external: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_accepts_only_fixed_statuses_numbers_and_issue_codes() {
        let change = binary_change_of_update(&json!({"e2e": {
            "status": "access_token=synthetic", "secs": -1, "log": "secret",
            "quarantine": {"error": "secret"}, "rerun": {"error": "secret"},
            "flaky": ["secret"], "unknown": {"credential": "secret"}
        }}))
        .unwrap();
        assert_eq!(
            serde_json::to_value(change.verification).unwrap(),
            json!({
                "status": "unrecognized", "issues": ["invalid_quarantine", "rerun_error"]
            })
        );
    }

    fn snapshot(pairs: &[(&str, &str)]) -> SettingsSnapshot {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_same_settings_record_nothing_and_a_change_names_its_paths() {
        let before = snapshot(&[
            ("dagq.toml", "a"),
            (".dagq/agents/x.md", "b"),
            (POLICY_KEY, "p"),
        ]);
        assert!(settings_changed(Some(&before), &before.clone()).is_none());
        let after = snapshot(&[
            ("dagq.toml", "c"),
            (".dagq/agents/y.md", "d"),
            (POLICY_KEY, "p"),
        ]);
        let record = settings_changed(Some(&before), &after).unwrap();
        assert_eq!(record.action, AuditAction::ConfigChanged);
        let change = record.change.unwrap();
        assert_eq!(change.before, Some(json!({"hashes": before})));
        assert_eq!(
            change.after.as_ref().unwrap()["paths"],
            json!([".dagq/agents/x.md", ".dagq/agents/y.md", "dagq.toml"])
        );
        assert_eq!(
            snapshot_of(Some(&change)),
            Some(after.clone()),
            "the next pass compares with what this record holds"
        );
    }

    #[test]
    fn the_first_observation_starts_with_no_before() {
        let after = snapshot(&[("dagq.toml", "a")]);
        let change = settings_changed(None, &after).unwrap().change.unwrap();
        assert_eq!(change.before, None);
        assert_eq!(change.after.unwrap()["paths"], json!(["dagq.toml"]));
        assert_eq!(snapshot_of(None), None);
    }

    #[test]
    fn the_chain_is_checked_first_and_then_every_hour() {
        let hour = Duration::from_secs(3600);
        assert_eq!(CHAIN_VERIFY_PERIOD, hour);
        assert!(verify_due(None, Duration::ZERO));
        let last = Duration::from_secs(10_000);
        assert!(!verify_due(Some(last), last));
        assert!(!verify_due(
            Some(last),
            last + hour - Duration::from_secs(1)
        ));
        assert!(verify_due(Some(last), last + hour));
        assert!(verify_due(Some(last), last - Duration::from_secs(1)));
    }

    #[test]
    fn a_check_applies_only_on_the_records_it_saw() {
        assert!(check_applies(None, None));
        assert!(check_applies(Some(4), Some(4)));
        // Another check recorded after this one's snapshot.
        assert!(!check_applies(None, Some(5)));
        assert!(!check_applies(Some(4), Some(5)));
    }

    #[test]
    fn one_standing_mismatch_is_one_attention() {
        let ok = json!({"audit_verified": true, "checked": 3});
        let bad = json!({"audit_verified": false, "seq": 2, "kind": "Hash"});
        assert!(!mismatch_is_new(None, &ok));
        assert!(mismatch_is_new(None, &bad));
        assert!(!mismatch_is_new(Some(&bad), &bad));
        let other = json!({"audit_verified": false, "seq": 3, "kind": "Hash"});
        assert!(mismatch_is_new(Some(&bad), &other));
    }

    #[test]
    fn an_update_records_versions_digests_and_checks_but_not_a_plugin_only_one() {
        let payload = json!({
            "version": "0.2.0", "previous_version": "0.1.0",
            "digest": "new", "replaced_binary": {"version": "0.1.0", "digest": "old"},
            "commit": "abc", "e2e": {"status": "passed"},
        });
        let change = binary_change_of_update(&payload).unwrap();
        assert_eq!(change.source, "commit");
        assert_eq!(change.of.as_deref(), Some("abc"));
        let record = binary_installed(&change);
        assert_eq!(record.action, AuditAction::BinaryInstalled);
        let recorded = record.change.unwrap();
        assert_eq!(
            recorded.before,
            Some(json!({"version": "0.1.0", "digest": "old"}))
        );
        let after = recorded.after.unwrap();
        assert_eq!(after["version_after"], "0.2.0");
        assert_eq!(after["digest_after"], "new");
        assert_eq!(after["verification"], json!({"status": "passed"}));
        assert!(after.get("protected_paths").is_none());
        // The version and the digest before come from the one binary.
        let mixed = binary_change_of_update(&json!({
            "version": "0.2.0", "previous_version": "0.2.0", "previous_digest": "old",
        }))
        .unwrap();
        assert_eq!((mixed.version_before, mixed.digest_before), (None, None));
        let kept = binary_change_of_update(&json!({
            "version": "0.2.0", "kept_outcome": "partially_handed_off",
        }))
        .unwrap();
        let record = binary_installed(&kept);
        assert_eq!(record.outcome.status, OutcomeStatus::Succeeded);
        assert_eq!(
            record.outcome.reason.as_deref(),
            Some("partially_handed_off")
        );
        let release = binary_change_of_update(&json!({"release": "0.3.0", "version": "0.3.0"}));
        assert_eq!(release.unwrap().source, "release");
        assert!(
            binary_change_of_update(&json!({"plugin_only": true, "version": "0.2.0"})).is_none()
        );
    }

    #[test]
    fn a_migration_and_a_lifecycle_record_their_versions_and_settings() {
        let migrated = schema_migrated(14, 16, &[15, 16]).change.unwrap();
        assert_eq!(migrated.before, Some(json!({"schema_version": 14})));
        assert_eq!(
            migrated.after,
            Some(json!({"schema_version": 16, "applied": [15, 16]}))
        );
        let up = service_lifecycle(LifecycleOperation::Up, None, Some("h"), None);
        assert_eq!(up.action, AuditAction::ServiceLifecycle);
        assert_eq!(up.outcome.status, OutcomeStatus::Succeeded);
        let failed = service_lifecycle(
            LifecycleOperation::Down,
            Some(LifecycleFailure::OperationFailed),
            None,
            None,
        );
        assert_eq!(failed.outcome.status, OutcomeStatus::Failed);
        assert_eq!(failed.outcome.reason.as_deref(), Some("operation_failed"));
        assert_eq!(
            up.change.unwrap().after,
            Some(json!({"operation": "up", "config_hash": "h"}))
        );
        // A used dagq.local.toml's hash joins the dagq.toml's.
        let layered = service_lifecycle(LifecycleOperation::Up, None, Some("h"), Some("l"));
        assert_eq!(
            layered.change.unwrap().after,
            Some(json!({"operation": "up", "config_hash": "h", "local_config_hash": "l"}))
        );
    }
}
