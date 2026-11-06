use super::*;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair};
use serde_json::json;

const ISSUER: &str = "https://idp.example";
const AUDIENCE: &str = "dagq";
const NOW: i64 = 1_500;

// Signed once with a throwaway 2048-bit key that was not kept, for
// iss = ISSUER, sub = person-1, aud = dagq, iat = 1000, exp = 2000 and kid = rsa-1.
const RS256_TOKEN: &str = "eyJhbGciOiJSUzI1NiIsImtpZCI6InJzYS0xIiwidHlwIjoiSldUIn0.eyJpc3MiOiJodHRwczovL2lkcC5leGFtcGxlIiwic3ViIjoicGVyc29uLTEiLCJhdWQiOiJkYWdxIiwiaWF0IjoxMDAwLCJleHAiOjIwMDB9.lwuYp7znp6sfsVKmC-P0vu9M3dfWXlzN18sVsoQmctHZEyWm5f1woMB4uIzdPxw2cddWC62BHnFW-48XcHux7J8n7gUqcL9vOY1VX3DTd9XaIxBtFM0HUZfgz13Hvvlep35Dcng68ufIlj2-_E25m0_7S2d4NbRZ4AfCHE3Tb6PFgn28cuACu1YtQ95l8giHSik2Nn2XGVGVQNvZE0C11Oepaw1gFgLYqeVKCJgM7hXAWj8DE8FfM5KfKn8p0a_4NZdjqxf15bcf4rHczm1-wCQbvsaT2AVo52vTrTrx50EBKLJN_DmKLMXmaIpfMpaGZ98eQrl6P1PwjfeCSNDStA";
const RS256_N: &str = "pogNKNtn4VPcoEC6uovIY7mZ5DQkLc9bOKiVLLdvWmrbK350Fy50bAnPNNHql7IGLkmScLebdYjexOoc5boSz95nwCI2kob3ye0WlVpwI08xALlhp6KAVqs-lNgK-ve_ogIEF78zjwTZIoYbkihBNFJBCrzc_KM0RRDO99dWXjKYBR3AIN8gEVIx3p5YUeMhwUnBO2PUrcjav0b9HVtfTlFLcYegBwUST512UTjN3x1KDqsss1ql_6WoJ8zcQ57b-uubwep93la9iKAbV4H6HaJ_vvV5NDTLG3QSyA1aVqWRECT_GfxxWtxpgkNcLuBOeR5MIDYCYrHzinzeew4hqQ";

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn config() -> OidcConfig {
    OidcConfig::new(ISSUER, AUDIENCE).unwrap()
}

fn claims() -> Value {
    json!({"iss": ISSUER, "sub": "person-1", "aud": AUDIENCE, "iat": 1000, "exp": 2000})
}

struct Es256 {
    pair: EcdsaKeyPair,
}

impl Es256 {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let alg = &ECDSA_P256_SHA256_FIXED_SIGNING;
        let der = EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
        Self {
            pair: EcdsaKeyPair::from_pkcs8(alg, der.as_ref(), &rng).unwrap(),
        }
    }

    fn jwk(&self, kid: &str) -> Value {
        let point = self.pair.public_key().as_ref();
        json!({"kty": "EC", "crv": "P-256", "kid": kid, "use": "sig",
            "x": b64(&point[1..33]), "y": b64(&point[33..])})
    }

    fn key(&self, kid: &str) -> PublicKey {
        parse_key(&self.jwk(kid)).unwrap()
    }

    fn sign(&self, kid: &str, claims: &Value) -> String {
        let input = signing_input(&json!({"alg": "ES256", "kid": kid}), claims);
        let signature = self
            .pair
            .sign(&SystemRandom::new(), input.as_bytes())
            .unwrap();
        format!("{input}.{}", b64(signature.as_ref()))
    }
}

fn signing_input(header: &Value, claims: &Value) -> String {
    format!(
        "{}.{}",
        b64(header.to_string().as_bytes()),
        b64(claims.to_string().as_bytes())
    )
}

/// A token under the given header with a signature of nonsense.
fn with_header(header: Value) -> String {
    format!("{}.AAAA", signing_input(&header, &claims()))
}

/// The whole check against one key.
fn check(token: &str, key: &PublicKey, config: &OidcConfig) -> Result<VerifiedJwt, JwtRefusal> {
    SignedJwt::parse(token, config)?.verify(key, config, NOW)
}

