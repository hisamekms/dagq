//! Runner credentials belong to an active human registry entry. All decisions
//! take stored facts and Unix seconds; no clock or credential value lives here.
use super::{
    oidc::OidcIdentity,
    queue_service::{Member, TeamRole},
};

/// Enrollment codes and access tokens last fifteen minutes. Refresh credentials
/// last thirty days and rotate on every use. Values are 256 random bits returned
/// only in responses; persistence receives their SHA-256 hashes. The caller
/// transfers an enrollment code to its runner; this service writes no credentials.
pub const SHORT_SECONDS: i64 = 15 * 60;
pub const REFRESH_SECONDS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    pub id: String,
    pub owner: OidcIdentity,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
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
        assert!(grants(ActorRole::Runner).is_empty());
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
