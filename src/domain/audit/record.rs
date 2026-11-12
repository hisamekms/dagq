//! One audit record (ADR-t4840-2 decisions 2, 3 and 10): what a sender
//! says happened ([`SenderInput`]), what the audit service adds from the
//! authentication and its own order ([`Admission`]), and the record of the
//! chain both make ([`AuditRecord`], made by [`admit`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::chain::{self, CHAIN_VERSION};
use super::redact::redact_value;
use super::{ACTION_VERSION, AuditAction, SCHEMA_VERSION, Sender};

/// Who acted, as the audit service takes it from the authentication the
/// sender received, never from what the sender says. At stage 0 the
/// actor's role and id, as an event's actor; a pseudonymous id from the
/// team version (stage 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub role: String,
    pub id: String,
}

/// How the audit service came to trust what a record says, from the path
/// it came by (ADR-t4840-1): only these go into the chain. What an actor
/// says of itself (`self_reported`) is no [`Attestation`]: it is an
/// [`AssuranceClaim`] that [`AssuranceClaim::chained`] refuses, and goes to
/// the telemetry's own stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attestation {
    /// The control side made it happen: nothing it records could happen
    /// another way.
    Enforced,
    /// The control side saw it happen, outside its own enforcement.
    Observed,
    /// The control side checked it afterwards against another source (a
    /// remote's ref, main's history).
    Verified,
}

/// How sure a fact is, as its path says, before the audit service admits
/// it: a chained [`Attestation`], or what an actor reported of itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssuranceClaim {
    Chained(Attestation),
    /// What an actor (a worker, a session) said of itself: never chained.
    SelfReported,
}

/// What [`AssuranceClaim::chained`] refuses: a self-reported fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotChained;

impl AssuranceClaim {
    /// The attestation a record of the chain carries, or [`NotChained`]
    /// for a self-reported fact.
    pub const fn chained(self) -> Result<Attestation, NotChained> {
        match self {
            Self::Chained(attestation) => Ok(attestation),
            Self::SelfReported => Err(NotChained),
        }
    }
}

/// A record's attestation with the evidence of its path: for a person's
/// direct answer, the controlling terminal, the chain of parent processes,
/// the socket's peer and the time to the answer (ADR-t4840-3 decision 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainedAssurance {
    pub level: Attestation,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub evidence: BTreeMap<String, String>,
}

/// The path a person's judgment (D) came by, from the ask's
/// `AnswerAuthority`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnsweredVia {
    /// A person answered directly, the inbox relaying nothing.
    HumanAttested,
    /// The inbox relayed a person's answer; the record keeps the hash of
    /// the person's words.
    RelayedByInbox,
}

/// What a record acts on: a task, a run, an ask, a commit, a ref or the
/// path of a setting, by its kind and id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    pub kind: String,
    pub id: String,
}

/// Why an action was allowed or refused: the capability, the policy's
/// rule and the policy's version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Authority {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<String>,
}

/// How an action ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Succeeded,
    Failed,
    Denied,
}

/// How an action ended, and why when it did not succeed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub status: OutcomeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What changed: the before and after, or the hash of them when they are
/// large. Both are redacted ([`redact_value`]) before they are recorded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// What ties a record to the rest: the request, the goal, the task, the
/// run and its attempt, the provider's session, the review's job, the
/// `seq` of the record that caused it (the chain's order is the order of
/// admission, its causes are here) and the W3C traceparent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Correlation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_job: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
}

/// A changed file's content after the change, or its removal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobState {
    Blob(String),
    Removed,
}

/// The facts of a landing's judgment and comparison (F, ADR-t4840-2
/// decision 7): each file the diff touches with its blob hash after the
/// change, which the comparison uses, and the diff hash and the tree hash,
/// which are recorded and shown only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandingFacts {
    pub approved_blobs: BTreeMap<String, BlobState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_hash: Option<String>,
}

/// An access to the outside (J): the destination, the direction, the
/// credential by reference, the digest of the content, the way it went and
/// the rule that let it. Stage 0's broker writes none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAccess {
    pub destination: String,
    pub direction: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_rule: Option<String>,
}

/// What a sender says happened: no principal, no order of the chain and
/// no time of admission, which the audit service adds ([`Admission`]).
#[derive(Debug, Clone, PartialEq)]
pub struct SenderInput {
    /// The sender's own counter: with the sender's id, the key that makes
    /// a record's admission idempotent and shows a sender's gaps.
    pub sender_seq: u64,
    /// When the fact happened (UTC, RFC 3339).
    pub at: String,
    pub action: AuditAction,
    pub resource: Resource,
    pub authority: Authority,
    pub outcome: Outcome,
    pub change: Option<Change>,
    pub correlation: Correlation,
    pub answered_via: Option<AnsweredVia>,
    pub landing: Option<LandingFacts>,
    pub external: Option<ExternalAccess>,
}

