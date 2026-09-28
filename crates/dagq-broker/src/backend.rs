//! [`Backend`]: what does an operation once the server has authenticated
//! the token, checked its capability and read the request. The server holds
//! one backend each for fs, process and git ([`Backends`]); until they are
//! written, [`Unimplemented`] answers every operation with `backend_error`.

use std::path::PathBuf;
use std::sync::Arc;

use dagq_broker_protocol::{ErrorCode, Operation, TokenClaims, decode, fs, git, process};

use crate::config::Limits;

/// Which backend an operation belongs to, the `<backend>` of its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Fs,
    Process,
    Git,
}

impl BackendKind {
    /// The backend of `operation`; `None` for health.
    pub fn of(operation: Operation) -> Option<Self> {
        match operation {
            Operation::Health => None,
            Operation::FsRead | Operation::FsList | Operation::FsWrite | Operation::FsEdit => {
                Some(Self::Fs)
            }
            Operation::ProcessExec => Some(Self::Process),
            Operation::GitStatus
            | Operation::GitDiff
            | Operation::GitLog
            | Operation::GitShow
            | Operation::GitAdd
            | Operation::GitCommit
            | Operation::GitRestore => Some(Self::Git),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Fs => "fs",
            Self::Process => "process",
            Self::Git => "git",
        }
    }
}

/// A request read as its operation's type (unknown fields refused).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendRequest {
    FsRead(fs::ReadRequest),
    FsList(fs::ListRequest),
    FsWrite(fs::WriteRequest),
    FsEdit(fs::EditRequest),
    ProcessExec(process::ExecRequest),
    GitStatus(git::StatusRequest),
    GitDiff(git::DiffRequest),
    GitLog(git::LogRequest),
    GitShow(git::ShowRequest),
    GitAdd(git::AddRequest),
    GitCommit(git::CommitRequest),
    GitRestore(git::RestoreRequest),
}

impl BackendRequest {
    /// Read `body` as the request of `operation`; `None` for health or a
    /// body that is not the operation's request.
    pub fn decode(operation: Operation, body: &[u8]) -> Option<Self> {
        let request = match operation {
            Operation::Health => return None,
            Operation::FsRead => Self::FsRead(decode(body).ok()?),
            Operation::FsList => Self::FsList(decode(body).ok()?),
            Operation::FsWrite => Self::FsWrite(decode(body).ok()?),
            Operation::FsEdit => Self::FsEdit(decode(body).ok()?),
            Operation::ProcessExec => Self::ProcessExec(decode(body).ok()?),
            Operation::GitStatus => Self::GitStatus(decode(body).ok()?),
            Operation::GitDiff => Self::GitDiff(decode(body).ok()?),
            Operation::GitLog => Self::GitLog(decode(body).ok()?),
            Operation::GitShow => Self::GitShow(decode(body).ok()?),
            Operation::GitAdd => Self::GitAdd(decode(body).ok()?),
            Operation::GitCommit => Self::GitCommit(decode(body).ok()?),
            Operation::GitRestore => Self::GitRestore(decode(body).ok()?),
        };
        Some(request)
    }

    /// The paths the request names, to confine to the workspace before the
    /// backend runs.
    pub fn paths(&self) -> Vec<&str> {
        match self {
            Self::FsRead(request) => vec![&request.path],
            Self::FsList(request) => vec![&request.path],
            Self::FsWrite(request) => vec![&request.path],
            Self::FsEdit(request) => vec![&request.path],
            Self::GitDiff(request) => request.paths.iter().map(String::as_str).collect(),
            Self::GitAdd(request) => request.paths.iter().map(String::as_str).collect(),
            Self::GitShow(request) => request.paths.iter().map(String::as_str).collect(),
            Self::GitRestore(request) => request.paths.iter().map(String::as_str).collect(),
            Self::ProcessExec(_) | Self::GitStatus(_) | Self::GitLog(_) | Self::GitCommit(_) => {
                Vec::new()
            }
        }
    }

    /// The program's argv, for `process.exec`.
    pub fn argv(&self) -> Option<&[String]> {
        match self {
            Self::ProcessExec(request) => Some(&request.argv),
            _ => None,
        }
    }
}

/// What a backend is given with a request: who asks (from the verified
/// token, never from the request), the request's paths confined to the
/// workspace, and the server's limits.
#[derive(Debug)]
pub struct Call<'a> {
    pub operation: Operation,
    pub claims: &'a TokenClaims,
    /// [`BackendRequest::paths`] inside the workspace, in the same order.
    pub confined: Vec<PathBuf>,
    pub limits: &'a Limits,
}

/// A backend's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    /// The response body, JSON.
    pub body: Vec<u8>,
    /// The process's exit code, for the audit.
    pub exit_code: Option<i32>,
}

/// A backend's refusal or failure. The message goes to the client and must
/// not hold file content, output or env values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: ErrorCode,
    pub message: String,
    pub exit_code: Option<i32>,
}

impl Failure {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            exit_code: None,
        }
    }
}

