//! The check of an OIDC issuer's JWT (ADR-t3985-1 decision 1): which token
//! passes under a deployment's issuer and audience, which answers of the
//! issuer's discovery and key documents to believe, and when the remembered
//! keys are used, fetched again or refused. Everything takes the time and
//! the documents as values; the fetching and its order are
//! [`crate::application::oidc`]'s and the HTTPS is the adapter's.
//!
//! Only this comparison decides who a token is: a person is the pair of
//! issuer and subject ([`OidcIdentity`]), never a mail address or a name.
//!
//! The token's value appears in no type's `Debug` or `Display` here, so a
//! refusal can be logged or recorded as it is.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{
    ECDSA_P256_SHA256_FIXED, ED25519, RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents,
    UnparsedPublicKey,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The clock difference allowed on `exp`, `nbf` and `iat`: 60 seconds.
pub const CLOCK_SKEW_SECS: i64 = 60;
/// The shortest time between two fetches of an issuer's keys: 60 seconds.
/// Tokens with made-up key identifiers therefore cost the issuer at most one
/// fetch a minute, and a key an issuer has just rotated in is usable within
/// a minute of its first token.
pub const MIN_FETCH_INTERVAL_SECS: i64 = 60;
/// How long fetched keys are remembered when the answer's `Cache-Control`
/// names no lifetime: one hour.
pub const DEFAULT_KEY_LIFETIME_SECS: i64 = 3600;
/// The longest an answer's `Cache-Control` can keep keys: one day, so a key
/// an issuer withdrew does not stay usable for long on a long `max-age`.
pub const MAX_KEY_LIFETIME_SECS: i64 = 86_400;
/// The largest discovery or key document read: 256 KiB. The documents of the
/// common issuers are a few KiB.
pub const MAX_DOCUMENT_BYTES: u64 = 256 * 1024;
/// How long one document's fetch may take, its body included: 10 seconds.
pub const FETCH_TIMEOUT_SECS: u64 = 10;
/// The longest token looked at: 16 KiB, larger than a token with many claims
/// and small enough to refuse before any decoding.
pub const MAX_JWT_BYTES: usize = 16 * 1024;

/// A signature algorithm a deployment can allow. The type has no symmetric
/// algorithm and no `none`, so no configuration can allow them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JwsAlgorithm {
    Rs256,
    Es256,
    EdDsa,
}

/// The algorithms allowed unless the deployment says otherwise.
pub const DEFAULT_ALGORITHMS: &[JwsAlgorithm] = &[JwsAlgorithm::Rs256, JwsAlgorithm::Es256];

impl JwsAlgorithm {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "RS256" => Some(Self::Rs256),
            "ES256" => Some(Self::Es256),
            "EdDSA" => Some(Self::EdDsa),
            _ => None,
        }
    }
}

/// What a deployment's configuration says about its issuer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcConfig {
    issuer: String,
    audience: String,
    algorithms: Vec<JwsAlgorithm>,
}

impl OidcConfig {
    /// The issuer is kept exactly as written (a trailing `/` is part of it)
    /// and has to be `https`, since its keys are trusted from that answer.
    /// The audience must be nonempty. The algorithms are [`DEFAULT_ALGORITHMS`].
    pub fn new(issuer: &str, audience: &str) -> Result<Self, KeyFetchFailure> {
        if !is_https(issuer) {
            return Err(KeyFetchFailure::IssuerNotHttps);
        }
        if audience.is_empty() {
            return Err(KeyFetchFailure::AudienceEmpty);
        }
        Ok(Self {
            issuer: issuer.to_string(),
            audience: audience.to_string(),
            algorithms: DEFAULT_ALGORITHMS.to_vec(),
        })
    }

    pub fn with_algorithms(mut self, algorithms: &[JwsAlgorithm]) -> Self {
        self.algorithms = algorithms.to_vec();
        self
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }
}

/// Who a token says it is, the whole of a person's identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OidcIdentity {
    pub issuer: String,
    pub subject: String,
}

/// A token that passed: its identity and all its claims (for what is only
/// shown, like a name, and for what a deployment binds besides the identity).
/// `Debug` shows only the identity, since the claims may hold personal data.
#[derive(Clone, PartialEq)]
pub struct VerifiedJwt {
    pub identity: OidcIdentity,
    pub claims: Map<String, Value>,
}

