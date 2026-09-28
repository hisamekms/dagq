//! The broker's errors: [`ErrorCode`] and the body [`ErrorBody`] that
//! carries a [`BrokerError`].

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::BrokerRequestId;

/// What went wrong, each with its HTTP status. A code this build does not
/// know is refused when read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No token, a malformed or badly signed or expired token, no active
    /// mark, an unknown capability or missing claims.
    Unauthorized,
    /// The token lacks the capability the operation needs, or the program is
    /// not allowed.
    CapabilityDenied,
    /// Outside the workspace, `..`, a symlink out, the worktree's `.git`, a
    /// commit on another branch.
    WorkspaceViolation,
    /// The process ran out of time and was stopped.
    Timeout,
    /// The output or the answer, or the content to write, was over the
    /// limit.
    OutputLimit,
    /// fs, process or git failed.
    BackendError,
    /// An unknown path or field, a wrong type, another protocol version, a
    /// body over the limit, an edit that did not match.
    InvalidRequest,
}

impl ErrorCode {
    /// Every code, in order.
    pub const ALL: [ErrorCode; 7] = [
        Self::Unauthorized,
        Self::CapabilityDenied,
        Self::WorkspaceViolation,
        Self::Timeout,
        Self::OutputLimit,
        Self::BackendError,
        Self::InvalidRequest,
    ];

    /// The name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::CapabilityDenied => "capability_denied",
            Self::WorkspaceViolation => "workspace_violation",
            Self::Timeout => "timeout",
            Self::OutputLimit => "output_limit",
            Self::BackendError => "backend_error",
            Self::InvalidRequest => "invalid_request",
        }
    }

    /// The HTTP status the server answers with.
    pub fn http_status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::CapabilityDenied | Self::WorkspaceViolation => 403,
            Self::Timeout => 504,
            Self::OutputLimit => 413,
            Self::BackendError => 502,
            Self::InvalidRequest => 400,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One error. `message` is a short sentence for a person and never holds a
/// token, a file's content or an env value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerError {
    pub code: ErrorCode,
    pub message: String,
    pub request_id: BrokerRequestId,
}

impl BrokerError {
    pub fn new(code: ErrorCode, message: impl Into<String>, request_id: BrokerRequestId) -> Self {
        Self {
            code,
            message: message.into(),
            request_id,
        }
    }
}

impl fmt::Display for BrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} (request {})",
            self.code, self.message, self.request_id
        )
    }
}

impl std::error::Error for BrokerError {}

/// The body of an error response, `{"error": {...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorBody {
    pub error: BrokerError,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    #[test]
    fn codes_have_their_names_and_statuses() {
        let table: Vec<_> = ErrorCode::ALL
            .iter()
            .map(|code| (code.as_str(), code.http_status()))
            .collect();
        assert_eq!(
            table,
            [
                ("unauthorized", 401),
                ("capability_denied", 403),
                ("workspace_violation", 403),
                ("timeout", 504),
                ("output_limit", 413),
                ("backend_error", 502),
                ("invalid_request", 400),
            ]
        );
        for code in ErrorCode::ALL {
            assert_eq!(serde_json::to_string(&code).unwrap(), format!("\"{code}\""));
        }
    }

    #[test]
    fn the_body_is_the_documented_shape() {
        let body = ErrorBody {
            error: BrokerError::new(
                ErrorCode::WorkspaceViolation,
                "outside the workspace",
                BrokerRequestId::new("r-1"),
            ),
        };
        let json = r#"{"error":{"code":"workspace_violation","message":"outside the workspace","request_id":"r-1"}}"#;
        assert_eq!(String::from_utf8(encode(&body).unwrap()).unwrap(), json);
        assert_eq!(decode::<ErrorBody>(json.as_bytes()).unwrap(), body);
        assert_eq!(
            body.error.to_string(),
            "workspace_violation: outside the workspace (request r-1)"
        );
    }

    #[test]
    fn an_unknown_code_or_field_is_refused() {
        assert!(
            decode::<ErrorBody>(br#"{"error":{"code":"teapot","message":"m","request_id":"r"}}"#)
                .is_err()
        );
        assert!(
            decode::<ErrorBody>(
                br#"{"error":{"code":"timeout","message":"m","request_id":"r","token":"t"}}"#
            )
            .is_err()
        );
        assert!(
            decode::<ErrorBody>(
                br#"{"error":{"code":"timeout","message":"m","request_id":"r"},"x":1}"#
            )
            .is_err()
        );
    }
}
