//! The run's token (ADR-t827-2 decisions 2〜4): [`sign`] makes it from
//! [`TokenClaims`] with the queue's [`SigningKey`], [`verify`] reads it back,
//! [`check_active`] is the receiving side of revocation, and
//! [`TokenClaims::require`] / [`TokenClaims::confine`] check what one request
//! asks for.
//!
//! The format is `dagq1.<base64url(claims JSON)>.<base64url(HMAC-SHA256(key,
//! "dagq1." + claims part))>`. Every failure refuses the whole token (fail
//! closed), and no error, `Debug` or `Display` here holds a token, a
//! signature or a key.

use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{BrokerCapability, ErrorCode, TOKEN_VERSION, TokenClaims, decode, encode};

/// What every token starts with: the format and its version.
pub const TOKEN_PREFIX: &str = "dagq1.";

/// The length of a [`SigningKey`] in bytes.
pub const KEY_LEN: usize = 32;

/// The queue's signing key: 32 random bytes. `Debug` does not show them and
/// there is no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct SigningKey([u8; KEY_LEN]);

impl SigningKey {
    /// The key of `bytes`, which must be exactly [`KEY_LEN`] long.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, TokenError> {
        let key: [u8; KEY_LEN] = bytes.try_into().map_err(|_| TokenError::BadKey)?;
        Ok(Self(key))
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKey(<redacted>)")
    }
}

/// A signed token. `Debug` does not show it and there is no `Display`: the
/// value leaves only through [`BrokerSessionToken::expose`], to be written
/// to the run's token file or sent in `Authorization`.
#[derive(Clone, PartialEq, Eq)]
pub struct BrokerSessionToken(String);

impl BrokerSessionToken {
    /// The token as read from a token file or a header (not yet verified).
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token's value, for the token file and the `Authorization` header
    /// only: never for a log, an event or an error.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The claims as written in the token, read without a key: the
    /// signature, the version and the expiry are not checked. For a
    /// person's inspection only (`dagq-broker-client token inspect`),
    /// never for a decision; that is [`verify`]'s.
    pub fn unverified_claims(&self) -> Result<TokenClaims, TokenError> {
        let rest = self
            .0
            .strip_prefix(TOKEN_PREFIX)
            .ok_or(TokenError::Malformed)?;
        let (claims_part, _) = rest.split_once('.').ok_or(TokenError::Malformed)?;
        let json = base64url_decode(claims_part).ok_or(TokenError::Malformed)?;
        decode(&json).map_err(|_| TokenError::BadClaims)
    }
}

impl fmt::Debug for BrokerSessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BrokerSessionToken(<redacted>)")
    }
}

/// Why a token or a request with it was refused. The messages name the
/// reason only, never the token, the signature or the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// The key is not [`KEY_LEN`] bytes.
    BadKey,
    /// Not `dagq1.<claims>.<signature>` in base64url.
    Malformed,
    /// The signature does not match the claims.
    BadSignature,
    /// The claims are not readable: a missing or unknown field, an unknown
    /// capability or role, or a `jti` that is not a plain id.
    BadClaims,
    /// A token format this build does not know.
    UnsupportedVersion,
    /// Past its `exp`.
    Expired,
    /// No active mark for the token's `jti` and run: revoked or never issued.
    Revoked,
    /// The token does not hold the capability the operation needs.
    CapabilityDenied(BrokerCapability),
    /// A path outside the token's workspace.
    WorkspaceViolation,
}

impl TokenError {
    /// The error code the server answers with.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::CapabilityDenied(_) => ErrorCode::CapabilityDenied,
            Self::WorkspaceViolation => ErrorCode::WorkspaceViolation,
            Self::BadKey
            | Self::Malformed
            | Self::BadSignature
            | Self::BadClaims
            | Self::UnsupportedVersion
            | Self::Expired
            | Self::Revoked => ErrorCode::Unauthorized,
        }
    }
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadKey => write!(f, "the broker key is not {KEY_LEN} bytes"),
            Self::Malformed => f.write_str("the token is malformed"),
            Self::BadSignature => f.write_str("the token's signature does not match"),
            Self::BadClaims => f.write_str("the token's claims are not readable"),
            Self::UnsupportedVersion => f.write_str("the token's version is not supported"),
            Self::Expired => f.write_str("the token has expired"),
            Self::Revoked => f.write_str("the token is not active"),
            Self::CapabilityDenied(capability) => {
                write!(f, "the token does not allow {capability}")
            }
            Self::WorkspaceViolation => f.write_str("the path is outside the workspace"),
        }
    }
}