fn es256_check(claims: Value) -> Result<VerifiedJwt, JwtRefusal> {
    let signer = Es256::new();
    check(&signer.sign("k1", &claims), &signer.key("k1"), &config())
}

fn edit(change: impl FnOnce(&mut Map<String, Value>)) -> Value {
    let mut claims = claims();
    change(claims.as_object_mut().unwrap());
    claims
}

#[test]
fn a_good_es256_token_passes_with_its_issuer_and_subject() {
    let verified = es256_check(claims()).unwrap();
    assert_eq!(
        verified.identity,
        OidcIdentity {
            issuer: ISSUER.into(),
            subject: "person-1".into(),
        }
    );
}

#[test]
fn a_good_rs256_token_passes_and_a_changed_signature_does_not() {
    let key = parse_key(&json!({"kty": "RSA", "kid": "rsa-1", "n": RS256_N, "e": "AQAB"})).unwrap();
    let verified = check(RS256_TOKEN, &key, &config()).unwrap();
    assert_eq!(verified.identity.subject, "person-1");

    let (input, signature) = RS256_TOKEN.rsplit_once('.').unwrap();
    let flipped = if signature.starts_with('l') { 'm' } else { 'l' };
    let tampered = format!("{input}.{flipped}{}", &signature[1..]);
    assert_eq!(
        check(&tampered, &key, &config()).unwrap_err(),
        JwtRefusal::BadSignature
    );
}

#[test]
fn a_signature_by_another_key_or_over_other_claims_is_refused() {
    let signer = Es256::new();
    let token = signer.sign("k1", &claims());
    let other = Es256::new().key("k1");
    assert_eq!(
        check(&token, &other, &config()).unwrap_err(),
        JwtRefusal::BadSignature
    );

    let parts: Vec<&str> = token.split('.').collect();
    let forged = edit(|c| {
        c.insert("sub".into(), json!("someone-else"));
    });
    let forged = format!(
        "{}.{}.{}",
        parts[0],
        b64(forged.to_string().as_bytes()),
        parts[2]
    );
    assert_eq!(
        check(&forged, &signer.key("k1"), &config()).unwrap_err(),
        JwtRefusal::BadSignature
    );
}

#[test]
fn none_symmetric_and_not_allowed_algorithms_are_refused_each_for_its_reason_before_any_key() {
    for (alg, refusal) in [
        ("none", JwtRefusal::AlgorithmNone),
        ("NONE", JwtRefusal::AlgorithmNone),
        ("HS256", JwtRefusal::SymmetricAlgorithm),
        ("HS512", JwtRefusal::SymmetricAlgorithm),
        ("hs256", JwtRefusal::SymmetricAlgorithm),
        ("RS512", JwtRefusal::AlgorithmNotAllowed),
        ("PS256", JwtRefusal::AlgorithmNotAllowed),
        ("EdDSA", JwtRefusal::AlgorithmNotAllowed),
    ] {
        let token = with_header(json!({"alg": alg, "kid": "k1"}));
        assert_eq!(
            SignedJwt::parse(&token, &config()).unwrap_err(),
            refusal,
            "{alg}"
        );
    }
}

#[test]
fn eddsa_passes_only_where_the_deployment_allows_it() {
    let rng = SystemRandom::new();
    let der = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let pair = Ed25519KeyPair::from_pkcs8(der.as_ref()).unwrap();
    let key = parse_key(&json!({"kty": "OKP", "crv": "Ed25519", "kid": "k1",
        "x": b64(pair.public_key().as_ref())}))
    .unwrap();
    let input = signing_input(&json!({"alg": "EdDSA", "kid": "k1"}), &claims());
    let token = format!("{input}.{}", b64(pair.sign(input.as_bytes()).as_ref()));

    assert_eq!(
        check(&token, &key, &config()).unwrap_err(),
        JwtRefusal::AlgorithmNotAllowed
    );
    let allowing = config().with_algorithms(&[JwsAlgorithm::EdDsa]);
    assert!(check(&token, &key, &allowing).is_ok());
}

#[test]
fn an_issuer_that_differs_even_by_a_trailing_slash_is_refused() {
    for iss in [
        json!("https://idp.example/"),
        json!("https://other.example"),
    ] {
        let claims = edit(|c| {
            c.insert("iss".into(), iss);
        });
        assert_eq!(es256_check(claims).unwrap_err(), JwtRefusal::IssuerMismatch);
    }
    let without = edit(|c| {
        c.remove("iss");
    });
    assert_eq!(
        es256_check(without).unwrap_err(),
        JwtRefusal::IssuerMismatch
    );

    // The same holds the other way: a configured `/` is part of the issuer.
    let signer = Es256::new();
    let slashed = OidcConfig::new("https://idp.example/", AUDIENCE).unwrap();
    assert_eq!(
        check(&signer.sign("k1", &claims()), &signer.key("k1"), &slashed).unwrap_err(),
        JwtRefusal::IssuerMismatch
    );
}