/// What the audit service adds to a sender's input: its order in the
/// chain, the previous record's hash, when it received it, and what it
/// took from the authentication.
#[derive(Debug, Clone, PartialEq)]
pub struct Admission {
    pub seq: u64,
    pub prev_hash: String,
    /// When the audit service received it (UTC, RFC 3339).
    pub received_at: String,
    /// Where the times came from: the host's clock at stage 0.
    pub clock_source: String,
    pub sender: Sender,
    /// The process of the sender (its pid or instance).
    pub sender_instance: String,
    pub principal: Principal,
    /// The chain of who acts for whom (the inbox for a person, ...).
    pub on_behalf_of: Vec<Principal>,
    /// How the caller was authenticated: the socket's peer, a token, OIDC.
    pub auth: String,
    pub assurance: AssuranceClaim,
    pub evidence: BTreeMap<String, String>,
}

impl Change {
    /// The change as a record keeps it: redacted, and a `null` before or
    /// after as none, which is how a stored record reads back.
    pub fn recorded(self) -> Self {
        let kept = |value: Option<Value>| value.filter(|v| !v.is_null()).map(redact_value);
        Self {
            before: kept(self.before),
            after: kept(self.after),
            digest: self.digest,
        }
    }
}

/// One record of the chain. The fields' meaning is on [`SenderInput`] and
/// [`Admission`]; [`Self::hash`] is the hash of the rest ([`chain`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRecord {
    /// The record's shape's version ([`SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The normalization and the hash's algorithm ([`CHAIN_VERSION`]).
    pub chain_version: String,
    /// The order of admission, from 1: the chain's order.
    pub seq: u64,
    /// The previous record's hash; the version's fixed value for the
    /// first ([`chain::genesis`]).
    pub prev_hash: String,
    /// The SHA-256 of the record but this field, normalized by JCS.
    pub hash: String,
    /// The sender as `<sender>:<instance>`, from the authentication.
    pub sender_id: String,
    pub sender_seq: u64,
    pub at: String,
    pub received_at: String,
    pub clock_source: String,
    pub principal: Principal,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_behalf_of: Vec<Principal>,
    pub auth: String,
    pub action: AuditAction,
    /// The action enum's version ([`ACTION_VERSION`]).
    pub action_version: u32,
    pub resource: Resource,
    pub authority: Authority,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<Change>,
    pub correlation: Correlation,
    pub attestation: ChainedAssurance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_via: Option<AnsweredVia>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<LandingFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalAccess>,
}

/// Why [`admit`] made no record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// A self-reported fact: it goes to the telemetry, not the chain.
    SelfReported,
    /// The action is not one this sender writes ([`AuditAction::senders`]).
    SenderNotAllowed { sender: Sender, action: AuditAction },
}

/// The record of the chain the audit service makes of a sender's input:
/// the before and after redacted, the principal and the order its own,
/// and the hash computed. A self-reported fact and an action the sender
/// does not write are refused.
pub fn admit(input: SenderInput, admission: Admission) -> Result<AuditRecord, RecordError> {
    let level = admission
        .assurance
        .chained()
        .map_err(|_| RecordError::SelfReported)?;
    if !input.action.senders().contains(&admission.sender) {
        return Err(RecordError::SenderNotAllowed {
            sender: admission.sender,
            action: input.action,
        });
    }
    let change = input.change.map(Change::recorded);
    let mut record = AuditRecord {
        schema_version: SCHEMA_VERSION,
        chain_version: CHAIN_VERSION.to_owned(),
        seq: admission.seq,
        prev_hash: admission.prev_hash,
        hash: String::new(),
        sender_id: format!("{}:{}", admission.sender, admission.sender_instance),
        sender_seq: input.sender_seq,
        at: input.at,
        received_at: admission.received_at,
        clock_source: admission.clock_source,
        principal: admission.principal,
        on_behalf_of: admission.on_behalf_of,
        auth: admission.auth,
        action: input.action,
        action_version: ACTION_VERSION,
        resource: input.resource,
        authority: input.authority,
        outcome: input.outcome,
        change,
        correlation: input.correlation,
        attestation: ChainedAssurance {
            level,
            evidence: admission.evidence,
        },
        answered_via: input.answered_via,
        landing: input.landing,
        external: input.external,
    };
    record.hash = chain::record_hash(&record);
    Ok(record)
}

