//! The check of an OIDC issuer's JWT with its keys remembered and fetched
//! (ADR-t3985-1 decision 1): [`verify_jwt`] runs the decisions of
//! [`crate::domain::oidc`] in order and fetches through
//! [`OidcDocumentSource`]. No entry calls it yet.

use tracing::warn;

use crate::domain::oidc::{
    JwtRefusal, KeyFetchFailure, KeyMemory, OidcConfig, PublicKey, SignedJwt, VerifiedJwt,
    check_discovery, discovery_url, key_lifetime, parse_keys,
};

/// A document fetched from an issuer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcDocument {
    pub body: Vec<u8>,
    pub cache_control: Option<String>,
}

/// The fetching of an issuer's documents. An implementation follows no
/// redirect and holds to the limits of [`crate::domain::oidc`]: it decides by
/// [`crate::domain::oidc::check_response`] and
/// [`crate::domain::oidc::check_body_length`], and bounds the time by
/// [`crate::domain::oidc::FETCH_TIMEOUT_SECS`].
pub trait OidcDocumentSource: Send + Sync {
    fn get(&self, url: &str) -> Result<OidcDocument, KeyFetchFailure>;
}

/// The issuer and subject and the claims of a token that passes at `now`
/// (Unix seconds), or why it does not.
///
/// One `memory` belongs to one issuer, and its holder serializes the calls
/// (a fetch happens inside). The token's value goes into no log and no
/// error; the log line of a failed fetch names the issuer and the failure.
pub fn verify_jwt(
    config: &OidcConfig,
    token: &str,
    memory: &mut KeyMemory,
    source: &dyn OidcDocumentSource,
    now: i64,
) -> Result<VerifiedJwt, JwtRefusal> {
    let jwt = SignedJwt::parse(token, config)?;
    if memory.should_fetch(&jwt, now) {
        match fetch_keys(config, source) {
            Ok((keys, lifetime)) => memory.record_fetched(keys, lifetime, now),
            Err(failure) => {
                warn!(issuer = config.issuer(), %failure, "could not fetch an OIDC issuer's keys");
                memory.record_failed(now);
            }
        }
    }
    jwt.verify(memory.key_for(&jwt, now)?, config, now)
}