#[test]
fn the_audience_is_one_of_an_array_or_the_only_one() {
    let aud = |value: Value| {
        edit(|c| {
            c.insert("aud".into(), value);
        })
    };
    assert!(es256_check(aud(json!(["other", AUDIENCE]))).is_ok());
    for refused in [
        aud(json!("other")),
        aud(json!(["other"])),
        aud(json!([])),
        aud(json!(1)),
        edit(|c| {
            c.remove("aud");
        }),
    ] {
        assert_eq!(
            es256_check(refused).unwrap_err(),
            JwtRefusal::AudienceMissing
        );
    }
}

#[test]
fn the_times_are_compared_with_the_allowed_clock_skew() {
    let at = |name: &str, value: Value| {
        edit(|c| {
            c.insert(name.into(), value);
        })
    };
    let skew = CLOCK_SKEW_SECS;
    assert_eq!(
        es256_check(at("exp", json!(NOW - skew))).unwrap_err(),
        JwtRefusal::Expired
    );
    assert!(es256_check(at("exp", json!(NOW - skew + 1))).is_ok());
    assert!(es256_check(at("exp", json!(1999.5))).is_ok());
    assert_eq!(
        es256_check(at("nbf", json!(NOW + skew + 1))).unwrap_err(),
        JwtRefusal::NotYetValid
    );
    assert!(es256_check(at("nbf", json!(NOW + skew))).is_ok());
    assert_eq!(
        es256_check(at("iat", json!(NOW + skew + 1))).unwrap_err(),
        JwtRefusal::IssuedInTheFuture
    );
    assert!(es256_check(at("iat", json!(NOW + skew))).is_ok());
    assert_eq!(
        es256_check(edit(|c| {
            c.remove("exp");
        }))
        .unwrap_err(),
        JwtRefusal::ExpiryMissing
    );
    for name in ["exp", "nbf", "iat"] {
        assert_eq!(
            es256_check(at(name, json!("soon"))).unwrap_err(),
            JwtRefusal::Malformed,
            "{name}"
        );
    }
}

#[test]
fn a_token_without_a_subject_is_refused() {
    for claims in [
        edit(|c| {
            c.remove("sub");
        }),
        edit(|c| {
            c.insert("sub".into(), json!(""));
        }),
    ] {
        assert_eq!(es256_check(claims).unwrap_err(), JwtRefusal::SubjectMissing);
    }
}

#[test]
fn a_token_of_the_wrong_form_is_refused() {
    let config = config();
    for token in ["", "a", "a.b", "a.b.c.d", "!!.!!.!!", "e30.e30.AAAA"] {
        assert_eq!(
            SignedJwt::parse(token, &config).unwrap_err(),
            JwtRefusal::Malformed,
            "{token}"
        );
    }
    assert_eq!(
        SignedJwt::parse(&"a".repeat(MAX_JWT_BYTES + 1), &config).unwrap_err(),
        JwtRefusal::TooLong
    );
    let critical = with_header(json!({"alg": "ES256", "crit": ["exp"]}));
    assert_eq!(
        SignedJwt::parse(&critical, &config).unwrap_err(),
        JwtRefusal::CriticalExtension
    );
    // Well signed over claims that are not JSON.
    let signer = Es256::new();
    let input = format!(
        "{}.{}",
        b64(json!({"alg": "ES256", "kid": "k1"}).to_string().as_bytes()),
        b64(b"not json")
    );
    let signature = signer
        .pair
        .sign(&SystemRandom::new(), input.as_bytes())
        .unwrap();
    let token = format!("{input}.{}", b64(signature.as_ref()));
    assert_eq!(
        check(&token, &signer.key("k1"), &config).unwrap_err(),
        JwtRefusal::Malformed
    );
}