impl std::error::Error for TokenError {}

/// Sign `claims` with `key`.
pub fn sign(key: &SigningKey, claims: &TokenClaims) -> Result<BrokerSessionToken, TokenError> {
    let json = encode(claims).map_err(|_| TokenError::BadClaims)?;
    let mut token = String::from(TOKEN_PREFIX);
    token.push_str(&base64url_encode(&json));
    let mac = hmac_sha256(&key.0, token.as_bytes());
    token.push('.');
    token.push_str(&base64url_encode(&mac));
    Ok(BrokerSessionToken(token))
}

/// Read `token` signed with `key` at `now` (UNIX seconds): its format, its
/// signature (before anything in the claims is read), its claims, their
/// version and their expiry. The active mark is [`check_active`]'s.
pub fn verify(
    key: &SigningKey,
    token: &BrokerSessionToken,
    now: u64,
) -> Result<TokenClaims, TokenError> {
    let rest = token
        .0
        .strip_prefix(TOKEN_PREFIX)
        .ok_or(TokenError::Malformed)?;
    let (claims_part, signature_part) = rest.split_once('.').ok_or(TokenError::Malformed)?;
    let json = base64url_decode(claims_part).ok_or(TokenError::Malformed)?;
    let signature = base64url_decode(signature_part).ok_or(TokenError::Malformed)?;
    let signed = &token.0[..TOKEN_PREFIX.len() + claims_part.len()];
    if !constant_time_eq(&hmac_sha256(&key.0, signed.as_bytes()), &signature) {
        return Err(TokenError::BadSignature);
    }
    let claims: TokenClaims = decode(&json).map_err(|_| TokenError::BadClaims)?;
    if claims.v != TOKEN_VERSION {
        return Err(TokenError::UnsupportedVersion);
    }
    if !is_plain_id(&claims.jti) {
        return Err(TokenError::BadClaims);
    }
    if claims.expired_at(now) {
        return Err(TokenError::Expired);
    }
    Ok(claims)
}

/// Whether `claims` has its active mark `<active_dir>/<jti>` holding its
/// run id (ADR-t827-2 decision 3): the supervisor writes the mark when it
/// issues the token and removes it when it revokes it. No mark, an
/// unreadable one or another run's is [`TokenError::Revoked`].
pub fn check_active(claims: &TokenClaims, active_dir: &Path) -> Result<(), TokenError> {
    if !is_plain_id(&claims.jti) {
        return Err(TokenError::BadClaims);
    }
    match fs::read_to_string(active_dir.join(&claims.jti)) {
        Ok(run) if run.trim() == claims.run_id => Ok(()),
        _ => Err(TokenError::Revoked),
    }
}

impl TokenClaims {
    /// Refuse unless the token allows `capability`.
    pub fn require(&self, capability: BrokerCapability) -> Result<(), TokenError> {
        if self.allows(capability) {
            Ok(())
        } else {
            Err(TokenError::CapabilityDenied(capability))
        }
    }

    /// `path` inside the token's workspace: a relative path is taken from
    /// the workspace, an absolute one must be under it. `..` anywhere, and
    /// an absolute path elsewhere (another run's worktree too), is
    /// [`TokenError::WorkspaceViolation`]. This is the lexical check only:
    /// symlinks are the server's to resolve.
    pub fn confine(&self, path: &Path) -> Result<PathBuf, TokenError> {
        let workspace = Path::new(&self.workspace);
        let relative = if path.is_absolute() {
            path.strip_prefix(workspace)
                .map_err(|_| TokenError::WorkspaceViolation)?
        } else {
            path
        };
        let mut confined = workspace.to_path_buf();
        for component in relative.components() {
            match component {
                Component::Normal(part) => confined.push(part),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(TokenError::WorkspaceViolation);
                }
            }
        }
        Ok(confined)
    }
}

