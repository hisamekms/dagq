//! Weakening operations require a directly attested human decision. Host
//! evidence is advisory: an agent with the same UID can forge a terminal.
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::authorization::{AuthorizationError, DenyReason};
use super::{ActorContext, ActorRole, Ask, AskKind, Capability};

/// Stored ask subject used by the protected-path approval producer. An
/// ordinary approve_landing ask has no such subject. The producer, rather
/// than answer text or client arguments, decides this classification.
pub const PROTECTED_PATHS_SUBJECT: &str = "protected_paths";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeakeningOperation {
    GuardrailDisable,
    PlanReviewBypass,
    Force,
    AuditPolicyChange,
    ProtectedLanding,
}

impl WeakeningOperation {
    pub const fn capability(self) -> Capability {
        match self {
            Self::PlanReviewBypass => Capability::TaskReadyBypassReview,
            Self::ProtectedLanding => Capability::AskAnswer,
            Self::AuditPolicyChange => Capability::QueueAdmin,
            Self::GuardrailDisable | Self::Force => Capability::ServiceLifecycle,
        }
    }
}

/// The weakening part of a service request, independent of effects. Audit
/// policy changes have no CLI yet; their future entry must use this policy.
pub fn operation(use_case: &str, raw: &Value) -> Option<WeakeningOperation> {
    use WeakeningOperation::*;
    match use_case {
        "ready" if raw["bypass_review"] == true => Some(PlanReviewBypass),
        "ready_inherit" | "ready_inherit_read" | "ready_inherit_images" => Some(PlanReviewBypass),
        "issue_token" if guardrail_off(&raw["opened"]) => Some(GuardrailDisable),
        "control" | "supervisor" => {
            let call = if use_case == "control" {
                &raw["call"]
            } else {
                raw
            };
            if call["op"] != "audit_privileged_operation" {
                return None;
            }
            match call["args"][0].as_str()? {
                "down_force" => Some(Force),
                "guardrail_disable" => Some(GuardrailDisable),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether an inbox's launch (`opened`, none for a renewal) goes without
/// its guardrail, by the judgment `status` shows, so a launch that omits
/// or misstates its `guardrail` is classified as `status` reads it.
pub fn guardrail_off(opened: &Value) -> bool {
    !opened.is_null() && super::sessions::inbox_guardrail_by(opened).is_none()
}

pub fn answer_operation(ask: &Ask) -> Option<WeakeningOperation> {
    (ask.kind == AskKind::ApproveLanding && ask.subject.as_deref() == Some(PROTECTED_PATHS_SUBJECT))
        .then_some(WeakeningOperation::ProtectedLanding)
}

/// The host CLI's observation, bound to the exact request (including the
/// ask identity and answer). Arguments/command lines are deliberately absent
/// from the parent chain to avoid retaining secrets. A human_attested
/// record keeps it as the route's evidence: the controlling terminal's
/// name, the caller's parent process chain, the socket peer the service
/// stamps and the time the confirmation took. An agent with the same UID
/// can forge all of it, so it serves checking afterwards, not prevention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub target: String,
    pub confirmation: String,
    pub terminal: String,
    pub role_env_absent: bool,
    pub parent_chain: Vec<u32>,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub socket_peer: Option<String>,
}

/// Strip the transport envelope before strict command deserialization.
/// The envelope itself never supplies an actor or a deployment mode.
pub fn extract(raw: &Value) -> (Value, Option<Evidence>) {
    let mut params = raw.clone();
    let evidence = params
        .as_object_mut()
        .and_then(|p| p.remove("_direct_human"))
        .and_then(|v| serde_json::from_value(v).ok());
    (params, evidence)
}

pub fn target(raw: &Value) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(raw.to_string().as_bytes()))
}

pub fn host_evidence_valid(evidence: &Evidence, raw: &Value) -> bool {
    let target = target(raw);
    evidence.role_env_absent
        && !evidence.terminal.is_empty()
        && evidence.target == target
        && evidence.confirmation == target[..12]
        && !evidence.parent_chain.is_empty()
}

/// Team mode uses the existing recent OIDC authentication; the host uses a
/// controlling TTY. Neither delegated AnswerAuthority nor an inbox role can
/// qualify, even if supplied with plausible host evidence.
pub fn permitted(actor: &ActorContext, team: Option<bool>, now: i64) -> bool {
    if actor.role() != ActorRole::User {
        return false;
    }
    match team {
        Some(false) => actor.direct_human().is_some(),
        None => actor.direct_human().is_some() || actor.direct_decision_checked(),
        Some(true) => {
            let (human, at) = actor.human_login();
            human
                && super::queue_service::approval_login(at, now)
                    .approval_until
                    .is_some()
        }
    }
}

pub fn authorize(
    actor: &ActorContext,
    op: WeakeningOperation,
    team: Option<bool>,
    now: i64,
) -> Result<(), AuthorizationError> {
    if permitted(actor, team, now) {
        Ok(())
    } else {
        Err(AuthorizationError {
            role: actor.role(),
            capability: op.capability(),
            reason: DenyReason::DirectHuman,
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    pub(crate) fn evidence(raw: &Value) -> Evidence {
        let hash = target(raw);
        Evidence {
            confirmation: hash[..12].into(),
            target: hash,
            terminal: "/dev/ttys009".into(),
            role_env_absent: true,
            parent_chain: vec![31, 17, 1],
            elapsed_ms: 400,
            socket_peer: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::evidence;
    use super::*;
    use serde_json::json;

    #[test]
    fn host_route_requires_the_tty_reply_role_absence_and_exact_target() {
        let raw = json!({"id": 7});
        assert_eq!(
            target(&raw),
            "a3c90e3b7448d23d9eacebd0ebf15cae100e21f9b2c688f3f9d238edcd26d67f"
        );
        let good = evidence(&raw);
        assert!(host_evidence_valid(&good, &raw));
        assert!(!host_evidence_valid(&good, &json!({"id": 8})));
        for bad in [
            Evidence {
                terminal: String::new(),
                ..good.clone()
            },
            Evidence {
                role_env_absent: false,
                ..good.clone()
            },
            Evidence {
                confirmation: "yes".into(),
                ..good.clone()
            },
            Evidence {
                parent_chain: Vec::new(),
                ..good.clone()
            },
        ] {
            assert!(!host_evidence_valid(&bad, &raw));
        }
    }

    #[test]
    fn every_weakening_operation_refuses_delegation_and_requires_direct_evidence() {
        let direct = ActorContext::user().with_direct_human(Some(evidence(&json!({"id": 7}))));
        for op in [
            WeakeningOperation::GuardrailDisable,
            WeakeningOperation::PlanReviewBypass,
            WeakeningOperation::Force,
            WeakeningOperation::AuditPolicyChange,
            WeakeningOperation::ProtectedLanding,
        ] {
            assert!(authorize(&direct, op, Some(false), 100).is_ok());
            for role in [ActorRole::Inbox, ActorRole::Worker, ActorRole::User] {
                let actor = ActorContext::new(role, "caller");
                let error = authorize(&actor, op, Some(false), 100).unwrap_err();
                assert_eq!(error.reason, DenyReason::DirectHuman);
                assert!(
                    error
                        .to_string()
                        .contains("separate terminal with no DAGQ_ROLE")
                );
            }
            let forged = ActorContext::new(ActorRole::Inbox, "inbox")
                .with_direct_human(direct.direct_human().cloned());
            assert!(authorize(&forged, op, Some(false), 100).is_err());
        }
        // A local proof never replaces team authentication.
        assert!(!permitted(&direct, Some(true), 100));
        let oidc = ActorContext::user().with_human_login(true, Some(100));
        assert!(permitted(&oidc, Some(true), 100));
        assert!(!permitted(&oidc, Some(true), 10000));
    }

    #[test]
    fn request_classification_is_about_the_operation_not_arbitrary_force_words() {
        use WeakeningOperation::*;
        for (use_case, raw, expected) in [
            (
                "ready",
                json!({"id": 7, "bypass_review": true}),
                Some(PlanReviewBypass),
            ),
            ("ready", json!({"id": 7, "bypass_review": false}), None),
            (
                "control",
                json!({"call": {"op": "audit_privileged_operation", "args": ["down_force"]}}),
                Some(Force),
            ),
            (
                "control",
                json!({"call": {"op": "audit_privileged_operation", "args": ["guardrail_disable"]}}),
                Some(GuardrailDisable),
            ),
            (
                "issue_token",
                json!({"opened": {"guardrail": false}}),
                Some(GuardrailDisable),
            ),
            // As `status` judges it, not as the launch says.
            ("issue_token", json!({"opened": {}}), Some(GuardrailDisable)),
            (
                "issue_token",
                json!({"opened": {"guardrail": true, "provider": "codex", "hooks": []}}),
                Some(GuardrailDisable),
            ),
            (
                "issue_token",
                json!({"opened": {"provider": "claude", "settings": "/s.json"}}),
                None,
            ),
            (
                "issue_token",
                json!({"opened": {"provider": "codex", "hooks": [{"hook": "pre_tool_use"}]}}),
                None,
            ),
            ("issue_token", json!({"principal": {}}), None),
            ("note", json!({"text": "force guardrail_disable"}), None),
        ] {
            assert_eq!(operation(use_case, &raw), expected);
        }
    }
}