impl fmt::Debug for VerifiedJwt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifiedJwt")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// Why a token was refused. No variant holds anything from the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JwtRefusal {
    /// Not three base64url parts of JSON, or a claim of the wrong type.
    Malformed,
    /// Larger than [`MAX_JWT_BYTES`].
    TooLong,
    /// The header's `crit` names extensions, and none is understood here.
    CriticalExtension,
    AlgorithmNone,
    /// `HS*`: the secret would be the public key or something shared.
    SymmetricAlgorithm,
    AlgorithmNotAllowed,
    /// The issuer's keys are remembered and valid, without this token's key.
    UnknownKey,
    /// No valid remembered keys, and they could not be fetched now.
    KeysUnavailable,
    BadSignature,
    IssuerMismatch,
    AudienceMissing,
    ExpiryMissing,
    Expired,
    NotYetValid,
    IssuedInTheFuture,
    SubjectMissing,
}

impl fmt::Display for JwtRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "the token is malformed",
            Self::TooLong => "the token is too long",
            Self::CriticalExtension => "the token requires an extension that is not understood",
            Self::AlgorithmNone => "the token is unsigned (alg none)",
            Self::SymmetricAlgorithm => "the token uses a symmetric algorithm",
            Self::AlgorithmNotAllowed => "the token's algorithm is not allowed",
            Self::UnknownKey => "the token's key is not among the issuer's keys",
            Self::KeysUnavailable => "the issuer's keys are not available",
            Self::BadSignature => "the token's signature does not verify",
            Self::IssuerMismatch => "the token's issuer is not the configured one",
            Self::AudienceMissing => "the token's audience does not include the configured one",
            Self::ExpiryMissing => "the token has no expiry",
            Self::Expired => "the token has expired",
            Self::NotYetValid => "the token is not valid yet",
            Self::IssuedInTheFuture => "the token was issued in the future",
            Self::SubjectMissing => "the token has no subject",
        })
    }
}

impl std::error::Error for JwtRefusal {}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: Option<String>,
    crit: Option<Value>,
}

/// A token whose form and algorithm passed, not yet its signature or claims.
pub struct SignedJwt<'a> {
    algorithm: JwsAlgorithm,
    kid: Option<String>,
    signing_input: &'a str,
    claims: &'a str,
    signature: Vec<u8>,
}

impl fmt::Debug for SignedJwt<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignedJwt")
            .field("algorithm", &self.algorithm)
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl<'a> SignedJwt<'a> {
    /// The checks before any key is looked at, so a token that could never
    /// pass costs no fetch.
    pub fn parse(token: &'a str, config: &OidcConfig) -> Result<Self, JwtRefusal> {
        if token.len() > MAX_JWT_BYTES {
            return Err(JwtRefusal::TooLong);
        }
        let mut parts = token.split('.');
        let (Some(header), Some(claims), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(JwtRefusal::Malformed);
        };
        let header: Header =
            serde_json::from_slice(&decode(header)?).map_err(|_| JwtRefusal::Malformed)?;
        if header.crit.is_some() {
            return Err(JwtRefusal::CriticalExtension);
        }
        let algorithm = allowed_algorithm(&header.alg, &config.algorithms)?;
        Ok(Self {
            algorithm,
            kid: header.kid,
            signing_input: &token[..token.len() - signature.len() - 1],
            claims,
            signature: decode(signature)?,
        })
    }

    /// The signature under `key`, then the claims against the configuration
    /// at `now` (Unix seconds).
    pub fn verify(
        &self,
        key: &PublicKey,
        config: &OidcConfig,
        now: i64,
    ) -> Result<VerifiedJwt, JwtRefusal> {
        key.verify(
            self.algorithm,
            self.signing_input.as_bytes(),
            &self.signature,
        )?;
        let claims =
            serde_json::from_slice(&decode(self.claims)?).map_err(|_| JwtRefusal::Malformed)?;
        check_claims(claims, config, now)
    }
}

fn decode(part: &str) -> Result<Vec<u8>, JwtRefusal> {
    URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| JwtRefusal::Malformed)
}

fn allowed_algorithm(name: &str, allowed: &[JwsAlgorithm]) -> Result<JwsAlgorithm, JwtRefusal> {
    if name.eq_ignore_ascii_case("none") {
        return Err(JwtRefusal::AlgorithmNone);
    }
    if name.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("HS")) {
        return Err(JwtRefusal::SymmetricAlgorithm);
    }
    JwsAlgorithm::from_name(name)
        .filter(|a| allowed.contains(a))
        .ok_or(JwtRefusal::AlgorithmNotAllowed)
}

