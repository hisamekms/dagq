//! The wire protocol of the dagq resource broker ([Broker], ADR-t827-1 and
//! ADR-t827-2): what `dagq` (which issues the tokens), `dagq-broker` (the
//! server) and `dagq-broker-client` share.
//!
//! Every type fails closed on what it does not know: an unknown capability,
//! error code or field is an error when read, never ignored. Every type
//! serializes deterministically: fields in their declared order, sets and
//! maps in the order of their keys ([`encode`]), so the same value is always
//! the same bytes (the claims are signed over their bytes).
//!
//! [Broker]: https://github.com/hisamekms/dagq/blob/main/docs/design/broker.md

pub mod build_id;
mod capability;
mod claims;
mod error;
pub mod fs;
pub mod git;
mod operation;
pub mod process;
mod request_id;
mod token;

pub use capability::{BrokerCapability, UnknownCapability};
pub use claims::{BrokerRole, Committer, TokenClaims};
pub use error::{BrokerError, ErrorBody, ErrorCode};
pub use operation::Operation;
pub use request_id::BrokerRequestId;
pub use token::{
    BrokerSessionToken, KEY_LEN, SigningKey, TOKEN_PREFIX, TokenError, check_active, sign, verify,
};

use serde::{Deserialize, Serialize};

/// The version of this protocol, sent both ways in [`PROTOCOL_HEADER`]. A
/// request of another version is `invalid_request`.
pub const PROTOCOL_VERSION: u32 = 1;

/// The header that carries [`PROTOCOL_VERSION`] on requests and responses.
pub const PROTOCOL_HEADER: &str = "X-Dagq-Broker-Protocol";

/// The response header that carries the server's build identifier.
pub const BUILD_HEADER: &str = "X-Dagq-Broker-Build";

/// The largest request body the server reads, 8 MiB.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// The version of the token format, the `v` of [`TokenClaims`].
pub const TOKEN_VERSION: u32 = 1;

/// `GET /v1/health`: needs no token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthResponse {
    /// Always `ok` when the server answers.
    pub status: String,
    /// The server's build identifier (`X.Y.Z` or `X.Y.Z-dev+<commit>`).
    pub build: String,
    /// The server's [`PROTOCOL_VERSION`].
    pub protocol: u32,
}

impl HealthResponse {
    /// The answer of a healthy server of `build`.
    pub fn ok(build: impl Into<String>) -> Self {
        Self {
            status: "ok".to_owned(),
            build: build.into(),
            protocol: PROTOCOL_VERSION,
        }
    }
}

/// The bytes of `value` as JSON, the same bytes for the same value.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(value)
}

/// Read `bytes` as a `T`, refusing unknown fields and names.
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_is_the_documented_body() {
        let health = HealthResponse::ok("0.4.0-dev+abc");
        assert_eq!(
            String::from_utf8(encode(&health).unwrap()).unwrap(),
            r#"{"status":"ok","build":"0.4.0-dev+abc","protocol":1}"#
        );
        assert_eq!(
            decode::<HealthResponse>(&encode(&health).unwrap()).unwrap(),
            health
        );
    }

    #[test]
    fn health_refuses_an_unknown_field() {
        let error =
            decode::<HealthResponse>(br#"{"status":"ok","build":"x","protocol":1,"extra":true}"#)
                .unwrap_err();
        assert!(
            error.to_string().contains("unknown field `extra`"),
            "{error}"
        );
    }
}