/// A `jti` that is safe as a file name: ASCII letters, digits and `-`.
fn is_plain_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// HMAC-SHA256 (RFC 2104) over `sha2`.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url without padding (RFC 4648 section 5).
fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]));
        }
    }
    out
}

/// The bytes of unpadded base64url `text`, or `None` for anything else
/// (padding, another alphabet, a length no encoding gives, stray bits).
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for chunk in text.as_bytes().chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            let value = ALPHABET.iter().position(|&a| a == c)? as u32;
            n |= value << (18 - 6 * i);
        }
        let bytes = chunk.len() - 1;
        // Bits past the last whole byte must be zero, so each text is the
        // one encoding of its bytes.
        if n & ((1 << (24 - 8 * bytes)) - 1) != 0 {
            return None;
        }
        out.extend((0..bytes).map(|i| (n >> (16 - 8 * i)) as u8));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BrokerRole, Committer};

    const KEY: [u8; KEY_LEN] = [7; KEY_LEN];

    fn key() -> SigningKey {
        SigningKey::from_bytes(&KEY).unwrap()
    }

    fn claims() -> TokenClaims {
        TokenClaims {
            v: TOKEN_VERSION,
            jti: "3f0c6a8e-0000-4000-8000-000000000001".to_owned(),
            actor_id: "worker:run-1".to_owned(),
            role: BrokerRole::Worker,
            task_id: 829,
            run_id: "run-1".to_owned(),
            workspace: "/q/runs/run-1/worktree".to_owned(),
            branch: "dagq/run-1".to_owned(),
            committer: Committer {
                name: "A".to_owned(),
                email: "a@example.com".to_owned(),
            },
            capabilities: [BrokerCapability::FsRead, BrokerCapability::GitRead]
                .into_iter()
                .collect(),
            iat: 100,
            exp: 200,
        }
    }

    fn parts(token: &BrokerSessionToken) -> (String, String) {
        let rest = token.expose().strip_prefix(TOKEN_PREFIX).unwrap();
        let (claims, signature) = rest.split_once('.').unwrap();
        (claims.to_owned(), signature.to_owned())
    }

    #[test]
    fn a_signed_token_verifies_to_its_claims() {
        let token = sign(&key(), &claims()).unwrap();
        assert!(token.expose().starts_with(TOKEN_PREFIX));
        assert_eq!(verify(&key(), &token, 150).unwrap(), claims());
        // The same claims are the same token.
        assert_eq!(sign(&key(), &claims()).unwrap(), token);
    }

    #[test]
    fn unverified_claims_read_without_a_key_and_refuse_other_text() {
        let token = sign(&key(), &claims()).unwrap();
        assert_eq!(token.unverified_claims().unwrap(), claims());
        // Another key's signature does not matter to inspection.
        let (claims_part, _) = parts(&token);
        let resigned = BrokerSessionToken::new(format!("{TOKEN_PREFIX}{claims_part}.AAAA"));
        assert_eq!(resigned.unverified_claims().unwrap(), claims());
        for text in ["", "dagq1.", "dagq2.a.b", "dagq1.!!.x", "dagq1.e30.x"] {
            let error = BrokerSessionToken::new(text)
                .unverified_claims()
                .unwrap_err();
            assert!(
                matches!(error, TokenError::Malformed | TokenError::BadClaims),
                "{text}: {error:?}"
            );
        }
    }

    #[test]
    fn an_expired_token_is_refused() {
        let token = sign(&key(), &claims()).unwrap();
        assert!(verify(&key(), &token, 199).is_ok());
        assert_eq!(verify(&key(), &token, 200), Err(TokenError::Expired));
        assert_eq!(verify(&key(), &token, 10_000), Err(TokenError::Expired));
        assert_eq!(TokenError::Expired.code(), ErrorCode::Unauthorized);
    }

    #[test]
    fn a_tampered_signature_or_another_key_is_refused() {
        let token = sign(&key(), &claims()).unwrap();
        let (claims_part, signature) = parts(&token);
        let mut flipped = signature.into_bytes();
        flipped[0] = if flipped[0] == b'A' { b'B' } else { b'A' };
        let tampered = BrokerSessionToken::new(format!(
            "{TOKEN_PREFIX}{claims_part}.{}",
            String::from_utf8(flipped).unwrap()
        ));
        assert_eq!(
            verify(&key(), &tampered, 150),
            Err(TokenError::BadSignature)
        );
        let other = SigningKey::from_bytes(&[8; KEY_LEN]).unwrap();
        assert_eq!(verify(&other, &token, 150), Err(TokenError::BadSignature));
        // A shortened signature is not a match either.
        let short = BrokerSessionToken::new(format!("{TOKEN_PREFIX}{claims_part}.AAAA"));
        assert_eq!(verify(&key(), &short, 150), Err(TokenError::BadSignature));
    }

    #[test]
    fn tampered_claims_are_refused() {
        let token = sign(&key(), &claims()).unwrap();
        let (_, signature) = parts(&token);
        let changes: [fn(&mut TokenClaims); 4] = [
            |c| c.exp = u64::MAX,
            |c| c.run_id = "run-2".to_owned(),
            |c| c.workspace = "/q/runs/run-2/worktree".to_owned(),
            |c| {
                c.capabilities.insert(BrokerCapability::ProcessExec);
            },
        ];
        for change in changes {
            let mut forged = claims();
            change(&mut forged);
            let forged = BrokerSessionToken::new(format!(
                "{TOKEN_PREFIX}{}.{signature}",
                base64url_encode(&encode(&forged).unwrap())
            ));
            assert_eq!(verify(&key(), &forged, 150), Err(TokenError::BadSignature));
        }
    }

    #[test]
    fn a_malformed_token_is_refused() {
        let token = sign(&key(), &claims()).unwrap();
        let (claims_part, signature) = parts(&token);
        for bad in [
            String::new(),
            "Bearer x".to_owned(),
            format!("dagq2.{claims_part}.{signature}"),
            format!("{TOKEN_PREFIX}{claims_part}"),
            format!("{TOKEN_PREFIX}{claims_part}.{signature}="),
            format!("{TOKEN_PREFIX}{claims_part}+.{signature}"),
            format!("{TOKEN_PREFIX}{claims_part}.{signature}.x"),
        ] {
            assert_eq!(
                verify(&key(), &BrokerSessionToken::new(bad.clone()), 150),
                Err(TokenError::Malformed),
                "{bad}"
            );
        }
    }

    #[test]
    fn signed_but_unreadable_or_unknown_claims_are_refused() {
        let signed = |json: &str| {
            let mut token = format!("{TOKEN_PREFIX}{}", base64url_encode(json.as_bytes()));
            let mac = hmac_sha256(&KEY, token.as_bytes());
            token.push('.');
            token.push_str(&base64url_encode(&mac));
            BrokerSessionToken::new(token)
        };
        let json = String::from_utf8(encode(&claims()).unwrap()).unwrap();
        assert!(verify(&key(), &signed(&json), 150).is_ok());
        let unknown_capability = json.replace(r#""git.read""#, r#""git.push""#);
        assert_eq!(
            verify(&key(), &signed(&unknown_capability), 150),
            Err(TokenError::BadClaims)
        );
        let missing = json.replace(r#""branch":"dagq/run-1","#, "");
        assert_eq!(
            verify(&key(), &signed(&missing), 150),
            Err(TokenError::BadClaims)
        );
        let traversal = json.replace("3f0c6a8e-0000-4000-8000-000000000001", "../../runs/run-1/x");
        assert_eq!(
            verify(&key(), &signed(&traversal), 150),
            Err(TokenError::BadClaims)
        );
        let version = json.replace(r#"{"v":1,"#, r#"{"v":2,"#);
        assert_eq!(
            verify(&key(), &signed(&version), 150),
            Err(TokenError::UnsupportedVersion)
        );
    }

    #[test]
    fn a_missing_capability_is_denied() {
        let claims = claims();
        assert!(claims.require(BrokerCapability::FsRead).is_ok());
        let error = claims.require(BrokerCapability::FsWrite).unwrap_err();
        assert_eq!(
            error,
            TokenError::CapabilityDenied(BrokerCapability::FsWrite)
        );
        assert_eq!(error.code(), ErrorCode::CapabilityDenied);
        assert_eq!(error.to_string(), "the token does not allow fs.write");
    }

    #[test]
    fn paths_are_confined_to_the_workspace() {
        let claims = claims();
        let workspace = Path::new("/q/runs/run-1/worktree");
        assert_eq!(
            claims.confine(Path::new("src/lib.rs")).unwrap(),
            workspace.join("src/lib.rs")
        );
        assert_eq!(
            claims.confine(Path::new("./a/./b")).unwrap(),
            workspace.join("a/b")
        );
        assert_eq!(
            claims
                .confine(Path::new("/q/runs/run-1/worktree/Cargo.toml"))
                .unwrap(),
            workspace.join("Cargo.toml")
        );
        assert_eq!(claims.confine(Path::new("")).unwrap(), workspace);
        for outside in [
            "../run-2/worktree/src",
            "a/../../x",
            "/q/runs/run-2/worktree/src/lib.rs",
            "/q/runs/run-1/worktree-2/x",
            "/q/runs/run-1/worktree/../../run-2/worktree",
            "/etc/passwd",
        ] {
            let error = claims.confine(Path::new(outside)).unwrap_err();
            assert_eq!(error, TokenError::WorkspaceViolation, "{outside}");
            assert_eq!(error.code(), ErrorCode::WorkspaceViolation);
        }
    }

    #[test]
    fn only_a_mark_for_the_run_makes_the_token_active() {
        let dir = std::env::temp_dir().join(format!(
            "dagq-broker-protocol-active-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).unwrap();
        let claims = claims();
        assert_eq!(check_active(&claims, &dir), Err(TokenError::Revoked));
        fs::write(dir.join(&claims.jti), "run-2\n").unwrap();
        assert_eq!(check_active(&claims, &dir), Err(TokenError::Revoked));
        fs::write(dir.join(&claims.jti), "run-1\n").unwrap();
        assert_eq!(check_active(&claims, &dir), Ok(()));
        let mut odd = claims.clone();
        odd.jti = "../x".to_owned();
        assert_eq!(check_active(&odd, &dir), Err(TokenError::BadClaims));
        fs::remove_file(dir.join(&claims.jti)).unwrap();
        assert_eq!(check_active(&claims, &dir), Err(TokenError::Revoked));
        fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn no_message_or_debug_holds_the_token_or_the_key() {
        let token = sign(&key(), &claims()).unwrap();
        let (_, signature) = parts(&token);
        assert_eq!(format!("{token:?}"), "BrokerSessionToken(<redacted>)");
        assert_eq!(format!("{:?}", key()), "SigningKey(<redacted>)");
        let key_text = format!("{KEY:?}");
        let errors = [
            TokenError::BadKey,
            TokenError::Malformed,
            TokenError::BadSignature,
            TokenError::BadClaims,
            TokenError::UnsupportedVersion,
            TokenError::Expired,
            TokenError::Revoked,
            TokenError::CapabilityDenied(BrokerCapability::GitWrite),
            TokenError::WorkspaceViolation,
        ];
        for error in errors {
            for text in [error.to_string(), format!("{error:?}")] {
                assert!(!text.contains(&signature), "{text}");
                assert!(!text.contains(token.expose()), "{text}");
                assert!(!text.contains(&key_text), "{text}");
            }
        }
        assert_eq!(SigningKey::from_bytes(&[1; 31]), Err(TokenError::BadKey));
        assert_eq!(TokenError::BadKey.code(), ErrorCode::Unauthorized);
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 2: key "Jefe".
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block is hashed first.
        let mac = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn base64url_round_trips_and_refuses_other_text() {
        for (bytes, text) in [
            (&b""[..], ""),
            (b"f", "Zg"),
            (b"fo", "Zm8"),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg"),
            (&[0xfb, 0xff][..], "-_8"),
        ] {
            assert_eq!(base64url_encode(bytes), text);
            assert_eq!(base64url_decode(text).as_deref(), Some(bytes));
        }
        for bad in ["Z", "Zg==", "Zh", "Zm+v", "Zm/v", "Zm9v!"] {
            assert_eq!(base64url_decode(bad), None, "{bad}");
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