#[test]
fn the_identity_is_the_issuer_and_subject_whatever_the_mail_and_name_say() {
    let with = |sub: &str, mail: &str, name: &str| {
        edit(|c| {
            c.insert("sub".into(), json!(sub));
            c.insert("email".into(), json!(mail));
            c.insert("name".into(), json!(name));
        })
    };
    let first = es256_check(with("person-1", "a@example.com", "A")).unwrap();
    let renamed = es256_check(with("person-1", "b@example.com", "B")).unwrap();
    assert_eq!(first.identity, renamed.identity);
    assert_eq!(first.claims["email"], "a@example.com");

    let same_mail = es256_check(with("person-2", "a@example.com", "A")).unwrap();
    assert_ne!(first.identity, same_mail.identity);
}

#[test]
fn nothing_shown_of_a_token_or_its_check_holds_the_token_or_its_claims() {
    let signer = Es256::new();
    let token = signer.sign(
        "k1",
        &edit(|c| {
            c.insert("email".into(), json!("private@example.com"));
        }),
    );
    let parsed = SignedJwt::parse(&token, &config()).unwrap();
    let verified = parsed.verify(&signer.key("k1"), &config(), NOW).unwrap();
    let refusal = parsed
        .verify(&Es256::new().key("k1"), &config(), NOW)
        .unwrap_err();
    for shown in [
        format!("{parsed:?}"),
        format!("{verified:?}"),
        format!("{refusal:?} {refusal}"),
    ] {
        for part in token.split('.') {
            assert!(!shown.contains(part), "{shown}");
        }
        assert!(!shown.contains("private@example.com"), "{shown}");
    }
}

#[test]
fn a_discovery_document_must_name_the_configured_issuer_and_an_https_key_address() {
    let config = config();
    let answer = |issuer: &str, jwks: &str| json!({"issuer": issuer, "jwks_uri": jwks}).to_string();
    let jwks = "https://keys.example/jwks";
    assert_eq!(
        check_discovery(&config, answer(ISSUER, jwks).as_bytes()).unwrap(),
        jwks
    );
    for issuer in ["https://idp.example/", "https://other.example"] {
        assert_eq!(
            check_discovery(&config, answer(issuer, jwks).as_bytes()).unwrap_err(),
            KeyFetchFailure::DiscoveryIssuerMismatch,
            "{issuer}"
        );
    }
    for jwks in ["http://keys.example/jwks", "ftp://k", "https://", "/jwks"] {
        assert_eq!(
            check_discovery(&config, answer(ISSUER, jwks).as_bytes()).unwrap_err(),
            KeyFetchFailure::JwksUriNotHttps,
            "{jwks}"
        );
    }
    assert_eq!(
        check_discovery(&config, b"{}").unwrap_err(),
        KeyFetchFailure::DiscoveryMalformed
    );
}

#[test]
fn the_discovery_address_follows_the_issuer_and_an_issuer_must_be_https() {
    let suffix = "/.well-known/openid-configuration";
    assert_eq!(
        discovery_url("https://idp.example/"),
        format!("https://idp.example{suffix}")
    );
    assert_eq!(
        discovery_url("https://idp.example/tenant"),
        format!("https://idp.example/tenant{suffix}")
    );
    assert_eq!(
        OidcConfig::new("http://idp.example", AUDIENCE).unwrap_err(),
        KeyFetchFailure::IssuerNotHttps
    );
}

