//! The check of an OIDC issuer's JWT with its keys remembered and fetched
//! (ADR-t3985-1 decision 1): [`OidcVerifier::verify_jwt`] runs the decisions of
//! [`crate::domain::oidc`] in order and fetches through
//! [`OidcDocumentSource`]. No entry calls it yet.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

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

/// Shared owner of one issuer's configuration, keys and document source.
/// Share this owner across threads (for example through `Arc`), rather than
/// putting verification behind a caller's lock. Only one fetch runs at once;
/// valid remembered keys remain available throughout it.
pub struct OidcVerifier<S> {
    config: OidcConfig,
    source: S,
    state: Mutex<KeyState>,
    fetched: Condvar,
}

#[derive(Default)]
struct KeyState {
    memory: KeyMemory,
    fetching: bool,
    completed: u64,
    #[cfg(test)]
    waiting: usize,
}

impl<S: OidcDocumentSource> OidcVerifier<S> {
    pub fn new(config: OidcConfig, source: S) -> Self {
        Self {
            config,
            source,
            state: Mutex::new(KeyState::default()),
            fetched: Condvar::new(),
        }
    }

    /// Verify at `now` (Unix seconds). No token value enters a log or error.
    /// A caller needing keys joins the current fetch and uses its result,
    /// without starting another fetch. Its wait is bounded by the two document
    /// time limits, even if a source violates its timeout contract. A source
    /// panic unwinds its caller, clears the fetch mark and wakes waiters;
    /// the failed attempt still counts toward the minimum fetch interval.
    pub fn verify_jwt(&self, token: &str, now: i64) -> Result<VerifiedJwt, JwtRefusal> {
        let jwt = SignedJwt::parse(token, &self.config)?;
        let mut state = self.state.lock().unwrap();
        if state.memory.key_for(&jwt, now).is_err() {
            if state.fetching {
                #[cfg(test)]
                {
                    state.waiting += 1;
                    self.fetched.notify_all();
                }
                let completed = state.completed;
                let (after, timeout) = self
                    .fetched
                    .wait_timeout_while(
                        state,
                        Duration::from_secs(2 * crate::domain::oidc::FETCH_TIMEOUT_SECS),
                        |s| s.fetching && s.completed == completed,
                    )
                    .unwrap();
                state = after;
                #[cfg(test)]
                {
                    state.waiting -= 1;
                }
                if timeout.timed_out() && state.fetching && state.completed == completed {
                    return Err(JwtRefusal::KeysUnavailable);
                }
            } else if state.memory.should_fetch(&jwt, now) {
                state.fetching = true;
                state.memory.record_failed(now);
                let fetching = Fetching(self);
                drop(state);
                // The guard clears the in-flight mark and wakes waiters on
                // success, failure and unwind, without poisoning the memory.
                let result = fetch_keys(&self.config, &self.source);
                if let Err(failure) = &result {
                    warn!(issuer = self.config.issuer(), %failure, "could not fetch an OIDC issuer's keys");
                }
                state = self.state.lock().unwrap();
                if let Ok((keys, lifetime)) = result {
                    state.memory.record_fetched(keys, lifetime, now);
                }
                drop(state);
                drop(fetching);
                state = self.state.lock().unwrap();
            }
        }
        let key = state.memory.key_for(&jwt, now)?.clone();
        drop(state);
        jwt.verify(&key, &self.config, now)
    }
}

struct Fetching<'a, S>(&'a OidcVerifier<S>);

