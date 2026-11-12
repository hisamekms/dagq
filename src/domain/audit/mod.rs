//! The audit record (ADR-t4840-1, ADR-t4840-2): who did what with which
//! authority, where main's code came from, whether the authority's
//! boundaries held, and whether the record itself was changed. It is not an
//! event and is not derived from the events: the use case that changes the
//! state writes both from the same facts of the domain. This module holds
//! the parts that hold for any store: the record's shape ([`record`]), the
//! action enum ([`AuditAction`]), its normalization, hash and the check of a
//! chain ([`chain`]), and the redaction of secrets ([`redact`]). Saving the
//! records is the store's, behind the port of
//! `crate::application::audit`.

pub mod chain;
pub mod record;
pub mod redact;

use std::fmt;

use serde::{Deserialize, Serialize};

use super::DomainError;

pub use chain::{ChainAnchor, ChainMismatch, MismatchKind, canonical_bytes, record_hash, verify};
pub use record::{
    Admission, AnsweredVia, AssuranceClaim, Attestation, AuditRecord, Authority, BlobState,
    ChainedAssurance, Change, Correlation, ExternalAccess, LandingFacts, Outcome, OutcomeStatus,
    Principal, RecordError, Resource, SenderInput, admit,
};
pub use redact::{REDACTED, is_credential_name, redact_value};

/// The version of the record's shape: its fields and what they mean. A
/// field added or changed raises it; it is apart from [`ACTION_VERSION`].
pub const SCHEMA_VERSION: u32 = 1;

/// The version of the action enum: an action added, removed or given a new
/// meaning raises it, apart from the record's [`SCHEMA_VERSION`].
pub const ACTION_VERSION: u32 = 1;

/// A category of what is recorded (ADR-t4840-1): what question a record
/// answers. Stage 0 writes F, D, B's refusals, G and I.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// B: the boundary of authority, a refusal or the grant of a weakening
    /// or privileged operation.
    Boundary,
    /// D: a person's judgment, an answer and whether it was applied.
    Judgment,
    /// F: the provenance of the code on main: what allowed a landing, what
    /// was merged and pushed.
    Provenance,
    /// G: the administration and the settings: the binary, the schema, the
    /// service's lifecycle and the landed changes of the settings.
    Administration,
    /// I: the audit of the audit: the checks of the chain, the changes of
    /// its scope and the corrections.
    AuditOfAudit,
    /// J1: an access to the outside that cannot be taken back (a push).
    ExternalWrite,
}

/// A part of the control side that sends records to the audit service
/// (ADR-t4840-2 decision 1). No AI actor (worker, review, planner, inbox)
/// is one, and none names its own principal: the service takes it from
/// the authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sender {
    /// The use cases of the queue service that change the state, in the
    /// same transaction: the approvals, the answers, the refusals and the
    /// privileged grants, the migrations, the changes of the audit's scope,
    /// the corrections and the service's lifecycle.
    QueueService,
    /// The supervisor, in its calls to the queue service: a landing's
    /// judgment and the handoff of the service's lifecycle.
    Supervisor,
    /// The integrator, in its calls to the queue service: the comparison
    /// with the approval, the merge, the push (waiting for the attempted
    /// record's ack) and the landed changes of the settings.
    Integrate,
    /// `install` and the automatic update, in their calls to the queue
    /// service.
    Updater,
    /// The control side's deterministic job (no AI actor) that checks the
    /// chain and compares main and the remote with the records.
    Reconciliation,
}

impl Sender {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::QueueService => "queue_service",
            Self::Supervisor => "supervisor",
            Self::Integrate => "integrate",
            Self::Updater => "updater",
            Self::Reconciliation => "reconciliation",
        }
    }
}

impl fmt::Display for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An OCSF class an action maps to: a candidate checked when the export is
/// made at stage 2; stage 0 only keeps the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OcsfClass {
    pub uid: u32,
    pub name: &'static str,
}