fn check_claims(
    claims: Map<String, Value>,
    config: &OidcConfig,
    now: i64,
) -> Result<VerifiedJwt, JwtRefusal> {
    if claims.get("iss").and_then(Value::as_str) != Some(config.issuer.as_str()) {
        return Err(JwtRefusal::IssuerMismatch);
    }
    let audience_included = match claims.get("aud") {
        Some(Value::String(one)) => *one == config.audience,
        Some(Value::Array(many)) => many
            .iter()
            .any(|a| a.as_str() == Some(config.audience.as_str())),
        _ => false,
    };
    if !audience_included {
        return Err(JwtRefusal::AudienceMissing);
    }
    // A NumericDate may have a fraction, and comparing as f64 is exact well
    // past any date a token holds.
    let now = now as f64;
    let skew = CLOCK_SKEW_SECS as f64;
    let exp = time_claim(&claims, "exp")?.ok_or(JwtRefusal::ExpiryMissing)?;
    if now >= exp + skew {
        return Err(JwtRefusal::Expired);
    }
    if time_claim(&claims, "nbf")?.is_some_and(|nbf| now + skew < nbf) {
        return Err(JwtRefusal::NotYetValid);
    }
    if time_claim(&claims, "iat")?.is_some_and(|iat| now + skew < iat) {
        return Err(JwtRefusal::IssuedInTheFuture);
    }
    let subject = match claims.get("sub").and_then(Value::as_str) {
        Some(subject) if !subject.is_empty() => subject.to_string(),
        _ => return Err(JwtRefusal::SubjectMissing),
    };
    Ok(VerifiedJwt {
        identity: OidcIdentity {
            issuer: config.issuer.clone(),
            subject,
        },
        claims,
    })
}

fn time_claim(claims: &Map<String, Value>, name: &str) -> Result<Option<f64>, JwtRefusal> {
    match claims.get(name) {
        None => Ok(None),
        Some(value) => value.as_f64().map(Some).ok_or(JwtRefusal::Malformed),
    }
}

/// An issuer's public key from its key document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    kid: Option<String>,
    /// The JWK's own `alg`, which then is the only algorithm it signs with.
    algorithm: Option<JwsAlgorithm>,
    material: KeyMaterial,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum KeyMaterial {
    Rsa {
        n: Vec<u8>,
        e: Vec<u8>,
    },
    /// The uncompressed point, as the verifier takes it.
    P256(Vec<u8>),
    Ed25519(Vec<u8>),
}

impl PublicKey {
    fn fits(&self, algorithm: JwsAlgorithm) -> bool {
        self.algorithm.is_none_or(|own| own == algorithm)
            && matches!(
                (&self.material, algorithm),
                (KeyMaterial::Rsa { .. }, JwsAlgorithm::Rs256)
                    | (KeyMaterial::P256(_), JwsAlgorithm::Es256)
                    | (KeyMaterial::Ed25519(_), JwsAlgorithm::EdDsa)
            )
    }

    fn verify(
        &self,
        algorithm: JwsAlgorithm,
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), JwtRefusal> {
        let verified = match (&self.material, algorithm) {
            (KeyMaterial::Rsa { n, e }, JwsAlgorithm::Rs256) => RsaPublicKeyComponents { n, e }
                .verify(&RSA_PKCS1_2048_8192_SHA256, message, signature),
            (KeyMaterial::P256(point), JwsAlgorithm::Es256) => {
                UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point).verify(message, signature)
            }
            (KeyMaterial::Ed25519(x), JwsAlgorithm::EdDsa) => {
                UnparsedPublicKey::new(&ED25519, x).verify(message, signature)
            }
            _ => return Err(JwtRefusal::BadSignature),
        };
        verified.map_err(|_| JwtRefusal::BadSignature)
    }
}

/// Why fetching an issuer's keys came to nothing. The first group is the
/// answer's form as the adapter meets it ([`check_response`],
/// [`check_body_length`]), the second what the documents say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyFetchFailure {
    /// A redirect, which is never followed: a key document must come from
    /// the address the issuer's own answer names.
    Redirected,
    Status(u16),
    /// Over [`MAX_DOCUMENT_BYTES`], declared or read.
    TooLarge,
    /// The connection, TLS or a time limit, in the transport's words.
    Unreachable(String),
    IssuerNotHttps,
    AudienceEmpty,
    DiscoveryMalformed,
    DiscoveryIssuerMismatch,
    JwksUriNotHttps,
    KeysMalformed,
    /// The key document holds no key usable here.
    NoUsableKeys,
}

