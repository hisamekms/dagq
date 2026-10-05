//! The package backend ([Broker] "package.install"): `package.install`
//! runs one of the commands the repository configured (`--package`, from
//! `[broker.package]` of `dagq.toml`), chosen by its name. The request
//! carries the name and a timeout only, never an argv, env or stdin, so
//! nothing but the configured commands runs through it.
//!
//! It runs as `process.exec` does ([`super::process`]): without a shell, in
//! the token's workspace, the program looked up on the fixed `PATH`, with
//! the env starting empty (`PATH`, a `HOME` of its own, `LANG`, `TERM`;
//! nothing of the request's or the broker's), the timeout capped by the
//! server's maximum and the output limit, the process group killed past
//! either. A name that is not configured is `capability_denied`. The
//! network is not narrowed here (Phase 4).
//!
//! [Broker]: https://github.com/hisamekms/dagq/blob/main/docs/design/broker.md

use std::collections::BTreeMap;
use std::path::PathBuf;

use dagq_broker_protocol::ErrorCode;

use crate::backend::{Backend, BackendRequest, Call, Done, Failure};
use crate::backends::process::{allowed_program, request_env, run_in_workspace, timeout};

/// The package backend over the server's mounted roots.
#[derive(Debug, Clone)]
pub struct PackageBackend {
    roots: Vec<PathBuf>,
}

impl PackageBackend {
    /// A backend that runs the configured commands in workspaces under
    /// `roots` (`--root`).
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }
}

impl Backend for PackageBackend {
    fn call(&self, call: &Call<'_>, request: BackendRequest) -> Result<Done, Failure> {
        let BackendRequest::PackageInstall(request) = request else {
            return Err(Failure::new(
                ErrorCode::InvalidRequest,
                format!("{} is not a package operation", call.operation),
            ));
        };
        let limits = call.limits;
        let Some(argv) = limits.packages.get(&request.name) else {
            return Err(Failure::new(
                ErrorCode::CapabilityDenied,
                format!(
                    "{} is not a configured package command",
                    printable(&request.name)
                ),
            ));
        };
        // The configuration was checked when read; an empty argv (limits
        // built around `Config::parse`) is refused before anything reads
        // `argv[0]`, and the same check as `process.exec`'s stays here (no
        // path, never git).
        let Some(program) = argv.first() else {
            return Err(Failure::new(
                ErrorCode::BackendError,
                format!(
                    "{} is configured without a program",
                    printable(&request.name)
                ),
            ));
        };
        allowed_program(argv, std::slice::from_ref(program))?;
        let timeout = timeout(request.timeout_secs, limits)?;
        let env = request_env(&BTreeMap::new(), &[])?;
        run_in_workspace(
            &self.roots,
            call,
            argv,
            &env,
            Vec::new(),
            timeout,
            limits.output_limit_bytes,
        )
    }
}

