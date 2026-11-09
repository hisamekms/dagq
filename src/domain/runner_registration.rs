//! Runner credentials belong to an active human registry entry. All decisions
//! take stored facts and Unix seconds; no clock or credential value lives here.
use super::{
    oidc::OidcIdentity,
    queue_service::{Member, TeamRole},
};
use serde::{Deserialize, Serialize};

/// Enrollment codes and access tokens last fifteen minutes. Refresh credentials
/// last thirty days and rotate on every use. Values are 256 random bits returned
/// only in responses; persistence receives their SHA-256 hashes. The caller
/// transfers an enrollment code to its runner; this service writes no credentials.
pub const SHORT_SECONDS: i64 = 15 * 60;
pub const REFRESH_SECONDS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Runner {
    pub id: String,
    pub owner: OidcIdentity,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
    pub declaration: Option<Declaration>,
    pub shared: bool,
    pub approved_by: Option<OidcIdentity>,
}

/// Scheduling hints only: Linux/macOS/Windows, x86_64/aarch64, container
/// availability, and Claude credential *kinds*, never credential values.
/// At most two distinct kinds (`team_api_key`, brokered by the control side,
/// and `owner_subscription`) and 1024 serialized bytes are accepted. Unknown
/// fields are refused, including attempts to send a credential value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    pub os: String,
    pub arch: String,
    pub container: bool,
    pub credentials: Vec<String>,
}
impl Declaration {
    pub fn valid(&self) -> bool {
        matches!(self.os.as_str(), "linux" | "macos" | "windows")
            && matches!(self.arch.as_str(), "x86_64" | "aarch64")
            && self.credentials.len() <= 2
            && self
                .credentials
                .iter()
                .all(|v| matches!(v.as_str(), "team_api_key" | "owner_subscription"))
            && (self.credentials.len() < 2 || self.credentials[0] != self.credentials[1])
    }
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        if serde_json::to_vec(value).ok()?.len() > 1024 {
            return None;
        }
        serde_json::from_value::<Self>(value.clone())
            .ok()
            .filter(Self::valid)
    }
}

/// Sharing is a registry approval, independent of later declarations. Removing
/// approval is allowed even after revocation or loss of container availability.
pub fn may_share(caller: &Member, runner: &Runner, shared: bool) -> bool {
    caller.disabled_at.is_none()
        && caller.role == TeamRole::Admin
        && (!shared
            || (runner.revoked_at.is_none()
                && runner.declaration.as_ref().is_some_and(|d| d.container)))
}
pub fn visible(caller: &Member, runner: &Runner) -> bool {
    caller.disabled_at.is_none()
        && (caller.role == TeamRole::Admin || caller.identity == runner.owner)
}

/// An inbox inherits only its server-recorded human parent's listing scope.
/// Local or missing parent bindings fail closed; request parameters never bind
/// an inbox to a person. The store rechecks that person's current membership.
pub fn listing_identity(
    principal: &super::queue_service::Principal,
    parent: Option<&str>,
) -> Option<OidcIdentity> {
    match principal.role {
        super::ActorRole::User => principal.human.clone(),
        super::ActorRole::Inbox => serde_json::from_str(parent?.strip_prefix("human:")?).ok(),
        _ => None,
    }
}

/// Optional, all-or-nothing deployment binding for standby Actions runners.
/// `issuer` is exactly https://token.actions.githubusercontent.com; audience
/// must be included in `aud`. Repository, workflow_ref and ref compare exactly;
/// workflow_ref is chosen rather than job_workflow_ref (no fallback). The
/// configured owner is a human registry identity, never the JWT subject.
#[derive(Clone)]
pub struct GithubActions {
    pub issuer: String,
    pub audience: String,
    pub repository: String,
    pub workflow_ref: String,
    pub git_ref: String,
    pub owner: OidcIdentity,
}
pub fn github_owner<'a>(
    config: Option<&'a GithubActions>,
    jwt: &super::oidc::VerifiedJwt,
) -> Option<&'a OidcIdentity> {
    let c = config?;
    let claims = &jwt.claims;
    let aud = claims.get("aud")?;
    let audience = aud.as_str() == Some(&c.audience)
        || aud
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(&c.audience)));
    (c.issuer == "https://token.actions.githubusercontent.com"
        && jwt.identity.issuer == c.issuer
        && claims.get("iss").and_then(serde_json::Value::as_str) == Some(&c.issuer)
        && audience
        && claims.get("repository").and_then(serde_json::Value::as_str) == Some(&c.repository)
        && claims
            .get("workflow_ref")
            .and_then(serde_json::Value::as_str)
            == Some(&c.workflow_ref)
        && claims.get("ref").and_then(serde_json::Value::as_str) == Some(&c.git_ref))
    .then_some(&c.owner)
}