/// Does the operations of one backend.
pub trait Backend: Send + Sync {
    fn call(&self, call: &Call<'_>, request: BackendRequest) -> Result<Done, Failure>;
}

/// A backend not written yet: every operation is `backend_error`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unimplemented;

impl Backend for Unimplemented {
    fn call(&self, call: &Call<'_>, _request: BackendRequest) -> Result<Done, Failure> {
        Err(Failure::new(
            ErrorCode::BackendError,
            format!("{} is not implemented yet", call.operation),
        ))
    }
}

/// The server's backends.
#[derive(Clone)]
pub struct Backends {
    pub fs: Arc<dyn Backend>,
    pub process: Arc<dyn Backend>,
    pub git: Arc<dyn Backend>,
}

impl Backends {
    /// Every backend [`Unimplemented`].
    pub fn unimplemented() -> Self {
        Self {
            fs: Arc::new(Unimplemented),
            process: Arc::new(Unimplemented),
            git: Arc::new(Unimplemented),
        }
    }

    pub fn get(&self, kind: BackendKind) -> &dyn Backend {
        match kind {
            BackendKind::Fs => self.fs.as_ref(),
            BackendKind::Process => self.process.as_ref(),
            BackendKind::Git => self.git.as_ref(),
        }
    }
}

impl std::fmt::Debug for Backends {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Backends")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_but_health_has_a_backend() {
        let table: Vec<_> = Operation::ALL
            .iter()
            .map(|operation| BackendKind::of(*operation).map(BackendKind::name))
            .collect();
        assert_eq!(
            table,
            [
                None,
                Some("fs"),
                Some("fs"),
                Some("fs"),
                Some("fs"),
                Some("process"),
                Some("git"),
                Some("git"),
                Some("git"),
                Some("git"),
                Some("git"),
                Some("git"),
                Some("git"),
            ]
        );
        for operation in Operation::ALL.into_iter().skip(1) {
            let prefix = format!("{}.", BackendKind::of(operation).unwrap().name());
            assert!(operation.name().starts_with(&prefix), "{operation}");
        }
    }

    #[test]
    fn decodes_each_request_and_names_its_paths() {
        let cases: [(Operation, &str, Vec<&str>); 12] = [
            (Operation::FsRead, r#"{"path":"a"}"#, vec!["a"]),
            (Operation::FsList, r#"{"path":"d"}"#, vec!["d"]),
            (
                Operation::FsWrite,
                r#"{"path":"w","content":"x"}"#,
                vec!["w"],
            ),
            (
                Operation::FsEdit,
                r#"{"path":"e","old_string":"a","new_string":"b"}"#,
                vec!["e"],
            ),
            (Operation::ProcessExec, r#"{"argv":["ls","-l"]}"#, vec![]),
            (Operation::GitStatus, "{}", vec![]),
            (Operation::GitDiff, r#"{"paths":["p","q"]}"#, vec!["p", "q"]),
            (Operation::GitLog, "{}", vec![]),
            (Operation::GitShow, r#"{"paths":["s"]}"#, vec!["s"]),
            (Operation::GitRestore, r#"{"paths":["t"]}"#, vec!["t"]),
            (Operation::GitAdd, r#"{"paths":["r"]}"#, vec!["r"]),
            (Operation::GitCommit, r#"{"message":"m"}"#, vec![]),
        ];
        for (operation, body, paths) in cases {
            let request = BackendRequest::decode(operation, body.as_bytes())
                .unwrap_or_else(|| panic!("{operation}: {body}"));
            assert_eq!(request.paths(), paths, "{operation}");
            assert_eq!(
                request.argv().is_some(),
                operation == Operation::ProcessExec,
                "{operation}"
            );
        }
    }

    #[test]
    fn refuses_unknown_fields_and_other_shapes() {
        assert_eq!(BackendRequest::decode(Operation::Health, b"{}"), None);
        assert_eq!(
            BackendRequest::decode(Operation::FsRead, br#"{"path":"a","run_id":"other"}"#),
            None
        );
        assert_eq!(BackendRequest::decode(Operation::GitStatus, b""), None);
        assert_eq!(BackendRequest::decode(Operation::GitCommit, b"{}"), None);
    }

    #[test]
    fn unimplemented_is_a_backend_error() {
        let claims: TokenClaims = serde_json::from_str(
            r#"{"v":1,"jti":"j","actor_id":"a","role":"worker","task_id":1,"run_id":"r","workspace":"/w","branch":"dagq/r","committer":{"name":"n","email":"e"},"capabilities":[],"iat":0,"exp":1}"#,
        )
        .unwrap();
        let limits = Limits::default();
        let backends = Backends::unimplemented();
        for kind in [BackendKind::Fs, BackendKind::Process, BackendKind::Git] {
            let call = Call {
                operation: Operation::GitLog,
                claims: &claims,
                confined: Vec::new(),
                limits: &limits,
            };
            let request = BackendRequest::GitLog(git::LogRequest { limit: None });
            let failure = backends.get(kind).call(&call, request).unwrap_err();
            assert_eq!(failure.code, ErrorCode::BackendError);
            assert_eq!(failure.message, "git.log is not implemented yet");
            assert_eq!(failure.exit_code, None);
        }
        assert_eq!(format!("{backends:?}"), "Backends");
    }
}