/// OCSF API Activity.
pub const API_ACTIVITY: OcsfClass = OcsfClass {
    uid: 6003,
    name: "API Activity",
};
/// OCSF Device Config State Change.
pub const CONFIG_STATE_CHANGE: OcsfClass = OcsfClass {
    uid: 5019,
    name: "Device Config State Change",
};
/// OCSF Application Lifecycle.
pub const APPLICATION_LIFECYCLE: OcsfClass = OcsfClass {
    uid: 6002,
    name: "Application Lifecycle",
};

string_enum!(
    /// The action enum, version [`ACTION_VERSION`]: what one record says
    /// happened. Each value's doc says what it means and what its record
    /// carries; [`AuditAction::categories`], [`AuditAction::senders`] and
    /// [`AuditAction::ocsf`] say what it answers, who sends it and its OCSF
    /// class.
    AuditAction {
    /// What allowed a landing: a review's pass, an AI's land, the
    /// convergence after the limit; with the set of the changed files' blob
    /// hashes, the diff hash and the tree hash it was judged on.
    LandingJudged => "landing.judged",
    /// A person's approval of a landing: approve_landing's land answer or
    /// a direct `dagq integrate`; with the set of blob hashes and the path
    /// of the answer.
    LandingApproved => "landing.approved",
    /// The comparison after the merge of the set of blob hashes with the
    /// approved one: whether they matched, and the files that differ.
    LandingCompared => "landing.compared",
    /// The merge onto main: main's sha before and after, the tree hash,
    /// the diff hash, the set of blob hashes, and whether a rebase or an
    /// automatic resolution of a conflict happened.
    LandingMerged => "landing.merged",
    /// A push about to be made: the remote, the ref and its old sha. The
    /// push waits for this record's ack.
    PushAttempted => "push.attempted",
    /// A push's end: completed or failed, with the remote ref's old and
    /// new sha.
    PushCompleted => "push.completed",
    /// An answer to an ask: the hash of the ask's text, the options, the
    /// recommendation and its confidence, the answer and its path.
    AskAnswered => "ask.answered",
    /// Whether an answer was applied, or why not.
    AnswerApplied => "answer.applied",
    /// A refusal of authority: the capability, the rule and the policy's
    /// version. Written fail-open.
    AuthzDenied => "authz.denied",
    /// A grant of a weakening or privileged operation: the guardrail's
    /// disabling, plan review's bypass, a force, a recovery by hand.
    AuthzPrivileged => "authz.privileged",
    /// A landed change of `dagq.toml`, a policy, `.dagq/agents` or the
    /// definition of the protected paths.
    ConfigChanged => "config.changed",
    /// `install` or an automatic update: the versions and digests before
    /// and after, and the changes of protected paths between them.
    BinaryInstalled => "binary.installed",
    /// A migration: the schema's version before and after.
    SchemaMigrated => "schema.migrated",
    /// `up`, `down` and the handoff of an exec.
    ServiceLifecycle => "service.lifecycle",
    /// The reconciliation's result: the chain's check, whether every
    /// commit of main has its F record, whether the push records match the
    /// remote's refs, and how many refusals could not be written.
    AuditChecked => "audit.checked",
    /// A change of the audit's scope or retention: a person's direct
    /// answer only.
    AuditPolicyChanged => "audit.policy_changed",
    /// A correction: a record that cancels the one at its `seq`, as no
    /// record is changed or removed.
    AuditCorrected => "audit.corrected",
});

impl AuditAction {
    /// Every action of version [`ACTION_VERSION`].
    pub const ALL: [Self; 17] = [
        Self::LandingJudged,
        Self::LandingApproved,
        Self::LandingCompared,
        Self::LandingMerged,
        Self::PushAttempted,
        Self::PushCompleted,
        Self::AskAnswered,
        Self::AnswerApplied,
        Self::AuthzDenied,
        Self::AuthzPrivileged,
        Self::ConfigChanged,
        Self::BinaryInstalled,
        Self::SchemaMigrated,
        Self::ServiceLifecycle,
        Self::AuditChecked,
        Self::AuditPolicyChanged,
        Self::AuditCorrected,
    ];

