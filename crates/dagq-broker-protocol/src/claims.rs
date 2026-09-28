//! [`TokenClaims`]: what a run's token says about the run (the claims of the
//! broker session, ADR-t827-2).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::BrokerCapability;

/// The role a token is issued to. Phase 1 issues tokens to workers only; any
/// other role is refused when read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerRole {
    Worker,
}

/// The author and committer of the broker's commits, taken from the
/// repository's `git config` when the token is issued (the container has no
/// `~/.gitconfig`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Committer {
    pub name: String,
    pub email: String,
}

/// The claims of a run's token. A field missing or unknown, or a capability
/// or role this build does not know, refuses the whole token (fail closed).
/// The fields serialize in this order and the capabilities in theirs, so the
/// signed bytes are the same for the same claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenClaims {
    /// The token format, [`crate::TOKEN_VERSION`].
    pub v: u32,
    /// The token's id (a UUID v4), the name of its active mark.
    pub jti: String,
    /// The worker's actor id.
    pub actor_id: String,
    pub role: BrokerRole,
    pub task_id: u64,
    pub run_id: String,
    /// The run's worktree, absolute and canonical: nothing outside it.
    pub workspace: String,
    /// `dagq/<run id>`, the one branch a commit may go on.
    pub branch: String,
    pub committer: Committer,
    pub capabilities: BTreeSet<BrokerCapability>,
    /// Issued at, UNIX seconds.
    pub iat: u64,
    /// Expires at, UNIX seconds.
    pub exp: u64,
}

impl TokenClaims {
    /// Whether the token allows `capability`.
    pub fn allows(&self, capability: BrokerCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Whether the token has expired at `now` (UNIX seconds): it is valid
    /// until, not at, `exp`.
    pub fn expired_at(&self, now: u64) -> bool {
        now >= self.exp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TOKEN_VERSION, decode, encode};

    fn claims() -> TokenClaims {
        TokenClaims {
            v: TOKEN_VERSION,
            jti: "3f0c6a8e-0000-4000-8000-000000000001".to_owned(),
            actor_id: "worker:run-1".to_owned(),
            role: BrokerRole::Worker,
            task_id: 828,
            run_id: "run-1".to_owned(),
            workspace: "/q/runs/run-1/worktree".to_owned(),
            branch: "dagq/run-1".to_owned(),
            committer: Committer {
                name: "A".to_owned(),
                email: "a@example.com".to_owned(),
            },
            capabilities: [BrokerCapability::GitWrite, BrokerCapability::FsRead]
                .into_iter()
                .collect(),
            iat: 100,
            exp: 200,
        }
    }

    const CLAIMS_JSON: &str = concat!(
        r#"{"v":1,"jti":"3f0c6a8e-0000-4000-8000-000000000001","actor_id":"worker:run-1","#,
        r#""role":"worker","task_id":828,"run_id":"run-1","workspace":"/q/runs/run-1/worktree","#,
        r#""branch":"dagq/run-1","committer":{"name":"A","email":"a@example.com"},"#,
        r#""capabilities":["fs.read","git.write"],"iat":100,"exp":200}"#
    );

    #[test]
    fn serializes_to_the_same_bytes_in_a_fixed_order() {
        let bytes = encode(&claims()).unwrap();
        assert_eq!(String::from_utf8(bytes.clone()).unwrap(), CLAIMS_JSON);
        // Capabilities given in another order are the same bytes.
        let mut other = claims();
        other.capabilities = [BrokerCapability::FsRead, BrokerCapability::GitWrite]
            .into_iter()
            .collect();
        assert_eq!(encode(&other).unwrap(), bytes);
        assert_eq!(decode::<TokenClaims>(&bytes).unwrap(), claims());
    }

    #[test]
    fn reading_capabilities_out_of_order_gives_the_same_claims() {
        let shuffled =
            CLAIMS_JSON.replace(r#"["fs.read","git.write"]"#, r#"["git.write","fs.read"]"#);
        let read = decode::<TokenClaims>(shuffled.as_bytes()).unwrap();
        assert_eq!(read, claims());
        assert_eq!(encode(&read).unwrap(), CLAIMS_JSON.as_bytes());
    }

    #[test]
    fn an_unknown_capability_refuses_the_claims() {
        let json = CLAIMS_JSON.replace(r#""git.write""#, r#""git.push""#);
        let error = decode::<TokenClaims>(json.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("git.push"), "{error}");
    }

    #[test]
    fn an_unknown_field_refuses_the_claims() {
        let json = CLAIMS_JSON.replace(r#""exp":200}"#, r#""exp":200,"admin":true}"#);
        let error = decode::<TokenClaims>(json.as_bytes()).unwrap_err();
        assert!(
            error.to_string().contains("unknown field `admin`"),
            "{error}"
        );
        let json = CLAIMS_JSON.replace(
            r#""email":"a@example.com"}"#,
            r#""email":"a@example.com","signingkey":"x"}"#,
        );
        assert!(decode::<TokenClaims>(json.as_bytes()).is_err());
    }

    #[test]
    fn a_missing_field_or_an_unknown_role_refuses_the_claims() {
        let json = CLAIMS_JSON.replace(r#""branch":"dagq/run-1","#, "");
        let error = decode::<TokenClaims>(json.as_bytes()).unwrap_err();
        assert!(
            error.to_string().contains("missing field `branch`"),
            "{error}"
        );
        let json = CLAIMS_JSON.replace(r#""role":"worker""#, r#""role":"supervisor""#);
        assert!(decode::<TokenClaims>(json.as_bytes()).is_err());
    }

    #[test]
    fn allows_and_expiry() {
        let claims = claims();
        assert!(claims.allows(BrokerCapability::FsRead));
        assert!(!claims.allows(BrokerCapability::ProcessExec));
        assert!(!claims.expired_at(199));
        assert!(claims.expired_at(200));
    }
}