#[test]
fn a_key_document_gives_the_keys_this_can_use_and_fails_with_none() {
    let signer = Es256::new();
    let good = signer.jwk("k1");
    let document = json!({"keys": [
        good,
        {"kty": "oct", "k": "AAAA", "kid": "secret"},
        {"kty": "EC", "crv": "P-521", "kid": "other-curve", "x": "AA", "y": "AA"},
        {"kty": "EC", "crv": "P-256", "kid": "short", "x": "AA", "y": "AA"},
        {"kty": "RSA", "kid": "encrypts", "use": "enc", "n": RS256_N, "e": "AQAB"},
        {"kty": "RSA", "kid": "pss", "alg": "PS256", "n": RS256_N, "e": "AQAB"},
        {"kty": "RSA", "kid": "no-exponent", "n": RS256_N},
    ]});
    let keys = parse_keys(document.to_string().as_bytes()).unwrap();
    assert_eq!(keys, vec![signer.key("k1")]);

    assert_eq!(
        parse_keys(br#"{"keys":[]}"#).unwrap_err(),
        KeyFetchFailure::NoUsableKeys
    );
    assert_eq!(
        parse_keys(b"[]").unwrap_err(),
        KeyFetchFailure::KeysMalformed
    );
}

#[test]
fn the_lifetime_of_keys_follows_cache_control_within_its_bounds() {
    assert_eq!(key_lifetime(None), DEFAULT_KEY_LIFETIME_SECS);
    assert_eq!(key_lifetime(Some("public")), DEFAULT_KEY_LIFETIME_SECS);
    assert_eq!(
        key_lifetime(Some("max-age=soon")),
        DEFAULT_KEY_LIFETIME_SECS
    );
    assert_eq!(key_lifetime(Some("public, Max-Age=300")), 300);
    assert_eq!(key_lifetime(Some("max-age=0")), MIN_FETCH_INTERVAL_SECS);
    assert_eq!(
        key_lifetime(Some("max-age=99999999")),
        MAX_KEY_LIFETIME_SECS
    );
    assert_eq!(
        key_lifetime(Some("max-age=300, no-store")),
        MIN_FETCH_INTERVAL_SECS
    );
    assert_eq!(key_lifetime(Some("no-cache")), MIN_FETCH_INTERVAL_SECS);
}

#[test]
fn a_redirect_a_bad_status_and_a_large_answer_are_refused_before_the_body() {
    assert_eq!(check_response(200, None), Ok(()));
    assert_eq!(check_response(200, Some(MAX_DOCUMENT_BYTES)), Ok(()));
    for status in [301, 302, 303, 307, 308] {
        assert_eq!(
            check_response(status, None),
            Err(KeyFetchFailure::Redirected)
        );
    }
    assert_eq!(check_response(204, None), Err(KeyFetchFailure::Status(204)));
    assert_eq!(check_response(500, None), Err(KeyFetchFailure::Status(500)));
    assert_eq!(
        check_response(200, Some(MAX_DOCUMENT_BYTES + 1)),
        Err(KeyFetchFailure::TooLarge)
    );
    assert_eq!(check_body_length(MAX_DOCUMENT_BYTES), Ok(()));
    assert_eq!(
        check_body_length(MAX_DOCUMENT_BYTES + 1),
        Err(KeyFetchFailure::TooLarge)
    );
}

/// One signer and its tokens under the key identifiers `k1` and `k2`.
struct Keys {
    signer: Es256,
    token_k1: String,
    token_k2: String,
}

impl Keys {
    fn new() -> Self {
        let signer = Es256::new();
        Self {
            token_k1: signer.sign("k1", &claims()),
            token_k2: signer.sign("k2", &claims()),
            signer,
        }
    }

    fn parse<'a>(token: &'a str) -> SignedJwt<'a> {
        SignedJwt::parse(token, &config()).unwrap()
    }
}

#[test]
fn a_remembered_key_is_used_without_a_fetch() {
    let keys = Keys::new();
    let mut memory = KeyMemory::default();
    memory.record_fetched(vec![keys.signer.key("k1")], 600, NOW);
    let jwt = Keys::parse(&keys.token_k1);

    assert!(!memory.should_fetch(&jwt, NOW + 1));
    let key = memory.key_for(&jwt, NOW + 1).unwrap();
    assert!(jwt.verify(key, &config(), NOW + 1).is_ok());
}

#[test]
fn an_unknown_key_fetches_once_and_not_again_within_the_shortest_interval() {
    let keys = Keys::new();
    let mut memory = KeyMemory::default();
    memory.record_fetched(vec![keys.signer.key("k1")], 600, NOW);
    let unknown = Keys::parse(&keys.token_k2);

    assert!(!memory.should_fetch(&unknown, NOW + MIN_FETCH_INTERVAL_SECS - 1));
    assert_eq!(
        memory.key_for(&unknown, NOW + 1).unwrap_err(),
        JwtRefusal::UnknownKey
    );

    let later = NOW + MIN_FETCH_INTERVAL_SECS;
    assert!(memory.should_fetch(&unknown, later));
    // The fetch brought the same keys: refused, and no second fetch for it.
    memory.record_fetched(vec![keys.signer.key("k1")], 600, later);
    assert_eq!(
        memory.key_for(&unknown, later).unwrap_err(),
        JwtRefusal::UnknownKey
    );
    assert!(!memory.should_fetch(&unknown, later));

    // The issuer rotated: the new key is taken and the old one is gone.
    let again = later + MIN_FETCH_INTERVAL_SECS;
    assert!(memory.should_fetch(&unknown, again));
    memory.record_fetched(vec![keys.signer.key("k2")], 600, again);
    assert!(memory.key_for(&unknown, again).is_ok());
    assert_eq!(
        memory
            .key_for(&Keys::parse(&keys.token_k1), again)
            .unwrap_err(),
        JwtRefusal::UnknownKey
    );
}

