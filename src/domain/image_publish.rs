//! The digests of the images a project's main CI builds, which the control
//! side takes and later pins to runs (ADR-t3987-1 decision 4): which CI
//! identity may hand over a digest, which requests are taken, and when a
//! resend changes nothing. No I/O: the store writes, the application asks.
//!
//! The CI is no person and no deterministic control-side code: it runs the
//! repository's code outside the control side, so what it hands over is data
//! the control side checks, and its principal (`image-publisher`) holds no
//! credential that writes main (ADR-t3985-1 decision 8(ii), the line of the
//! runners and the verifying machines).

use super::oidc::VerifiedJwt;
use super::runner_registration::{ActionsClaims, actions_claims_match};
use serde::Serialize;
use serde_json::Value;

/// An image publisher's token lasts fifteen minutes, enough for one CI job
/// to hand over each architecture's digest. It is never refreshed: the
/// exchange issues no refresh credential, and a later job exchanges again.
pub const TOKEN_SECONDS: i64 = 15 * 60;

/// The architectures taken, in OCI's names: the two the base image is
/// published for (ADR-t3990-3).
pub const ARCHES: &[&str] = &["amd64", "arm64"];

/// The deployment's binding of the one CI that may hand over digests
/// (`DAGQ_IMAGE_CI_*`, all or none, read by
/// `infrastructure::deployment_config::DeploymentConfig::image_publish`):
/// GitHub's Actions issuer, the audience the CI asks its JWT for, the repository, the `workflow_ref`
/// (the repository's workflow file pinned at `git_ref`) and `git_ref`,
/// main's `refs/heads/<branch>`. Its values differ from the standby
/// runners' binding ([`super::runner_registration::GithubActions`]), so a
/// runner's job cannot hand over a digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishBinding {
    pub issuer: String,
    pub audience: String,
    pub repository: String,
    pub workflow_ref: String,
    pub git_ref: String,
}

/// Who handed a digest over: the claims of the CI's JWT (`repository`,
/// `workflow_ref`, and `sha`, the commit of main the job ran on), kept on
/// the publisher's token and on every row it records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Publisher {
    pub repository: String,
    pub workflow_ref: String,
    pub commit: String,
}

/// The publisher of a verified JWT when `binding` is configured and every
/// bound claim matches (the comparison of the standby runners' exchange)
/// and its `sha` names a commit; `None` refuses the exchange.
pub fn publisher(binding: Option<&PublishBinding>, jwt: &VerifiedJwt) -> Option<Publisher> {
    let b = binding?;
    let matched = actions_claims_match(
        ActionsClaims {
            issuer: &b.issuer,
            audience: &b.audience,
            repository: &b.repository,
            workflow_ref: &b.workflow_ref,
            git_ref: &b.git_ref,
        },
        jwt,
    );
    let commit = jwt.claims.get("sha").and_then(Value::as_str)?;
    (matched && is_commit(commit)).then(|| Publisher {
        repository: b.repository.clone(),
        workflow_ref: b.workflow_ref.clone(),
        commit: commit.to_owned(),
    })
}

/// A Git commit id as Actions names it: 40 lowercase hexadecimal digits.
fn is_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The issued token's record: only the value's hash is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublisherToken {
    pub id: String,
    pub hash: String,
    pub publisher: Publisher,
    pub expires_at: i64,
}

/// A token is good until its absolute expiry, in Unix seconds.
pub fn token_live(expires_at: i64, now: i64) -> bool {
    now < expires_at
}

/// One architecture's image as the CI hands it over: the tag the control
/// side computes for its inputs (`domain::container_image`) and the
/// registry's manifest digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Publication {
    pub arch: String,
    pub tag: String,
    pub digest: String,
}

/// Why a publication is refused before anything is recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationRefusal {
    /// Not `sha256:` and 64 lowercase hexadecimal digits.
    Digest,
    /// Not one of [`ARCHES`].
    Arch,
    /// Not an OCI tag (`[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`).
    Tag,
    /// The image's client speaks an API version outside the control side's
    /// range ([`super::queue_service::answers`], ADR-t3990-1 decision 3).
    ApiVersion,
}

impl PublicationRefusal {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Digest => "the digest is not sha256: and 64 lowercase hex digits",
            Self::Arch => "the architecture is not one the control side takes",
            Self::Tag => "the tag is not an OCI tag",
            Self::ApiVersion => {
                "the image's client speaks an API version outside the control side's range"
            }
        }
    }
}