impl fmt::Display for KeyFetchFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redirected => f.write_str("the answer is a redirect, which is not followed"),
            Self::Status(status) => write!(f, "the answer's status is {status}"),
            Self::TooLarge => write!(f, "the answer is over {MAX_DOCUMENT_BYTES} bytes"),
            Self::Unreachable(why) => write!(f, "the fetch failed: {why}"),
            Self::AudienceEmpty => f.write_str("the audience is empty"),
            Self::IssuerNotHttps => f.write_str("the issuer is not an https address"),
            Self::DiscoveryMalformed => f.write_str("the discovery document is malformed"),
            Self::DiscoveryIssuerMismatch => {
                f.write_str("the discovery document's issuer is not the configured one")
            }
            Self::JwksUriNotHttps => f.write_str("the discovery document's jwks_uri is not https"),
            Self::KeysMalformed => f.write_str("the key document is malformed"),
            Self::NoUsableKeys => f.write_str("the key document holds no usable key"),
        }
    }
}

impl std::error::Error for KeyFetchFailure {}

/// Whether to read an answer's body. Called before reading, with its status
/// and declared length.
pub fn check_response(status: u16, declared_length: Option<u64>) -> Result<(), KeyFetchFailure> {
    match status {
        200 => {}
        300..=399 => return Err(KeyFetchFailure::Redirected),
        other => return Err(KeyFetchFailure::Status(other)),
    }
    check_body_length(declared_length.unwrap_or(0))
}

/// Whether a body is within the limit; the reader stops one byte past it, so
/// a longer body is refused without reading the rest.
pub fn check_body_length(length: u64) -> Result<(), KeyFetchFailure> {
    if length > MAX_DOCUMENT_BYTES {
        return Err(KeyFetchFailure::TooLarge);
    }
    Ok(())
}

/// Where an issuer's discovery document is: `/.well-known/...` after the
/// issuer without its trailing `/` (OIDC Discovery section 4.1).
pub fn discovery_url(issuer: &str) -> String {
    let base = issuer.strip_suffix('/').unwrap_or(issuer);
    format!("{base}/.well-known/openid-configuration")
}

fn is_https(url: &str) -> bool {
    url.strip_prefix("https://")
        .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'))
}

/// Discovery issuer matching shared by the control side and the public CLI.
/// Exact equality includes any trailing slash; no endpoint policy is implied.
pub fn check_discovery_issuer(expected: &str, actual: &str) -> Result<(), KeyFetchFailure> {
    if expected == actual {
        Ok(())
    } else {
        Err(KeyFetchFailure::DiscoveryIssuerMismatch)
    }
}

/// The address of the key document, from a discovery document that names
/// exactly the configured issuer (OIDC Discovery section 4.3).
pub fn check_discovery(config: &OidcConfig, body: &[u8]) -> Result<String, KeyFetchFailure> {
    #[derive(Deserialize)]
    struct Discovery {
        issuer: String,
        jwks_uri: String,
    }
    let discovery: Discovery =
        serde_json::from_slice(body).map_err(|_| KeyFetchFailure::DiscoveryMalformed)?;
    check_discovery_issuer(&config.issuer, &discovery.issuer)?;
    if !is_https(&discovery.jwks_uri) {
        return Err(KeyFetchFailure::JwksUriNotHttps);
    }
    Ok(discovery.jwks_uri)
}

/// The keys of a key document this module can use. A key of another type or
/// use is left out, as RFC 7517 section 5 asks; a document with none left is
/// a failure, so it does not replace keys that work.
pub fn parse_keys(body: &[u8]) -> Result<Vec<PublicKey>, KeyFetchFailure> {
    #[derive(Deserialize)]
    struct KeySet {
        keys: Vec<Value>,
    }
    let set: KeySet = serde_json::from_slice(body).map_err(|_| KeyFetchFailure::KeysMalformed)?;
    let keys: Vec<PublicKey> = set.keys.iter().filter_map(parse_key).collect();
    if keys.is_empty() {
        return Err(KeyFetchFailure::NoUsableKeys);
    }
    Ok(keys)
}