#[cfg(test)]
pub(super) mod fixtures {
    use super::*;

    pub(in super::super) fn input(sender_seq: u64, action: AuditAction) -> SenderInput {
        SenderInput {
            sender_seq,
            at: "2026-11-13T00:00:00Z".to_owned(),
            action,
            resource: Resource {
                kind: "task".to_owned(),
                id: "4841".to_owned(),
            },
            authority: Authority {
                capability: Some("landing.land".to_owned()),
                ..Authority::default()
            },
            outcome: Outcome {
                status: OutcomeStatus::Succeeded,
                reason: None,
            },
            change: None,
            correlation: Correlation {
                task_id: Some(4841),
                ..Correlation::default()
            },
            answered_via: None,
            landing: None,
            external: None,
        }
    }

    pub(in super::super) fn admission(seq: u64, prev_hash: &str, sender: Sender) -> Admission {
        Admission {
            seq,
            prev_hash: prev_hash.to_owned(),
            received_at: "2026-11-13T00:00:01Z".to_owned(),
            clock_source: "host".to_owned(),
            sender,
            sender_instance: "100".to_owned(),
            principal: Principal {
                role: "user".to_owned(),
                id: "user".to_owned(),
            },
            on_behalf_of: Vec::new(),
            auth: "socket_peer".to_owned(),
            assurance: AssuranceClaim::Chained(Attestation::Enforced),
            evidence: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{admission, input};
    use super::*;
    use serde_json::json;

    #[test]
    fn the_audit_service_adds_the_order_the_principal_and_the_hash() {
        let record = admit(
            input(7, AuditAction::LandingApproved),
            admission(3, &chain::genesis(), Sender::QueueService),
        )
        .unwrap();
        assert_eq!(record.seq, 3);
        assert_eq!(record.sender_id, "queue_service:100");
        assert_eq!(record.sender_seq, 7);
        assert_eq!(record.received_at, "2026-11-13T00:00:01Z");
        assert_eq!(record.principal.role, "user");
        assert_eq!(record.schema_version, SCHEMA_VERSION);
        assert_eq!(record.action_version, ACTION_VERSION);
        assert_eq!(record.chain_version, "v1");
        assert_eq!(record.hash, chain::record_hash(&record));
        assert_eq!(record.hash.len(), 64);
    }

    #[test]
    fn a_self_reported_fact_never_becomes_a_record_of_the_chain() {
        assert_eq!(AssuranceClaim::SelfReported.chained(), Err(NotChained));
        assert_eq!(
            AssuranceClaim::Chained(Attestation::Observed).chained(),
            Ok(Attestation::Observed)
        );
        let mut by = admission(1, &chain::genesis(), Sender::QueueService);
        by.assurance = AssuranceClaim::SelfReported;
        assert_eq!(
            admit(input(1, AuditAction::AskAnswered), by),
            Err(RecordError::SelfReported)
        );
        // Nor does a stored record read back with it.
        assert!(serde_json::from_value::<Attestation>(json!("self_reported")).is_err());
        for (level, name) in [
            (Attestation::Enforced, "enforced"),
            (Attestation::Observed, "observed"),
            (Attestation::Verified, "verified"),
        ] {
            assert_eq!(serde_json::to_value(level).unwrap(), json!(name));
        }
    }

    #[test]
    fn an_action_is_admitted_only_from_a_sender_that_writes_it() {
        assert_eq!(
            admit(
                input(1, AuditAction::PushAttempted),
                admission(1, &chain::genesis(), Sender::QueueService),
            ),
            Err(RecordError::SenderNotAllowed {
                sender: Sender::QueueService,
                action: AuditAction::PushAttempted,
            })
        );
        assert!(
            admit(
                input(1, AuditAction::PushAttempted),
                admission(1, &chain::genesis(), Sender::Integrate),
            )
            .is_ok()
        );
    }

    #[test]
    fn a_null_before_reads_back_with_the_same_hash() {
        let mut with = input(1, AuditAction::ConfigChanged);
        with.change = Some(Change {
            before: Some(Value::Null),
            after: Some(json!({"a": 1})),
            digest: None,
        });
        let record = admit(with, admission(1, &chain::genesis(), Sender::Integrate)).unwrap();
        let read: AuditRecord =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        assert_eq!(read, record);
        assert_eq!(chain::record_hash(&read), record.hash);
    }
}