impl<S> Drop for Fetching<'_, S> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.fetching = false;
        state.completed += 1;
        self.0.fetched.notify_all();
    }
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
        let verifier = OidcVerifier::new(config(), source);
        let source = &verifier.source;

        for now in [NOW, NOW + 1, NOW + 599] {
            let verified = verifier.verify_jwt(&signer.token("k1"), now).unwrap();
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
        let verifier = OidcVerifier::new(config(), source);
        let source = &verifier.source;
        verifier.verify_jwt(&signer.token("k1"), NOW).unwrap();
        assert_eq!(source.fetches(), 1);

        // Within the interval: no fetch.
        assert_eq!(
            verifier
                .verify_jwt(&signer.token("k2"), NOW + 1)
                .unwrap_err(),
            JwtRefusal::UnknownKey
        );
        assert_eq!(source.fetches(), 1);

        *source.jwks.lock().unwrap() = Err(KeyFetchFailure::Status(503));
        let later = NOW + 61;
        assert_eq!(
            verifier.verify_jwt(&signer.token("k2"), later).unwrap_err(),
            JwtRefusal::UnknownKey
        );
        assert_eq!(source.asked.lock().unwrap().len(), 4);
        assert!(verifier.verify_jwt(&signer.token("k1"), later).is_ok());
        assert_eq!(source.asked.lock().unwrap().len(), 4);
    }

    #[test]
    fn an_issuer_that_cannot_be_fetched_is_refused_without_keys_or_past_their_time() {
        let signer = Signer::new();
        let source = Source::new(Err(KeyFetchFailure::Unreachable("down".into())));
        let verifier = OidcVerifier::new(config(), source);
        let source = &verifier.source;
        assert_eq!(
            verifier.verify_jwt(&signer.token("k1"), NOW).unwrap_err(),
            JwtRefusal::KeysUnavailable
        );

        *source.jwks.lock().unwrap() = Ok(signer.jwks("k1"));
        let up = NOW + 61;
        verifier.verify_jwt(&signer.token("k1"), up).unwrap();
        *source.jwks.lock().unwrap() = Err(KeyFetchFailure::Status(500));
        assert_eq!(
            verifier
                .verify_jwt(&signer.token("k1"), up + 600)
                .unwrap_err(),
            JwtRefusal::KeysUnavailable
        );
    }

    #[test]
    fn a_token_refused_on_its_form_costs_no_fetch() {
        let source = Source::new(Ok(Vec::new()));
        let verifier = OidcVerifier::new(config(), source);
        let source = &verifier.source;
        assert_eq!(
            verifier.verify_jwt("a.b.c", NOW).unwrap_err(),
            JwtRefusal::Malformed
        );
        assert_eq!(source.asked.lock().unwrap().len(), 0);
    }

    /// Stops only the JWKS request, leaving both source and memory locks free.
    struct BlockedSource {
        source: Source,
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<bool>>,
    }

    impl OidcDocumentSource for BlockedSource {
        fn get(&self, url: &str) -> Result<OidcDocument, KeyFetchFailure> {
            if url.ends_with("/jwks") {
                self.entered.send(()).unwrap();
                let panic = self
                    .release
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                assert!(!panic, "source panic");
            }
            self.source.get(url)
        }
    }

    #[test]
    fn known_keys_do_not_wait_and_fetch_waiters_share_success_failure_and_panic() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;
        let limit = Duration::from_secs(5);
        // All waits have deadlines; channels and the waiter's registration,
        // rather than elapsed sleep, establish the ordering.
        for outcome in [0, 1, 2] {
            let signer = Signer::new();
            let (entered, entering) = mpsc::channel();
            let (release, released) = mpsc::channel();
            let verifier = Arc::new(OidcVerifier::new(
                config(),
                BlockedSource {
                    source: Source::new(Ok(signer.jwks("k2"))),
                    entered,
                    release: Mutex::new(released),
                },
            ));
            verifier.state.lock().unwrap().memory.record_fetched(
                parse_keys(&signer.jwks("k1")).unwrap(),
                600,
                NOW,
            );
            if outcome == 1 {
                *verifier.source.source.jwks.lock().unwrap() = Err(KeyFetchFailure::Status(503));
            }
            let (done, results) = mpsc::channel();
            let spawn = |kid: &str| {
                let verifier = Arc::clone(&verifier);
                let token = signer.token(kid);
                let done = done.clone();
                std::thread::spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        verifier.verify_jwt(&token, NOW + 61)
                    }));
                    done.send(result).unwrap();
                })
            };
            let fetching = spawn("k2");
            entering.recv_timeout(limit).unwrap();
            let known = spawn("k1");
            assert!(results.recv_timeout(limit).unwrap().unwrap().is_ok());
            known.join().unwrap();
            let accepted = spawn("k2");
            let refused = spawn("made-up");
            let state = verifier.state.lock().unwrap();
            let (state, timeout) = verifier
                .fetched
                .wait_timeout_while(state, limit, |s| s.waiting != 2)
                .unwrap();
            assert!(!timeout.timed_out());
            drop(state);
            release.send(outcome == 2).unwrap();
            let results: Vec<_> = (0..3)
                .map(|_| results.recv_timeout(limit).unwrap())
                .collect();
            fetching.join().unwrap();
            accepted.join().unwrap();
            refused.join().unwrap();
            assert_eq!(
                results.iter().filter(|r| r.is_err()).count(),
                usize::from(outcome == 2)
            );
            let answers: Vec<_> = results.into_iter().filter_map(Result::ok).collect();
            assert_eq!(
                answers.iter().filter(|r| r.is_ok()).count(),
                if outcome == 0 { 2 } else { 0 }
            );
            assert!(
                answers
                    .iter()
                    .filter(|r| r.is_err())
                    .all(|r| r.as_ref().unwrap_err() == &JwtRefusal::UnknownKey)
            );
            assert!(entering.try_recv().is_err());
            if outcome != 0 {
                assert!(verifier.verify_jwt(&signer.token("k1"), NOW + 62).is_ok());
            }
            // Made-up kids do not bypass the interval, including after panic.
            for now in NOW + 62..NOW + 121 {
                assert_eq!(
                    verifier
                        .verify_jwt(&signer.token("made-up"), now)
                        .unwrap_err(),
                    JwtRefusal::UnknownKey
                );
            }
            *verifier.source.source.jwks.lock().unwrap() = Ok(signer.jwks("k3"));
            release.send(false).unwrap();
            assert!(verifier.verify_jwt(&signer.token("k3"), NOW + 121).is_ok());
            entering.recv_timeout(limit).unwrap();
            assert!(entering.try_recv().is_err());
        }
    }
}