/// The publication `params` ask for (`arch`, `tag`, `digest`, and
/// `client_api_version`, which the CI read from the client in the image).
/// The control side pulls no image: the version is the word of the CI bound
/// to main.
pub fn publication(params: &Value) -> Result<Publication, PublicationRefusal> {
    let text = |name| params.get(name).and_then(Value::as_str);
    let digest = text("digest").filter(|d| is_digest(d));
    let digest = digest.ok_or(PublicationRefusal::Digest)?;
    let arch = text("arch").filter(|a| ARCHES.contains(a));
    let arch = arch.ok_or(PublicationRefusal::Arch)?;
    let tag = text("tag")
        .filter(|t| is_tag(t))
        .ok_or(PublicationRefusal::Tag)?;
    let version = params.get("client_api_version").and_then(Value::as_u64);
    if !version
        .and_then(|v| u32::try_from(v).ok())
        .is_some_and(super::queue_service::answers)
    {
        return Err(PublicationRefusal::ApiVersion);
    }
    Ok(Publication {
        arch: arch.to_owned(),
        tag: tag.to_owned(),
        digest: digest.to_owned(),
    })
}

fn is_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

fn is_tag(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        && value.len() <= 128
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A recorded row: the rows of an architecture are appended, and its latest
/// is main's digest now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImageDigest {
    pub arch: String,
    pub tag: String,
    pub digest: String,
    pub publisher: Publisher,
    pub published_at: i64,
}

/// The row `publication` resends, among the architecture's `recorded` rows
/// (in the order they were recorded): the first with the same tag and digest.
/// A CI job run again, or a later job of main whose inputs did not change,
/// hands over a pair already taken, and is answered with that row, adding
/// no row and no event and leaving the architecture's current digest as it
/// is, also when another pair was recorded after it. The row keeps who
/// handed the pair over first.
pub fn resent<'a>(
    recorded: impl IntoIterator<Item = &'a ImageDigest>,
    publication: &Publication,
) -> Option<&'a ImageDigest> {
    recorded.into_iter().find(|r| {
        r.arch == publication.arch && r.tag == publication.tag && r.digest == publication.digest
    })
}