/// The issuer's keys and how long to remember them.
fn fetch_keys(
    config: &OidcConfig,
    source: &dyn OidcDocumentSource,
) -> Result<(Vec<PublicKey>, i64), KeyFetchFailure> {
    let discovery = source.get(&discovery_url(config.issuer()))?;
    let jwks_uri = check_discovery(config, &discovery.body)?;
    let document = source.get(&jwks_uri)?;
    let keys = parse_keys(&document.body)?;
    Ok((keys, key_lifetime(document.cache_control.as_deref())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    use serde_json::{Value, json};
    use std::sync::Mutex;

    const ISSUER: &str = "https://idp.example";
    const NOW: i64 = 1_500;

    fn b64(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    struct Signer(EcdsaKeyPair);

    impl Signer {
        fn new() -> Self {
            let rng = SystemRandom::new();
            let alg = &ECDSA_P256_SHA256_FIXED_SIGNING;
            let der = EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
            Self(EcdsaKeyPair::from_pkcs8(alg, der.as_ref(), &rng).unwrap())
        }

        fn jwks(&self, kid: &str) -> Vec<u8> {
            let point = self.0.public_key().as_ref();
            json!({"keys": [{"kty": "EC", "crv": "P-256", "kid": kid,
                "x": b64(&point[1..33]), "y": b64(&point[33..])}]})
            .to_string()
            .into_bytes()
        }

        fn token(&self, kid: &str) -> String {
            let header: Value = json!({"alg": "ES256", "kid": kid});
            let claims = json!({"iss": ISSUER, "sub": "person-1", "aud": "dagq", "exp": 9_000});
            let input = format!(
                "{}.{}",
                b64(header.to_string().as_bytes()),
                b64(claims.to_string().as_bytes())
            );
            let signature = self.0.sign(&SystemRandom::new(), input.as_bytes()).unwrap();
            format!("{input}.{}", b64(signature.as_ref()))
        }
    }

    /// Answers the two documents, or fails, and counts what was asked.
    struct Source {
        jwks: Mutex<Result<Vec<u8>, KeyFetchFailure>>,
        asked: Mutex<Vec<String>>,
    }

    impl Source {
        fn new(jwks: Result<Vec<u8>, KeyFetchFailure>) -> Self {
            Self {
                jwks: Mutex::new(jwks),
                asked: Mutex::new(Vec::new()),
            }
        }

        fn fetches(&self) -> usize {
            self.asked.lock().unwrap().len() / 2
        }
    }

    impl OidcDocumentSource for Source {
        fn get(&self, url: &str) -> Result<OidcDocument, KeyFetchFailure> {
            self.asked.lock().unwrap().push(url.to_string());
            let body = if url.ends_with("/.well-known/openid-configuration") {
                json!({"issuer": ISSUER, "jwks_uri": "https://keys.example/jwks"})
                    .to_string()
                    .into_bytes()
            } else {
                self.jwks.lock().unwrap().clone()?
            };
            Ok(OidcDocument {
                body,
                cache_control: Some("max-age=600".into()),
            })
        }
    }

    fn config() -> OidcConfig {
        OidcConfig::new(ISSUER, "dagq").unwrap()
    }

    #[test]
    fn the_keys_are_fetched_through_discovery_once_and_remembered() {
        let signer = Signer::new();
        let source = Source::new(Ok(signer.jwks("k1")));
        let mut memory = KeyMemory::default();

        for now in [NOW, NOW + 1, NOW + 599] {
            let verified =
                verify_jwt(&config(), &signer.token("k1"), &mut memory, &source, now).unwrap();
            assert_eq!(verified.identity.subject, "person-1");
        }
        assert_eq!(
            *source.asked.lock().unwrap(),
            [
                "https://idp.example/.well-known/openid-configuration",
                "https://keys.example/jwks"
            ]
        );
    }

    #[test]
    fn an_unknown_key_fetches_once_and_a_failed_fetch_refuses_only_what_is_not_known() {
        let signer = Signer::new();
        let source = Source::new(Ok(signer.jwks("k1")));
        let mut memory = KeyMemory::default();
        let config = config();
        verify_jwt(&config, &signer.token("k1"), &mut memory, &source, NOW).unwrap();
        assert_eq!(source.fetches(), 1);

        // Within the interval: no fetch.
        assert_eq!(
            verify_jwt(&config, &signer.token("k2"), &mut memory, &source, NOW + 1).unwrap_err(),
            JwtRefusal::UnknownKey
        );
        assert_eq!(source.fetches(), 1);

        *source.jwks.lock().unwrap() = Err(KeyFetchFailure::Status(503));
        let later = NOW + 61;
        assert_eq!(
            verify_jwt(&config, &signer.token("k2"), &mut memory, &source, later).unwrap_err(),
            JwtRefusal::UnknownKey
        );
        assert_eq!(source.asked.lock().unwrap().len(), 4);
        assert!(verify_jwt(&config, &signer.token("k1"), &mut memory, &source, later).is_ok());
        assert_eq!(source.asked.lock().unwrap().len(), 4);
    }

    #[test]
    fn an_issuer_that_cannot_be_fetched_is_refused_without_keys_or_past_their_time() {
        let signer = Signer::new();
        let source = Source::new(Err(KeyFetchFailure::Unreachable("down".into())));
        let mut memory = KeyMemory::default();
        assert_eq!(
            verify_jwt(&config(), &signer.token("k1"), &mut memory, &source, NOW).unwrap_err(),
            JwtRefusal::KeysUnavailable
        );

        *source.jwks.lock().unwrap() = Ok(signer.jwks("k1"));
        let up = NOW + 61;
        verify_jwt(&config(), &signer.token("k1"), &mut memory, &source, up).unwrap();
        *source.jwks.lock().unwrap() = Err(KeyFetchFailure::Status(500));
        assert_eq!(
            verify_jwt(
                &config(),
                &signer.token("k1"),
                &mut memory,
                &source,
                up + 600
            )
            .unwrap_err(),
            JwtRefusal::KeysUnavailable
        );
    }

    #[test]
    fn a_token_refused_on_its_form_costs_no_fetch() {
        let source = Source::new(Ok(Vec::new()));
        let mut memory = KeyMemory::default();
        assert_eq!(
            verify_jwt(&config(), "a.b.c", &mut memory, &source, NOW).unwrap_err(),
            JwtRefusal::Malformed
        );
        assert_eq!(source.asked.lock().unwrap().len(), 0);
    }
}
