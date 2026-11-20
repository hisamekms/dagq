//! Current ask recipients and the common answer/close resource rule.
use super::authorization::DenyReason;
use super::goal_owner::Owner;
use super::oidc::OidcIdentity;
use super::queue_service::TeamRole;
use super::{
    ActorContext, ActorRole, AuthorizationError, Authorizer, Capability, Resource, StaticPolicy,
};
use serde::Serialize;

/// Why this person may act. A recipient's reason wins over the admin fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipientReason {
    Owner,
    Delegate,
    Admin,
}

/// The identity and reason added as `human_action` to a human's
/// `ask_answered` or `ask_closed` payload. Absent in local mode and on
/// host/runtime operations; it does not change AnswerAuthority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recipient {
    #[serde(flatten)]
    pub identity: OidcIdentity,
    pub reason: RecipientReason,
}

/// Derive recipients from the current target owner, delegates and explicit
/// escalation rows. Owner/delegate recipients are never snapshotted in
/// ask_recipients: a goal move, owner change or delegate removal must take
/// effect on the next read/answer/close. Only escalation needs stored rows.
/// The caller resolves task/run ownership with goal_owner::task_owner;
/// unrelated finding/request asks supply no owner. Inactive or unregistered
/// people are excluded independently (a disabled owner's delegates remain).
pub fn recipients(
    owner: Option<&Owner>,
    delegates: &[OidcIdentity],
    escalated: &[Recipient],
    mut enabled: impl FnMut(&OidcIdentity) -> bool,
) -> Vec<Recipient> {
    let mut result = Vec::new();
    if let Some(owner) = owner {
        result.push(Recipient {
            identity: owner.identity().clone(),
            reason: RecipientReason::Owner,
        });
        result.extend(delegates.iter().cloned().map(|identity| Recipient {
            identity,
            reason: RecipientReason::Delegate,
        }));
    }
    result.extend_from_slice(escalated);
    let mut seen = std::collections::HashSet::new();
    result.retain(|r| enabled(&r.identity) && seen.insert(r.identity.clone()));
    result
}

/// The authenticated person represented by the caller, including an inbox's
/// human parent. Host socket actors and control actors have no person.
pub fn person(actor: &ActorContext) -> Option<OidcIdentity> {
    match actor.role() {
        ActorRole::User => actor.human_identity(),
        ActorRole::Inbox => actor.parent_identity().cloned(),
        _ => None,
    }
}