#[test]
fn a_failed_fetch_leaves_known_keys_within_their_time_and_refuses_the_rest() {
    let keys = Keys::new();
    let known = Keys::parse(&keys.token_k1);
    let unknown = Keys::parse(&keys.token_k2);

    let mut memory = KeyMemory::default();
    memory.record_fetched(vec![keys.signer.key("k1")], 600, NOW);
    let failed_at = NOW + MIN_FETCH_INTERVAL_SECS;
    assert!(memory.should_fetch(&unknown, failed_at));
    memory.record_failed(failed_at);

    assert!(memory.key_for(&known, failed_at).is_ok());
    assert_eq!(
        memory.key_for(&unknown, failed_at).unwrap_err(),
        JwtRefusal::UnknownKey
    );
    // The failure counts against the interval too.
    assert!(!memory.should_fetch(&unknown, failed_at + 1));

    // Past their time even a known key is refused until a fetch succeeds.
    let expired = NOW + 600;
    assert_eq!(
        memory.key_for(&known, expired).unwrap_err(),
        JwtRefusal::KeysUnavailable
    );
    assert!(memory.should_fetch(&known, expired));
    memory.record_failed(expired);
    assert_eq!(
        memory.key_for(&known, expired).unwrap_err(),
        JwtRefusal::KeysUnavailable
    );
    assert!(!memory.should_fetch(&known, expired + 1));
}

#[test]
fn an_issuer_with_nothing_remembered_fetches_and_refuses_when_that_fails() {
    let keys = Keys::new();
    let jwt = Keys::parse(&keys.token_k1);
    let mut memory = KeyMemory::default();
    assert_eq!(
        memory.key_for(&jwt, NOW).unwrap_err(),
        JwtRefusal::KeysUnavailable
    );
    assert!(memory.should_fetch(&jwt, NOW));
    memory.record_failed(NOW);
    assert_eq!(
        memory.key_for(&jwt, NOW).unwrap_err(),
        JwtRefusal::KeysUnavailable
    );
    assert!(!memory.should_fetch(&jwt, NOW));
    assert!(memory.should_fetch(&jwt, NOW + MIN_FETCH_INTERVAL_SECS));
    // A clock that went back does not hold fetches off.
    assert!(memory.should_fetch(&jwt, NOW - 1));
}

#[test]
fn a_token_without_a_key_identifier_takes_the_only_key_that_fits() {
    let signer = Es256::new();
    let input = signing_input(&json!({"alg": "ES256"}), &claims());
    let signature = signer
        .pair
        .sign(&SystemRandom::new(), input.as_bytes())
        .unwrap();
    let token = format!("{input}.{}", b64(signature.as_ref()));
    let jwt = Keys::parse(&token);

    let mut one = KeyMemory::default();
    one.record_fetched(vec![signer.key("k1")], 600, NOW);
    assert!(one.key_for(&jwt, NOW).is_ok());

    let mut two = KeyMemory::default();
    two.record_fetched(vec![signer.key("k1"), signer.key("k2")], 600, NOW);
    assert_eq!(two.key_for(&jwt, NOW).unwrap_err(), JwtRefusal::UnknownKey);
}

#[test]
fn a_key_is_not_used_with_an_algorithm_other_than_its_own() {
    let signer = Es256::new();
    let token = signer.sign("k1", &claims());
    let jwt = Keys::parse(&token);
    // The same identifier on a key of another type does not take the token,
    // and neither does an ES256 key that says it signs otherwise.
    let rsa = parse_key(&json!({"kty": "RSA", "kid": "k1", "n": RS256_N, "e": "AQAB"})).unwrap();
    let mut narrowed = signer.jwk("k1");
    narrowed["alg"] = json!("RS256");
    let narrowed = parse_key(&narrowed).unwrap();
    for key in [rsa, narrowed] {
        let mut memory = KeyMemory::default();
        memory.record_fetched(vec![key], 600, NOW);
        assert_eq!(
            memory.key_for(&jwt, NOW).unwrap_err(),
            JwtRefusal::UnknownKey
        );
    }
}

#[test]
fn an_empty_configured_audience_is_refused() {
    assert_eq!(
        OidcConfig::new(ISSUER, "").unwrap_err(),
        KeyFetchFailure::AudienceEmpty
    );
}