fn parse_key(key: &Value) -> Option<PublicKey> {
    if key.get("use").is_some_and(|u| u != "sig") {
        return None;
    }
    let algorithm = match key.get("alg") {
        None => None,
        Some(alg) => Some(JwsAlgorithm::from_name(alg.as_str()?)?),
    };
    let bytes = |name: &str| {
        key.get(name)
            .and_then(Value::as_str)
            .and_then(|v| URL_SAFE_NO_PAD.decode(v).ok())
    };
    let curve = key.get("crv").and_then(Value::as_str);
    let material = match (key.get("kty").and_then(Value::as_str)?, curve) {
        ("RSA", _) => KeyMaterial::Rsa {
            n: bytes("n")?,
            e: bytes("e")?,
        },
        ("EC", Some("P-256")) => {
            let (x, y) = (bytes("x")?, bytes("y")?);
            if x.len() != 32 || y.len() != 32 {
                return None;
            }
            KeyMaterial::P256([&[4][..], &x, &y].concat())
        }
        ("OKP", Some("Ed25519")) => KeyMaterial::Ed25519(bytes("x")?),
        _ => return None,
    };
    let kid = key.get("kid").and_then(Value::as_str).map(str::to_string);
    Some(PublicKey {
        kid,
        algorithm,
        material,
    })
}

/// How long to remember keys from the `Cache-Control` of their answer:
/// `max-age` within [`MIN_FETCH_INTERVAL_SECS`] and
/// [`MAX_KEY_LIFETIME_SECS`], [`DEFAULT_KEY_LIFETIME_SECS`] without one.
/// `no-store` and `no-cache` count as the shortest time: keys that could not
/// be kept until the next fetch is allowed would refuse every good token in
/// between.
pub fn key_lifetime(cache_control: Option<&str>) -> i64 {
    let mut lifetime = DEFAULT_KEY_LIFETIME_SECS;
    for directive in cache_control.unwrap_or("").split(',') {
        let directive = directive.trim().to_ascii_lowercase();
        if directive == "no-store" || directive == "no-cache" {
            return MIN_FETCH_INTERVAL_SECS;
        }
        if let Some(age) = directive.strip_prefix("max-age=") {
            lifetime = age.parse().unwrap_or(lifetime);
        }
    }
    lifetime.clamp(MIN_FETCH_INTERVAL_SECS, MAX_KEY_LIFETIME_SECS)
}

/// What is remembered of one issuer's keys. Keys are only replaced by a
/// fetch that succeeded, so a failed fetch leaves valid keys usable, and they
/// are only used until [`Self::expires_at`], so an issuer out of reach fails
/// closed once that passes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyMemory {
    keys: Vec<PublicKey>,
    expires_at: i64,
    /// The last fetch, whichever way it ended: a failing issuer is not asked
    /// more often than a working one.
    last_fetch: Option<i64>,
}

impl KeyMemory {
    /// The valid remembered key of this token, or why there is none now.
    /// A token without `kid` takes the only key there is (OIDC Core section
    /// 10.1 allows that with one key in the set).
    ///
    /// A key found here is final until its time ends: a signature that fails
    /// under it fetches nothing, so an issuer that changes a key without
    /// changing its identifier, or without using identifiers, is refused
    /// until then.
    pub fn key_for(&self, jwt: &SignedJwt<'_>, now: i64) -> Result<&PublicKey, JwtRefusal> {
        if now >= self.expires_at {
            return Err(JwtRefusal::KeysUnavailable);
        }
        let fitting = |key: &&PublicKey| key.fits(jwt.algorithm);
        match &jwt.kid {
            Some(kid) => self
                .keys
                .iter()
                .filter(fitting)
                .find(|key| key.kid.as_ref() == Some(kid)),
            None => match self.keys.as_slice() {
                [only] => Some(only).filter(fitting),
                _ => None,
            },
        }
        .ok_or(JwtRefusal::UnknownKey)
    }

    /// Whether to fetch before answering this token: only when no valid
    /// remembered key answers it and the shortest interval has passed. One
    /// token leads to at most one fetch, since [`Self::key_for`]'s answer
    /// after it is final.
    pub fn should_fetch(&self, jwt: &SignedJwt<'_>, now: i64) -> bool {
        self.key_for(jwt, now).is_err()
            && self.last_fetch.is_none_or(|last| {
                // A clock that went back does not hold fetches off until it
                // catches up.
                now < last || now - last >= MIN_FETCH_INTERVAL_SECS
            })
    }

    pub fn record_fetched(&mut self, keys: Vec<PublicKey>, lifetime_secs: i64, now: i64) {
        self.keys = keys;
        self.expires_at = now + lifetime_secs;
        self.last_fetch = Some(now);
    }

    pub fn record_failed(&mut self, now: i64) {
        self.last_fetch = Some(now);
    }
}

#[cfg(test)]
mod tests;