/// Shared resource rule for both answer and close. Capabilities still refuse
/// AI actors first; supervisor close and host socket paths retain their
/// grants. Only a configured issuer adds the current-recipient/admin rule.
/// Landing approval's recent direct-human-login rule is applied separately.
pub fn authorize(
    actor: &ActorContext,
    team: Option<bool>,
    capability: Capability,
    current: &[Recipient],
) -> Result<Option<Recipient>, AuthorizationError> {
    StaticPolicy.authorize(actor, capability, &Resource::Queue)?;
    if team != Some(true) {
        return Ok(None);
    }
    let Some(identity) = person(actor) else {
        return Ok(None);
    };
    if let Some(recipient) = current.iter().find(|r| r.identity == identity) {
        return Ok(Some(recipient.clone()));
    }
    if actor
        .human_role()
        .is_some_and(|r| r.role == TeamRole::Admin)
    {
        return Ok(Some(Recipient {
            identity,
            reason: RecipientReason::Admin,
        }));
    }
    Err(AuthorizationError {
        role: actor.role(),
        capability,
        reason: DenyReason::Resource,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::actor::HumanRole;
    use crate::domain::goal_owner::{Membership, task_owner};

    fn identity(subject: &str) -> OidcIdentity {
        OidcIdentity {
            issuer: "https://id.example".into(),
            subject: subject.into(),
        }
    }
    fn actor(subject: &str, admin: bool, inbox: bool) -> ActorContext {
        let role = HumanRole {
            role: if admin {
                TeamRole::Admin
            } else {
                TeamRole::Member
            },
            on_call: false,
        };
        if inbox {
            ActorContext::new(ActorRole::Inbox, "inbox").with_parent_role(identity(subject), role)
        } else {
            ActorContext::new(
                ActorRole::User,
                format!(
                    "human:{}",
                    serde_json::to_string(&identity(subject)).unwrap()
                ),
            )
            .with_human_role(role)
            .with_human_login(true, Some(100))
        }
    }
    fn recipient(subject: &str, reason: RecipientReason) -> Recipient {
        Recipient {
            identity: identity(subject),
            reason,
        }
    }

    #[test]
    fn recipients_follow_current_membership_owner_delegates_and_enabled_people() {
        let goal = Owner::new(identity("goal"));
        let own = Owner::new(identity("own"));
        let moved = Owner::new(identity("moved"));
        // Both task and run asks, including worker questions and landing,
        // share task_owner; ask kind cannot override this rule.
        for (membership, expected) in [
            (Membership::Goal(Some(&goal)), "goal"),
            (Membership::NoGoal, "own"),
            (Membership::Goal(Some(&moved)), "moved"),
        ] {
            assert_eq!(
                recipients(
                    task_owner(membership, Some(&own)).person(),
                    &[identity("proxy")],
                    &[],
                    |_| true
                ),
                vec![
                    recipient(expected, RecipientReason::Owner),
                    recipient("proxy", RecipientReason::Delegate)
                ]
            );
        }
        // Task-less goal asks; owner replacement and delegate removal take
        // effect without changing the ask or any recipient row.
        assert_eq!(
            recipients(Some(&moved), &[], &[], |_| true),
            vec![recipient("moved", RecipientReason::Owner)]
        );
        assert_eq!(
            recipients(
                Some(&goal),
                &[identity("proxy"), identity("disabled")],
                &[],
                |p| p.subject == "proxy"
            ),
            vec![recipient("proxy", RecipientReason::Delegate)]
        );
        assert!(recipients(None, &[identity("proxy")], &[], |_| true).is_empty());
        assert!(
            recipients(
                task_owner(Membership::Goal(None), Some(&own)).person(),
                &[],
                &[],
                |_| true
            )
            .is_empty()
        );
    }

    #[test]
    fn answer_and_close_share_the_current_recipient_admin_and_host_table() {
        let current = vec![
            recipient("owner", RecipientReason::Owner),
            recipient("proxy", RecipientReason::Delegate),
        ];
        for capability in [Capability::AskAnswer, Capability::AskClose] {
            for inbox in [false, true] {
                for (subject, admin, reason) in [
                    ("owner", false, Some(RecipientReason::Owner)),
                    ("proxy", false, Some(RecipientReason::Delegate)),
                    ("other", false, None),
                    ("admin", true, Some(RecipientReason::Admin)),
                ] {
                    let a = actor(subject, admin, inbox);
                    let result = authorize(&a, Some(true), capability, &current);
                    match reason {
                        Some(reason) => assert_eq!(result, Ok(Some(recipient(subject, reason)))),
                        None => assert_eq!(result.unwrap_err().reason, DenyReason::Resource),
                    }
                    // Empty recipients also model a removed proxy at close.
                    let empty = authorize(&a, Some(true), capability, &[]);
                    assert_eq!(empty.is_ok(), admin);
                    for local in [Some(false), None] {
                        assert_eq!(authorize(&a, local, capability, &[]), Ok(None));
                    }
                }
            }
            for host in [
                ActorContext::user(),
                ActorContext::new(ActorRole::Inbox, "inbox"),
            ] {
                assert_eq!(authorize(&host, Some(true), capability, &[]), Ok(None));
            }
            for (subject, reason) in [
                ("owner", RecipientReason::Owner),
                ("proxy", RecipientReason::Delegate),
            ] {
                assert_eq!(
                    authorize(
                        &actor(subject, true, false),
                        Some(true),
                        capability,
                        &current
                    ),
                    Ok(Some(recipient(subject, reason)))
                );
            }
            for role in ActorRole::ALL {
                if matches!(role, ActorRole::User | ActorRole::Inbox) {
                    continue;
                }
                let result = authorize(
                    &ActorContext::new(role, "ai"),
                    Some(true),
                    capability,
                    &current,
                );
                assert_eq!(
                    result.is_ok(),
                    role == ActorRole::Supervisor && capability == Capability::AskClose,
                    "{role:?} {capability:?}"
                );
            }
        }
    }

    #[test]
    fn landing_login_rule_is_not_widened_by_proxy_or_admin() {
        use crate::domain::{AskKind, authorization::AnswerPolicy};
        let policy = AnswerPolicy::new(Some(true), 101);
        let current = vec![recipient("proxy", RecipientReason::Delegate)];
        for (subject, admin) in [("proxy", false), ("admin", true), ("other", false)] {
            for inbox in [false, true] {
                let a = actor(subject, admin, inbox);
                let result = authorize(&a, Some(true), Capability::AskAnswer, &current)
                    .and_then(|_| policy.authorize_answer(&a, &AskKind::ApproveLanding, "land"));
                assert_eq!(result.is_ok(), !inbox && subject != "other");
                let stale = a.with_human_login(true, Some(-100_000));
                assert!(
                    policy
                        .authorize_answer(&stale, &AskKind::ApproveLanding, "land")
                        .is_err()
                );
            }
        }
    }
}
