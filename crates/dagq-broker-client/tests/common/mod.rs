//! The tests' broker, started in process on `127.0.0.1:0` (no podman), a
//! run's worktree and token files, and the binary's runner.

#![allow(dead_code)]

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dagq_broker::backend::Backends;
use dagq_broker::backends::fs::FsBackend;
use dagq_broker::backends::git::GitBackend;
use dagq_broker::backends::process::ProcessBackend;
use dagq_broker::config::Config;
use dagq_broker::server::Server;
use dagq_broker_client::{BrokerClient, ClientError, Endpoint, TOKEN_FILE_ENV, URL_ENV};
use dagq_broker_protocol::{
    BrokerCapability, BrokerRole, Committer, ErrorBody, ErrorCode, SigningKey, TOKEN_VERSION,
    TokenClaims, sign,
};
use serde_json::Value;

/// The longest any one wait in these tests takes.
pub const LIMIT: Duration = Duration::from_secs(30);

pub const KEY: [u8; 32] = [9; 32];

pub struct Broker {
    pub dir: tempfile::TempDir,
    pub url: String,
}

impl Broker {
    /// A broker over `<dir>/runs`, where run-1's worktree is on the branch
    /// `dagq/run-1` of `<dir>/repo` and run-2 has a worktree of its own.
    pub fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("key"), KEY).unwrap();
        fs::create_dir_all(root.join("active")).unwrap();
        fs::create_dir_all(root.join("tokens")).unwrap();
        fs::create_dir_all(root.join("runs/run-2/worktree")).unwrap();
        fs::write(root.join("runs/run-2/worktree/theirs"), "theirs\n").unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        host_git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("README"), "hello\n").unwrap();
        host_git(&repo, &["add", "README"]);
        host_git(&repo, &["commit", "-q", "-m", "first"]);
        fs::create_dir_all(root.join("runs/run-1")).unwrap();
        let workspace = root.join("runs/run-1/worktree");
        host_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "dagq/run-1",
                workspace.to_str().unwrap(),
            ],
        );
        let path = |name: &str| root.join(name).to_string_lossy().into_owned();
        let args: Vec<String> = [
            "--listen",
            "127.0.0.1:0",
            "--key",
            &path("key"),
            "--active",
            &path("active"),
            "--audit",
            &path("audit"),
            "--root",
            &path("runs"),
            "--exec-allow",
            "sh",
            "--exec-allow",
            "git",
        ]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
        let config = Config::parse(&args).unwrap();
        let backends = Backends {
            fs: Arc::new(FsBackend::new(config.roots.clone())),
            process: Arc::new(ProcessBackend::new(config.roots.clone())),
            git: Arc::new(GitBackend::new(config.roots.clone())),
        };
        let server = Server::bind(&config, backends).unwrap();
        let addr = server.local_addr().unwrap();
        std::thread::spawn(move || server.serve());
        Self {
            dir,
            url: format!("http://{addr}"),
        }
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    pub fn workspace(&self) -> PathBuf {
        self.root().join("runs/run-1/worktree")
    }

    /// Run-1's claims with `capabilities`, expiring at `exp`.
    pub fn claims(&self, jti: &str, capabilities: &[BrokerCapability], exp: u64) -> TokenClaims {
        TokenClaims {
            v: TOKEN_VERSION,
            jti: jti.to_owned(),
            actor_id: "worker:run-1".to_owned(),
            role: BrokerRole::Worker,
            task_id: 834,
            run_id: "run-1".to_owned(),
            workspace: self.workspace().to_string_lossy().into_owned(),
            branch: "dagq/run-1".to_owned(),
            committer: Committer {
                name: "Worker".to_owned(),
                email: "worker@example.com".to_owned(),
            },
            capabilities: capabilities.iter().copied().collect(),
            iat: now() - 10,
            exp,
        }
    }

    /// Sign `claims`, mark them active unless `active` is false, and write
    /// the token file `tokens/<jti>`.
    pub fn token_file(&self, claims: &TokenClaims, active: bool) -> PathBuf {
        if active {
            fs::write(self.root().join("active").join(&claims.jti), &claims.run_id).unwrap();
        }
        let token = sign(&SigningKey::from_bytes(&KEY).unwrap(), claims).unwrap();
        let file = self.root().join("tokens").join(&claims.jti);
        fs::write(&file, format!("{}\n", token.expose())).unwrap();
        file
    }

    /// A token file of run-1 with every capability, valid for an hour.
    pub fn full_token_file(&self, jti: &str) -> PathBuf {
        self.token_file(
            &self.claims(jti, &BrokerCapability::ALL, now() + 3600),
            true,
        )
    }

    pub fn client(&self, token_file: &Path) -> BrokerClient {
        BrokerClient::new(
            Endpoint::parse(&self.url).unwrap(),
            Some(token_file.to_path_buf()),
        )
        .with_read_timeout(LIMIT)
    }

    pub fn audit_text(&self) -> String {
        fs::read_dir(self.root().join("audit"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect()
    }

    /// Run the binary with the broker's env and `token_file`.
    pub fn cli(&self, token_file: &Path, args: &[&str], stdin: &str) -> Output {
        cli(
            args,
            &[
                (URL_ENV, self.url.as_str()),
                (TOKEN_FILE_ENV, token_file.to_str().unwrap()),
            ],
            stdin,
        )
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// `git <args>` in `dir` as a person would, outside the broker.
pub fn host_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Host")
        .env("GIT_AUTHOR_EMAIL", "host@example.com")
        .env("GIT_COMMITTER_NAME", "Host")
        .env("GIT_COMMITTER_EMAIL", "host@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn json(&self) -> Value {
        assert_eq!(self.code, Some(0), "stderr: {}", self.stderr);
        assert!(self.stderr.is_empty(), "{}", self.stderr);
        serde_json::from_str(self.stdout.trim()).expect(&self.stdout)
    }

    /// The broker's refusal: exit 1, nothing on stdout, its error body on
    /// stderr.
    pub fn refused(&self, code: ErrorCode) {
        assert_eq!(
            self.code,
            Some(1),
            "stdout: {} stderr: {}",
            self.stdout,
            self.stderr
        );
        assert!(self.stdout.is_empty(), "{}", self.stdout);
        let body: ErrorBody = serde_json::from_str(self.stderr.trim()).expect(&self.stderr);
        assert_eq!(body.error.code, code, "{}", self.stderr);
        assert!(!body.error.request_id.as_str().is_empty());
    }
}

/// Clear the client's environment, preserving the supplied coverage destination.
/// Without it, an instrumented child writes default_*.profraw in the crate directory.
pub fn client_command(profile_file: Option<OsString>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq-broker-client"));
    command.env_clear();
    if let Some(profile_file) = profile_file {
        command.env("LLVM_PROFILE_FILE", profile_file);
    }
    command
}

/// Run `dagq-broker-client <args>` with `env` and the coverage destination, feeding `stdin`, and
/// wait at most [`LIMIT`].
pub fn cli(args: &[&str], env: &[(&str, &str)], stdin: &str) -> Output {
    let mut child = client_command(std::env::var_os("LLVM_PROFILE_FILE"))
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(child.wait_with_output());
    });
    let output = receive
        .recv_timeout(LIMIT)
        .unwrap_or_else(|_| panic!("dagq-broker-client {args:?} did not finish"))
        .unwrap();
    Output {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

pub fn refusal(result: Result<impl std::fmt::Debug, ClientError>) -> (u16, ErrorCode) {
    match result {
        Err(ClientError::Broker { status, error }) => {
            assert!(!error.message.is_empty());
            (status, error.code)
        }
        other => panic!("expected the broker's refusal, got {other:?}"),
    }
}