    /// The categories the action's record answers.
    pub const fn categories(self) -> &'static [Category] {
        use Category::*;
        match self {
            Self::LandingJudged | Self::LandingCompared | Self::LandingMerged => &[Provenance],
            Self::LandingApproved => &[Judgment, Provenance],
            Self::PushAttempted | Self::PushCompleted => &[Provenance, ExternalWrite],
            Self::AskAnswered | Self::AnswerApplied => &[Judgment],
            Self::AuthzDenied | Self::AuthzPrivileged => &[Boundary],
            Self::ConfigChanged
            | Self::BinaryInstalled
            | Self::SchemaMigrated
            | Self::ServiceLifecycle => &[Administration],
            Self::AuditChecked | Self::AuditPolicyChanged | Self::AuditCorrected => &[AuditOfAudit],
        }
    }

    /// The senders that may write the action; the audit service refuses
    /// it from any other ([`admit`]).
    pub const fn senders(self) -> &'static [Sender] {
        use Sender::*;
        match self {
            Self::LandingJudged => &[Supervisor],
            Self::LandingCompared
            | Self::LandingMerged
            | Self::PushAttempted
            | Self::PushCompleted
            | Self::ConfigChanged => &[Integrate],
            Self::BinaryInstalled => &[Updater],
            Self::AuditChecked => &[Reconciliation],
            Self::ServiceLifecycle => &[QueueService, Supervisor],
            Self::LandingApproved
            | Self::AskAnswered
            | Self::AnswerApplied
            | Self::AuthzDenied
            | Self::AuthzPrivileged
            | Self::SchemaMigrated
            | Self::AuditPolicyChanged
            | Self::AuditCorrected => &[QueueService],
        }
    }

    /// The OCSF class the action maps to.
    pub const fn ocsf(self) -> OcsfClass {
        match self {
            Self::ConfigChanged => CONFIG_STATE_CHANGE,
            Self::BinaryInstalled | Self::SchemaMigrated | Self::ServiceLifecycle => {
                APPLICATION_LIFECYCLE
            }
            _ => API_ACTIVITY,
        }
    }
}

impl fmt::Display for AuditAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_action_is_listed_once_and_reads_back_from_its_name() {
        let names: BTreeSet<&str> = AuditAction::ALL.iter().map(|a| a.as_str()).collect();
        assert_eq!(names.len(), AuditAction::ALL.len());
        for action in AuditAction::ALL {
            assert_eq!(action.as_str().parse::<AuditAction>().unwrap(), action);
            assert!(!action.categories().is_empty(), "{action}");
            assert!(!action.senders().is_empty(), "{action}");
        }
        assert!("landing.unknown".parse::<AuditAction>().is_err());
    }

    #[test]
    fn the_actions_map_to_the_ocsf_classes_and_senders_of_the_design() {
        assert_eq!(AuditAction::ConfigChanged.ocsf().uid, 5019);
        assert_eq!(AuditAction::BinaryInstalled.ocsf().uid, 6002);
        assert_eq!(AuditAction::SchemaMigrated.ocsf().uid, 6002);
        assert_eq!(AuditAction::ServiceLifecycle.ocsf().uid, 6002);
        assert_eq!(AuditAction::PushAttempted.ocsf().uid, 6003);
        assert_eq!(AuditAction::AuthzDenied.ocsf().name, "API Activity");
        assert_eq!(AuditAction::LandingJudged.senders(), &[Sender::Supervisor]);
        assert_eq!(AuditAction::PushCompleted.senders(), &[Sender::Integrate]);
        assert_eq!(
            AuditAction::AuditChecked.senders(),
            &[Sender::Reconciliation]
        );
        assert_eq!(
            AuditAction::LandingApproved.categories(),
            &[Category::Judgment, Category::Provenance]
        );
        assert!(
            AuditAction::PushAttempted
                .categories()
                .contains(&Category::ExternalWrite)
        );
    }
}