/// Absolute expiry in Unix seconds and the consumption time. Consumed records
/// remain stored to distinguish refresh replay from an unknown credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub expires_at: i64,
    pub used_at: Option<i64>,
}

pub fn enrollment_live(record: Option<&Credential>, now: i64) -> bool {
    record.is_some_and(|r| r.used_at.is_none() && now < r.expires_at)
}

pub fn runner_live(runner: &Runner, owner: Option<&Member>) -> bool {
    runner.revoked_at.is_none()
        && owner.is_some_and(|m| m.identity == runner.owner && m.disabled_at.is_none())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshDecision {
    Rotate,
    Refuse,
    Reuse,
}

/// Reuse revokes the entire runner, including tokens issued by a successful
/// concurrent refresh. A consumed credential remains a reuse signal after expiry.
pub fn refresh_decision(
    record: Option<&Credential>,
    runner: &Runner,
    owner: Option<&Member>,
    now: i64,
) -> RefreshDecision {
    if !runner_live(runner, owner) {
        return RefreshDecision::Refuse;
    }
    match record {
        Some(r) if r.used_at.is_some() => RefreshDecision::Reuse,
        Some(r) if now < r.expires_at => RefreshDecision::Rotate,
        _ => RefreshDecision::Refuse,
    }
}

pub fn may_revoke(caller: &Member, runner: &Runner) -> bool {
    caller.disabled_at.is_none()
        && (caller.role == TeamRole::Admin || caller.identity == runner.owner)
}

pub fn token_live(expires_at: i64, runner: &Runner, owner: Option<&Member>, now: i64) -> bool {
    now < expires_at && runner_live(runner, owner)
}

/// Hashes only, supplied by the application. A transaction consumes the old
/// credential and creates every replacement atomically. Reuse is a committed
/// refusal so that the revocation cannot be rolled back by the error response.
pub enum Change {
    Declare {
        runner_id: String,
        declaration: Declaration,
    },
    Share {
        caller: OidcIdentity,
        runner_id: String,
        shared: bool,
    },
    List {
        caller: OidcIdentity,
    },
    Github {
        owner: OidcIdentity,
        runner_id: String,
        token: String,
    },
    Code {
        owner: OidcIdentity,
        hash: String,
    },
    Enroll {
        hash: String,
        runner_id: String,
        refresh: String,
        token: String,
    },
    Refresh {
        hash: String,
        refresh: String,
        token: String,
    },
    Revoke {
        caller: OidcIdentity,
        runner_id: String,
    },
}
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Updated,
    Listed(Vec<Runner>),
    Code,
    Issued(String),
    Revoked,
    Unauthenticated,
    Denied,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn member() -> Member {
        Member {
            identity: OidcIdentity {
                issuer: "https://idp.example".into(),
                subject: "owner".into(),
            },
            role: TeamRole::Member,
            on_call: false,
            display_name: None,
            created_at: 0,
            disabled_at: None,
        }
    }
    fn runner() -> Runner {
        Runner {
            id: "runner-id".into(),
            owner: member().identity,
            created_at: 100,
            revoked_at: None,
            declaration: None,
            shared: false,
            approved_by: None,
        }
    }
    fn declaration(container: bool) -> Declaration {
        Declaration {
            os: "linux".into(),
            arch: "aarch64".into(),
            container,
            credentials: vec!["team_api_key".into(), "owner_subscription".into()],
        }
    }
    #[test]
    fn declarations_are_bounded_known_hints_without_secret_fields() {
        let good = serde_json::to_value(declaration(true)).unwrap();
        assert_eq!(Declaration::parse(&good), Some(declaration(true)));
        for (field, bad) in [
            ("os", serde_json::json!("unknown")),
            ("arch", serde_json::json!("unknown")),
            ("container", serde_json::json!("true")),
            ("credentials", serde_json::json!(["secret"])),
            (
                "credentials",
                serde_json::json!(["team_api_key", "team_api_key"]),
            ),
            (
                "credentials",
                serde_json::json!(["team_api_key", "owner_subscription", "team_api_key"]),
            ),
            ("token", serde_json::json!("secret")),
            ("value", serde_json::json!("x".repeat(1025))),
        ] {
            let mut bad_value = good.clone();
            bad_value[field] = bad;
            assert!(Declaration::parse(&bad_value).is_none(), "{field}");
        }
        for os in ["linux", "macos", "windows"] {
            for arch in ["x86_64", "aarch64"] {
                assert!(
                    Declaration {
                        os: os.into(),
                        arch: arch.into(),
                        credentials: vec![],
                        ..declaration(false)
                    }
                    .valid()
                );
            }
        }
    }
    #[test]
    fn sharing_requires_active_admin_container_and_live_runner_but_removal_does_not() {
        let admin = Member {
            role: TeamRole::Admin,
            ..member()
        };
        let mut r = runner();
        assert!(!may_share(&admin, &r, true));
        r.declaration = Some(declaration(false));
        assert!(!may_share(&admin, &r, true));
        r.declaration = Some(declaration(true));
        assert!(may_share(&admin, &r, true));
        assert!(!may_share(&member(), &r, true));
        assert!(!may_share(
            &Member {
                disabled_at: Some(1),
                ..admin.clone()
            },
            &r,
            true
        ));
        r.revoked_at = Some(1);
        assert!(!may_share(&admin, &r, true));
        assert!(may_share(&admin, &r, false));
    }
    #[test]
    fn list_scope_is_active_owners_or_administrators_and_never_contains_secrets() {
        let r = runner();
        assert!(visible(&member(), &r));
        let other = Member {
            identity: OidcIdentity {
                subject: "other".into(),
                ..member().identity
            },
            ..member()
        };
        assert!(!visible(&other, &r));
        assert!(visible(
            &Member {
                role: TeamRole::Admin,
                ..other
            },
            &r
        ));
        assert!(!visible(
            &Member {
                disabled_at: Some(1),
                ..member()
            },
            &r
        ));
        let json = serde_json::to_value(r).unwrap();
        for secret in ["token", "refresh", "hash", "credential_value"] {
            assert!(json.get(secret).is_none());
        }
    }
    #[test]
    fn inbox_listing_uses_only_recorded_human_parent_and_retains_owner_scope() {
        use crate::domain::{ActorContext, ActorRole, queue_service::Principal};
        let human = Principal::human(member().identity);
        let inbox = Principal::of(&ActorContext::new(ActorRole::Inbox, "inbox"));
        assert_eq!(listing_identity(&inbox, Some(&human.actor_id)), human.human);
        assert_eq!(listing_identity(&human, None), human.human);
        for binding in [None, Some("user"), Some("human:invalid")] {
            assert!(listing_identity(&inbox, binding).is_none());
        }
        assert!(listing_identity(&Principal::planner(1), Some(&human.actor_id)).is_none());
    }
    #[test]
    fn actions_require_all_claim_bindings_and_choose_only_configured_owner() {
        let config = GithubActions {
            issuer: "https://token.actions.githubusercontent.com".into(),
            audience: "dagq".into(),
            repository: "org/repo".into(),
            workflow_ref: "org/repo/.github/workflows/runner.yml@refs/heads/main".into(),
            git_ref: "refs/heads/main".into(),
            owner: member().identity,
        };
        let claims = serde_json::json!({"iss": config.issuer, "aud": ["other", "dagq"], "repository": config.repository, "workflow_ref": config.workflow_ref, "ref": config.git_ref});
        let jwt = super::super::oidc::VerifiedJwt {
            identity: OidcIdentity {
                issuer: config.issuer.clone(),
                subject: "job".into(),
            },
            claims: claims.as_object().unwrap().clone(),
        };
        assert_eq!(github_owner(Some(&config), &jwt), Some(&config.owner));
        assert!(github_owner(None, &jwt).is_none());
        for name in ["iss", "aud", "repository", "workflow_ref", "ref"] {
            let mut bad = jwt.clone();
            bad.claims
                .insert(name.into(), serde_json::json!("different"));
            assert!(github_owner(Some(&config), &bad).is_none(), "{name}");
            bad.claims.remove(name);
            assert!(github_owner(Some(&config), &bad).is_none());
        }
    }
    #[test]
    fn runner_capability_matrix_excludes_every_ai_actor_and_inbox_sharing() {
        use crate::domain::{ActorRole, Capability, authorization::grants};
        for role in ActorRole::ALL {
            assert_eq!(
                grants(role).contains(&Capability::RunnerDeclare),
                role == ActorRole::Runner
            );
            assert_eq!(
                grants(role).contains(&Capability::RunnerShare),
                role == ActorRole::User
            );
            assert_eq!(
                grants(role).contains(&Capability::RunnerList),
                matches!(role, ActorRole::User | ActorRole::Inbox)
            );
        }
    }
    #[test]
    fn enrollment_requires_an_unconsumed_unexpired_record() {
        let live = Credential {
            expires_at: 200,
            used_at: None,
        };
        assert!(enrollment_live(Some(&live), 199));
        assert!(!enrollment_live(Some(&live), 200));
        assert!(!enrollment_live(None, 100));
        assert!(!enrollment_live(
            Some(&Credential {
                used_at: Some(100),
                ..live
            }),
            101
        ));
    }
    #[test]
    fn refresh_distinguishes_rotation_reuse_and_refusal_from_stored_facts() {
        let runner = runner();
        let owner = member();
        let live = Credential {
            expires_at: 200,
            used_at: None,
        };
        assert_eq!(
            refresh_decision(Some(&live), &runner, Some(&owner), 199),
            RefreshDecision::Rotate
        );
        for record in [
            None,
            Some(&Credential {
                expires_at: 100,
                used_at: None,
            }),
        ] {
            assert_eq!(
                refresh_decision(record, &runner, Some(&owner), 100),
                RefreshDecision::Refuse
            );
        }
        let used = Credential {
            used_at: Some(150),
            ..live.clone()
        };
        for now in [151, 200, 201] {
            assert_eq!(
                refresh_decision(Some(&used), &runner, Some(&owner), now),
                RefreshDecision::Reuse
            );
        }
        for owner in [
            None,
            Some(Member {
                disabled_at: Some(100),
                ..owner.clone()
            }),
        ] {
            assert_eq!(
                refresh_decision(Some(&live), &runner, owner.as_ref(), 100),
                RefreshDecision::Refuse
            );
        }
        assert_eq!(
            refresh_decision(
                Some(&live),
                &Runner {
                    revoked_at: Some(100),
                    ..runner
                },
                Some(&owner),
                100
            ),
            RefreshDecision::Refuse
        );
    }
    #[test]
    fn tokens_require_unexpired_records_live_runners_and_active_matching_owners() {
        let runner = runner();
        let owner = member();
        assert!(token_live(200, &runner, Some(&owner), 199));
        assert!(!token_live(200, &runner, Some(&owner), 200));
        assert!(!token_live(
            200,
            &Runner {
                revoked_at: Some(100),
                ..runner.clone()
            },
            Some(&owner),
            100
        ));
        for owner in [
            None,
            Some(Member {
                disabled_at: Some(100),
                ..owner.clone()
            }),
            Some(Member {
                identity: OidcIdentity {
                    subject: "other".into(),
                    ..owner.identity.clone()
                },
                ..owner
            }),
        ] {
            assert!(!token_live(200, &runner, owner.as_ref(), 100));
        }
    }
    #[test]
    fn only_an_active_owner_or_admin_may_revoke() {
        let owner = member();
        assert!(may_revoke(&owner, &runner()));
        let other = Member {
            identity: OidcIdentity {
                subject: "other".into(),
                ..owner.identity.clone()
            },
            ..owner.clone()
        };
        assert!(!may_revoke(&other, &runner()));
        assert!(may_revoke(
            &Member {
                role: TeamRole::Admin,
                ..other
            },
            &runner()
        ));
        assert!(!may_revoke(
            &Member {
                disabled_at: Some(100),
                ..owner
            },
            &runner()
        ));
    }
    #[test]
    fn runner_is_untrusted_and_only_users_hold_human_runner_capabilities() {
        use crate::domain::authorization::{Authorizer, Resource, StaticPolicy, grants};
        use crate::domain::{ActorRole, Capability, TrustLevel};
        assert_eq!(ActorRole::Runner.trust(), TrustLevel::UntrustedAgent);
        assert_eq!(grants(ActorRole::Runner), &[Capability::RunnerDeclare]);
        for role in ActorRole::ALL {
            for cap in [Capability::RunnerRegister, Capability::RunnerRevoke] {
                assert_eq!(
                    grants(role).contains(&cap),
                    role == ActorRole::User,
                    "{role:?}"
                );
                assert_eq!(
                    StaticPolicy
                        .authorize(
                            &crate::domain::ActorContext::new(role, "actor"),
                            cap,
                            &Resource::Queue
                        )
                        .is_ok(),
                    role == ActorRole::User
                );
            }
        }
    }
}