/// What the store did with a publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishOutcome {
    /// A new row (and its event) was written.
    Recorded(ImageDigest),
    /// The current row, unchanged.
    Resent(ImageDigest),
    /// The token is unknown or expired, read in the write's transaction.
    Unauthenticated,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::oidc::OidcIdentity;
    use serde_json::json;

    fn binding() -> PublishBinding {
        PublishBinding {
            issuer: "https://token.actions.githubusercontent.com".into(),
            audience: "dagq-images".into(),
            repository: "org/repo".into(),
            workflow_ref: "org/repo/.github/workflows/image.yml@refs/heads/main".into(),
            git_ref: "refs/heads/main".into(),
        }
    }
    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn jwt(b: &PublishBinding) -> VerifiedJwt {
        let claims = json!({"iss": b.issuer, "aud": b.audience, "repository": b.repository,
            "workflow_ref": b.workflow_ref, "ref": b.git_ref, "sha": SHA});
        VerifiedJwt {
            identity: OidcIdentity {
                issuer: b.issuer.clone(),
                subject: "repo:org/repo:ref:refs/heads/main".into(),
            },
            claims: claims.as_object().unwrap().clone(),
        }
    }

    #[test]
    fn only_a_jwt_matching_every_bound_claim_of_main_is_a_publisher() {
        let b = binding();
        let good = jwt(&b);
        assert_eq!(
            publisher(Some(&b), &good),
            Some(Publisher {
                repository: "org/repo".into(),
                workflow_ref: b.workflow_ref.clone(),
                commit: SHA.into(),
            })
        );
        assert!(publisher(None, &good).is_none(), "no binding configured");
        for name in ["iss", "aud", "repository", "workflow_ref", "ref", "sha"] {
            let mut bad = good.clone();
            bad.claims.insert(name.into(), json!("different"));
            assert!(publisher(Some(&b), &bad).is_none(), "{name}");
            bad.claims.remove(name);
            assert!(publisher(Some(&b), &bad).is_none(), "{name} missing");
        }
        let mut branch = good.clone();
        branch
            .claims
            .insert("ref".into(), json!("refs/heads/feature"));
        assert!(publisher(Some(&b), &branch).is_none(), "not main");
        let mut identity = good.clone();
        identity.identity.issuer = "https://other.example".into();
        assert!(publisher(Some(&b), &identity).is_none());
        let other_issuer = PublishBinding {
            issuer: "https://other.example".into(),
            ..b.clone()
        };
        assert!(publisher(Some(&other_issuer), &jwt(&other_issuer)).is_none());
        // A runner's binding of another workflow is not the publisher's.
        let runner = PublishBinding {
            workflow_ref: "org/repo/.github/workflows/runner.yml@refs/heads/main".into(),
            ..b.clone()
        };
        assert!(publisher(Some(&b), &jwt(&runner)).is_none());
    }

    fn params() -> Value {
        json!({"arch": "arm64", "tag": "abc123", "digest": format!("sha256:{}", "a".repeat(64)),
            "client_api_version": crate::domain::queue_service::API_VERSION})
    }

    #[test]
    fn a_publication_needs_a_sha256_digest_a_known_arch_a_tag_and_a_version_in_range() {
        let good = publication(&params()).unwrap();
        assert_eq!((good.arch.as_str(), good.tag.as_str()), ("arm64", "abc123"));
        use crate::domain::queue_service::{API_VERSION, MIN_API_VERSION};
        for (field, bad, refusal) in [
            ("digest", json!("sha256:abc"), PublicationRefusal::Digest),
            (
                "digest",
                json!(format!("sha512:{}", "a".repeat(64))),
                PublicationRefusal::Digest,
            ),
            (
                "digest",
                json!(format!("sha256:{}", "A".repeat(64))),
                PublicationRefusal::Digest,
            ),
            (
                "digest",
                json!(format!("sha256:{}", "a".repeat(65))),
                PublicationRefusal::Digest,
            ),
            ("digest", json!(null), PublicationRefusal::Digest),
            ("arch", json!("x86_64"), PublicationRefusal::Arch),
            ("arch", json!("riscv64"), PublicationRefusal::Arch),
            ("arch", json!(null), PublicationRefusal::Arch),
            ("tag", json!(""), PublicationRefusal::Tag),
            ("tag", json!("-lead"), PublicationRefusal::Tag),
            ("tag", json!("a/b"), PublicationRefusal::Tag),
            ("tag", json!("a".repeat(129)), PublicationRefusal::Tag),
            (
                "client_api_version",
                json!(MIN_API_VERSION - 1),
                PublicationRefusal::ApiVersion,
            ),
            (
                "client_api_version",
                json!(API_VERSION + 1),
                PublicationRefusal::ApiVersion,
            ),
            (
                "client_api_version",
                json!(u64::MAX),
                PublicationRefusal::ApiVersion,
            ),
            (
                "client_api_version",
                json!("2"),
                PublicationRefusal::ApiVersion,
            ),
            (
                "client_api_version",
                json!(null),
                PublicationRefusal::ApiVersion,
            ),
        ] {
            let mut p = params();
            p[field] = bad;
            assert_eq!(publication(&p), Err(refusal), "{field}: {p}");
        }
        for arch in ARCHES {
            let mut p = params();
            p["arch"] = json!(arch);
            assert!(publication(&p).is_ok());
        }
        for version in MIN_API_VERSION..=API_VERSION {
            let mut p = params();
            p["client_api_version"] = json!(version);
            assert!(publication(&p).is_ok());
        }
        let mut long = params();
        long["tag"] = json!(format!("_{}", "a.b-c".repeat(25)));
        assert!(publication(&long).is_ok());
    }

    #[test]
    fn any_pair_the_arch_recorded_before_is_a_resend_of_its_first_row() {
        let p = publication(&params()).unwrap();
        let row = |digest: char, at: i64| ImageDigest {
            arch: p.arch.clone(),
            tag: p.tag.clone(),
            digest: format!("sha256:{}", digest.to_string().repeat(64)),
            publisher: Publisher {
                repository: "org/repo".into(),
                workflow_ref: "w".into(),
                commit: SHA.into(),
            },
            published_at: at,
        };
        // A, then B, then A again: the first A is resent, not recorded anew.
        let recorded = [row('a', 1), row('b', 2), row('a', 3)];
        assert_eq!(resent(&recorded, &p), Some(&recorded[0]));
        assert_eq!(resent(&recorded[1..2], &p), None);
        assert_eq!(resent(&[], &p), None);
        for change in [
            |r: &mut ImageDigest| r.tag = "other".into(),
            |r: &mut ImageDigest| r.arch = "amd64".into(),
        ] {
            let mut other = row('a', 1);
            change(&mut other);
            assert_eq!(resent([&other], &p), None);
        }
        assert!(token_live(10, 9) && !token_live(10, 10));
    }

    #[test]
    fn the_image_publisher_is_an_untrusted_principal_only_role_of_one_capability() {
        use crate::domain::authorization::{Authorizer, Resource, StaticPolicy, grants, starts};
        use crate::domain::{ActorContext, ActorRole, Capability, TrustLevel};
        let role = ActorRole::ImagePublisher;
        assert_eq!(role.as_str(), "image-publisher");
        assert_eq!(role.trust(), TrustLevel::UntrustedAgent);
        assert_eq!(grants(role), &[Capability::ImagePublish]);
        for other in ActorRole::ALL {
            assert_eq!(
                grants(other).contains(&Capability::ImagePublish),
                other == role,
                "{other:?}"
            );
            assert!(!starts(other, role), "{other:?} starts it");
        }
        assert!(role.env_value().is_none());
        let from_env = ActorContext::from_env(|name| {
            (name == "DAGQ_ROLE").then(|| "image-publisher".to_owned())
        });
        assert!(from_env.is_err());
        let actor = ActorContext::instance(role, "p1");
        for capability in Capability::ALL {
            assert_eq!(
                StaticPolicy
                    .authorize(&actor, capability, &Resource::Queue)
                    .is_ok(),
                capability == Capability::ImagePublish,
                "{capability:?}"
            );
        }
        let principal = crate::domain::queue_service::Principal::image_publisher("p1");
        assert!(principal.check().is_ok());
        assert_eq!(principal.actor_id, "image-publisher:p1");
    }
}