/// `name` for a message: as it is when it could be configured, else a
/// placeholder (the request's text is not echoed back).
fn printable(name: &str) -> String {
    if dagq_broker_protocol::package::valid_name(name) {
        format!("`{name}`")
    } else {
        "the name".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dagq_broker_protocol::{Operation, TokenClaims, git, package, process};
    use serde_json::json;

    use super::*;
    use crate::config::Limits;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        workspace: PathBuf,
        backend: PackageBackend,
        claims: TokenClaims,
        limits: Limits,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap().join("runs");
            let workspace = root.join("run-1/worktree");
            std::fs::create_dir_all(&workspace).unwrap();
            let claims: TokenClaims = serde_json::from_value(json!({
                "v": 1, "jti": "j", "actor_id": "worker:run-1", "role": "worker",
                "task_id": 840, "run_id": "run-1", "workspace": workspace,
                "branch": "dagq/run-1", "committer": {"name": "n", "email": "e"},
                "capabilities": ["package.install"], "iat": 0, "exp": 1
            }))
            .unwrap();
            let mut limits = Limits {
                exec_timeout_secs: 1,
                exec_max_timeout_secs: 1,
                output_limit_bytes: 64,
                ..Limits::default()
            };
            for (name, words) in [
                (
                    "fetch",
                    &["sh", "-c", "pwd; env; echo fetched > fetched"][..],
                ),
                ("slow", &["sh", "-c", "sleep 30"][..]),
                ("loud", &["sh", "-c", "while :; do echo out; done"][..]),
                ("git", &["git", "fetch"][..]),
                ("path", &["/bin/sh", "-c", "true"][..]),
                ("empty", &[][..]),
            ] {
                limits.packages.insert(name.to_owned(), argv(words));
            }
            Self {
                _dir: dir,
                workspace,
                backend: PackageBackend::new(vec![root]),
                claims,
                limits,
            }
        }

        fn install(&self, name: &str, timeout_secs: Option<u64>) -> Result<Done, Failure> {
            let call = Call {
                operation: Operation::PackageInstall,
                claims: &self.claims,
                confined: Vec::new(),
                limits: &self.limits,
            };
            let request = BackendRequest::PackageInstall(package::InstallRequest {
                name: name.to_owned(),
                timeout_secs,
            });
            self.backend.call(&call, request)
        }
    }

    #[test]
    fn a_configured_command_runs_in_the_workspace_with_a_clean_env() {
        let mut fixture = Fixture::new();
        fixture.limits.output_limit_bytes = 64 * 1024;
        let done = fixture.install("fetch", None).unwrap();
        assert_eq!(done.exit_code, Some(0));
        let answer: process::ExecResponse = serde_json::from_slice(&done.body).unwrap();
        let stdout = answer.stdout;
        assert!(
            stdout.starts_with(&format!("{}\n", fixture.workspace.display())),
            "{stdout}"
        );
        assert!(
            stdout.contains("PATH=/usr/local/bin:/usr/bin:/bin"),
            "{stdout}"
        );
        assert!(stdout.contains("LANG=C.UTF-8"), "{stdout}");
        assert!(!stdout.contains("CARGO"), "{stdout}");
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("fetched")).unwrap(),
            "fetched\n"
        );
    }

    #[test]
    fn a_name_that_is_not_configured_runs_nothing() {
        let fixture = Fixture::new();
        for name in ["other", "sh", "a b"] {
            let failure = fixture.install(name, None).unwrap_err();
            assert_eq!(failure.code, ErrorCode::CapabilityDenied, "{name}");
            assert!(failure.message.contains("not a configured"), "{name}");
        }
        assert!(
            fixture
                .install("a b", None)
                .unwrap_err()
                .message
                .starts_with("the name")
        );
        // A configuration that went around the check still never runs git
        // or a path.
        for name in ["git", "path"] {
            let failure = fixture.install(name, None).unwrap_err();
            assert_eq!(failure.code, ErrorCode::CapabilityDenied, "{name}");
        }
    }

    #[test]
    fn an_empty_argv_is_refused_before_it_is_read() {
        let fixture = Fixture::new();
        let failure = fixture.install("empty", None).unwrap_err();
        assert_eq!(failure.code, ErrorCode::BackendError);
        assert_eq!(failure.message, "`empty` is configured without a program");
    }

    #[test]
    fn the_timeout_and_the_output_limit_stop_the_command() {
        let fixture = Fixture::new();
        let started = std::time::Instant::now();
        let failure = fixture.install("slow", Some(3600)).unwrap_err();
        assert_eq!(failure.code, ErrorCode::Timeout);
        assert!(started.elapsed() < Duration::from_secs(20));
        let failure = fixture.install("loud", None).unwrap_err();
        assert_eq!(failure.code, ErrorCode::OutputLimit);
        let failure = fixture.install("slow", Some(0)).unwrap_err();
        assert_eq!(failure.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn another_operation_is_refused() {
        let fixture = Fixture::new();
        let call = Call {
            operation: Operation::GitLog,
            claims: &fixture.claims,
            confined: Vec::new(),
            limits: &fixture.limits,
        };
        let failure = fixture
            .backend
            .call(
                &call,
                BackendRequest::GitLog(git::LogRequest { limit: None }),
            )
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::InvalidRequest);
    }
}
