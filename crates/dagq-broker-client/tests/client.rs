//! The client against a broker started in the test (in process, on
//! `127.0.0.1:0`, no podman): the library's typed calls and the
//! `dagq-broker-client` binary do fs, process and git in a run's worktree,
//! and the broker's refusals come back as its structured error (and exit 1).
//! Also: the crate depends on no dagq.

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
    TokenClaims, fs as fs_ops, git, process, sign,
};
use serde_json::Value;

/// The longest any one wait in these tests takes.
const LIMIT: Duration = Duration::from_secs(30);

const KEY: [u8; 32] = [9; 32];

struct Broker {
    dir: tempfile::TempDir,
    url: String,
}

impl Broker {
    /// A broker over `<dir>/runs`, where run-1's worktree is on the branch
    /// `dagq/run-1` of `<dir>/repo` and run-2 has a worktree of its own.
    fn start() -> Self {
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

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn workspace(&self) -> PathBuf {
        self.root().join("runs/run-1/worktree")
    }

    /// Run-1's claims with `capabilities`, expiring at `exp`.
    fn claims(&self, jti: &str, capabilities: &[BrokerCapability], exp: u64) -> TokenClaims {
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
    fn token_file(&self, claims: &TokenClaims, active: bool) -> PathBuf {
        if active {
            fs::write(self.root().join("active").join(&claims.jti), &claims.run_id).unwrap();
        }
        let token = sign(&SigningKey::from_bytes(&KEY).unwrap(), claims).unwrap();
        let file = self.root().join("tokens").join(&claims.jti);
        fs::write(&file, format!("{}\n", token.expose())).unwrap();
        file
    }

    /// A token file of run-1 with every capability, valid for an hour.
    fn full_token_file(&self, jti: &str) -> PathBuf {
        self.token_file(
            &self.claims(jti, &BrokerCapability::ALL, now() + 3600),
            true,
        )
    }

    fn client(&self, token_file: &Path) -> BrokerClient {
        BrokerClient::new(
            Endpoint::parse(&self.url).unwrap(),
            Some(token_file.to_path_buf()),
        )
        .with_read_timeout(LIMIT)
    }

    fn audit_text(&self) -> String {
        fs::read_dir(self.root().join("audit"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect()
    }

    /// Run the binary with the broker's env and `token_file`.
    fn cli(&self, token_file: &Path, args: &[&str], stdin: &str) -> Output {
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

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// `git <args>` in `dir` as a person would, outside the broker.
fn host_git(dir: &Path, args: &[&str]) -> String {
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

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Output {
    fn json(&self) -> Value {
        assert_eq!(self.code, Some(0), "stderr: {}", self.stderr);
        assert!(self.stderr.is_empty(), "{}", self.stderr);
        serde_json::from_str(self.stdout.trim()).expect(&self.stdout)
    }

    /// The broker's refusal: exit 1, nothing on stdout, its error body on
    /// stderr.
    fn refused(&self, code: ErrorCode) {
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

/// Run `dagq-broker-client <args>` with only `env`, feeding `stdin`, and
/// wait at most [`LIMIT`].
fn cli(args: &[&str], env: &[(&str, &str)], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dagq-broker-client"))
        .args(args)
        .env_clear()
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

fn refusal(result: Result<impl std::fmt::Debug, ClientError>) -> (u16, ErrorCode) {
    match result {
        Err(ClientError::Broker { status, error }) => {
            assert!(!error.message.is_empty());
            (status, error.code)
        }
        other => panic!("expected the broker's refusal, got {other:?}"),
    }
}

#[test]
fn the_library_does_fs_process_and_git_in_the_run_worktree() {
    let broker = Broker::start();
    let token_file = broker.full_token_file("jti-lib");
    let client = broker.client(&token_file);

    let health = client.health().unwrap();
    assert_eq!(
        (
            health.status.as_str(),
            health.build.as_str(),
            health.protocol
        ),
        ("ok", dagq_broker::BUILD, 1)
    );

    let written = client
        .fs_write(&fs_ops::WriteRequest {
            path: "dir/a.txt".into(),
            content: "one\ntwo\n".into(),
            create_dirs: true,
        })
        .unwrap();
    assert_eq!(written.bytes, 8);
    let edited = client
        .fs_edit(&fs_ops::EditRequest {
            path: "dir/a.txt".into(),
            old_string: "two".into(),
            new_string: "three".into(),
            replace_all: false,
        })
        .unwrap();
    assert_eq!(edited.replacements, 1);
    let read = client
        .fs_read(&fs_ops::ReadRequest {
            path: "dir/a.txt".into(),
            offset: Some(1),
            limit: None,
        })
        .unwrap();
    assert_eq!((read.content.as_str(), read.truncated), ("three\n", false));
    let listed = client
        .fs_list(&fs_ops::ListRequest { path: "dir".into() })
        .unwrap();
    let names: Vec<_> = listed.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["a.txt"]);
    assert_eq!(
        fs::read_to_string(broker.workspace().join("dir/a.txt")).unwrap(),
        "one\nthree\n"
    );

    let executed = client
        .exec(&process::ExecRequest {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "cat dir/a.txt; cat; exit 3".into(),
            ],
            env: Default::default(),
            stdin: Some("from-stdin\n".into()),
            timeout_secs: Some(10),
        })
        .unwrap();
    assert_eq!(executed.exit_code, Some(3));
    assert_eq!(executed.stdout, "one\nthree\nfrom-stdin\n");

    let status = client.git_status().unwrap();
    assert_eq!(status.branch.as_deref(), Some("dagq/run-1"));
    assert!(
        status.entries.iter().any(|e| e.path.starts_with("dir")),
        "{status:?}"
    );
    client
        .git_add(&git::AddRequest {
            paths: vec!["dir/a.txt".into()],
        })
        .unwrap();
    let diff = client
        .git_diff(&git::DiffRequest {
            staged: true,
            paths: Vec::new(),
        })
        .unwrap();
    assert!(diff.diff.contains("+three"), "{}", diff.diff);
    let commit = client
        .git_commit(&git::CommitRequest {
            message: "add a.txt".into(),
        })
        .unwrap()
        .commit;
    assert_eq!(
        host_git(&broker.root().join("repo"), &["rev-parse", "dagq/run-1"]),
        commit
    );
    let log = client.git_log(&git::LogRequest { limit: Some(1) }).unwrap();
    assert_eq!(log.commits[0].commit, commit);
    assert_eq!(log.commits[0].author, "Worker");
    let shown = client
        .git_show(&git::ShowRequest {
            commit: None,
            paths: Vec::new(),
        })
        .unwrap();
    assert_eq!(shown.commit, commit);
    assert!(shown.show.contains("add a.txt"), "{}", shown.show);
    fs::write(broker.workspace().join("dir/a.txt"), "scratch\n").unwrap();
    client
        .git_restore(&git::RestoreRequest {
            staged: false,
            paths: vec!["dir/a.txt".into()],
        })
        .unwrap();
    assert_eq!(
        fs::read_to_string(broker.workspace().join("dir/a.txt")).unwrap(),
        "one\nthree\n"
    );

    // Every request is audited under the run, and the token is not.
    let audit = broker.audit_text();
    let token = fs::read_to_string(&token_file).unwrap();
    assert!(!audit.contains(token.trim()));
    assert_eq!(audit.matches("\"run_id\":\"run-1\"").count(), 12, "{audit}");
}

#[test]
fn the_library_returns_the_brokers_refusals_as_structured_errors() {
    let broker = Broker::start();
    let client = broker.client(&broker.full_token_file("jti-refused"));
    let read = |path: &str| {
        client.fs_read(&fs_ops::ReadRequest {
            path: path.into(),
            offset: None,
            limit: None,
        })
    };
    // Outside the workspace: `..`, an absolute path, another run, a symlink.
    assert_eq!(
        refusal(read("../../run-2/worktree/theirs")),
        (403, ErrorCode::WorkspaceViolation)
    );
    assert_eq!(
        refusal(read("/etc/hosts")),
        (403, ErrorCode::WorkspaceViolation)
    );
    let theirs = broker.root().join("runs/run-2/worktree/theirs");
    assert_eq!(
        refusal(read(theirs.to_str().unwrap())),
        (403, ErrorCode::WorkspaceViolation)
    );
    std::os::unix::fs::symlink(&theirs, broker.workspace().join("escape")).unwrap();
    assert_eq!(
        refusal(read("escape")),
        (403, ErrorCode::WorkspaceViolation)
    );
    assert_eq!(refusal(read("missing")), (502, ErrorCode::BackendError));
    // git through exec, and a program off the allowlist.
    let exec = |argv: &[&str]| {
        client.exec(&process::ExecRequest {
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            env: Default::default(),
            stdin: None,
            timeout_secs: None,
        })
    };
    assert_eq!(
        refusal(exec(&["git", "push"])),
        (403, ErrorCode::CapabilityDenied)
    );
    assert_eq!(
        refusal(exec(&["cat", "x"])),
        (403, ErrorCode::CapabilityDenied)
    );
    // An edit that matches nothing.
    assert_eq!(
        refusal(client.fs_edit(&fs_ops::EditRequest {
            path: "README".into(),
            old_string: "absent".into(),
            new_string: "x".into(),
            replace_all: false,
        })),
        (400, ErrorCode::InvalidRequest)
    );

    // A body larger than the broker reads is not sent.
    let error = client
        .fs_write(&fs_ops::WriteRequest {
            path: "big".into(),
            content: "x".repeat(dagq_broker_protocol::MAX_REQUEST_BYTES),
            create_dirs: false,
        })
        .unwrap_err();
    assert!(matches!(error, ClientError::Config(_)), "{error:?}");
    assert!(error.to_string().contains("more than the broker reads"));

    // A token without the capability, an expired one, a revoked one and a
    // tampered one.
    let read_only = broker.token_file(
        &broker.claims("jti-read-only", &[BrokerCapability::FsRead], now() + 3600),
        true,
    );
    let write = |client: &BrokerClient| {
        client.fs_write(&fs_ops::WriteRequest {
            path: "x".into(),
            content: "x".into(),
            create_dirs: false,
        })
    };
    assert_eq!(
        refusal(write(&broker.client(&read_only))),
        (403, ErrorCode::CapabilityDenied)
    );
    assert!(!broker.workspace().join("x").exists());
    let expired = broker.token_file(
        &broker.claims("jti-expired", &BrokerCapability::ALL, now() - 1),
        true,
    );
    assert_eq!(
        refusal(broker.client(&expired).git_status()),
        (401, ErrorCode::Unauthorized)
    );
    let revoked = broker.token_file(
        &broker.claims("jti-revoked", &BrokerCapability::ALL, now() + 3600),
        false,
    );
    assert_eq!(
        refusal(broker.client(&revoked).git_status()),
        (401, ErrorCode::Unauthorized)
    );
    let tampered = broker.root().join("tokens/tampered");
    let token = fs::read_to_string(broker.full_token_file("jti-tampered")).unwrap();
    fs::write(&tampered, format!("{}x", token.trim())).unwrap();
    assert_eq!(
        refusal(broker.client(&tampered).git_status()),
        (401, ErrorCode::Unauthorized)
    );
}

#[test]
fn the_cli_does_fs_process_and_git_and_exits_non_zero_on_a_refusal() {
    let broker = Broker::start();
    let token_file = broker.full_token_file("jti-cli");
    let token = fs::read_to_string(&token_file).unwrap().trim().to_owned();
    let run = |args: &[&str], stdin: &str| {
        let output = broker.cli(&token_file, args, stdin);
        assert!(!output.stdout.contains(&token) && !output.stderr.contains(&token));
        output
    };

    let health = run(&["health"], "");
    assert_eq!(health.code, Some(0), "{}", health.stderr);
    assert_eq!(
        health.stdout,
        format!("ok build {} protocol 1\n", dagq_broker::BUILD)
    );
    assert_eq!(run(&["health", "--json"], "").json()["status"], "ok");

    let claims = run(&["token", "inspect"], "").json();
    assert_eq!(claims["run_id"], "run-1");
    assert_eq!(claims["jti"], "jti-cli");

    let written = run(
        &["fs", "write", "notes/b.txt", "--create-dirs"],
        "alpha\nbeta\n",
    )
    .json();
    assert_eq!(written["bytes"], 11);
    let edited = run(
        &[
            "fs",
            "edit",
            "notes/b.txt",
            "--old",
            "beta",
            "--new",
            "gamma",
        ],
        "",
    )
    .json();
    assert_eq!(edited["replacements"], 1);
    let read = run(&["fs", "read", "notes/b.txt"], "").json();
    assert_eq!(read["content"], "alpha\ngamma\n");
    let listed = run(&["fs", "list", "notes"], "").json();
    assert_eq!(listed["entries"][0]["name"], "b.txt");

    let executed = run(
        &[
            "exec",
            "--stdin",
            "in\n",
            "--",
            "sh",
            "-c",
            "cat notes/b.txt; cat",
        ],
        "",
    )
    .json();
    assert_eq!(executed["exit_code"], 0);
    assert_eq!(executed["stdout"], "alpha\ngamma\nin\n");

    assert_eq!(run(&["git", "status"], "").json()["branch"], "dagq/run-1");
    assert_eq!(
        run(&["git", "add", "notes/b.txt"], "").json(),
        serde_json::json!({})
    );
    let diff = run(&["git", "diff", "--staged"], "").json();
    assert!(diff["diff"].as_str().unwrap().contains("+gamma"));
    let commit = run(&["git", "commit", "-m", "add b.txt"], "").json();
    let commit = commit["commit"].as_str().unwrap().to_owned();
    assert_eq!(
        run(&["git", "log", "--limit", "1"], "").json()["commits"][0]["commit"],
        commit.as_str()
    );
    assert_eq!(
        run(&["git", "show", "--commit", "HEAD"], "").json()["commit"],
        commit.as_str()
    );
    fs::write(broker.workspace().join("notes/b.txt"), "scratch\n").unwrap();
    assert_eq!(
        run(&["git", "restore", "notes/b.txt"], "").json(),
        serde_json::json!({})
    );
    assert_eq!(
        fs::read_to_string(broker.workspace().join("notes/b.txt")).unwrap(),
        "alpha\ngamma\n"
    );

    // The broker's refusals: exit 1 and its error body on stderr.
    run(&["fs", "read", "../../run-2/worktree/theirs"], "").refused(ErrorCode::WorkspaceViolation);
    run(&["exec", "--", "git", "push"], "").refused(ErrorCode::CapabilityDenied);
    run(&["git", "diff", "../x"], "").refused(ErrorCode::WorkspaceViolation);
    let revoked = broker.token_file(
        &broker.claims("jti-cli-revoked", &BrokerCapability::ALL, now() + 3600),
        false,
    );
    broker
        .cli(&revoked, &["git", "status"], "")
        .refused(ErrorCode::Unauthorized);
    // --url and --token-file win over the env.
    let output = cli(
        &[
            "--url",
            &broker.url,
            "--token-file",
            token_file.to_str().unwrap(),
            "fs",
            "list",
            ".",
        ],
        &[
            (URL_ENV, "http://127.0.0.1:1"),
            (TOKEN_FILE_ENV, "/missing"),
        ],
        "",
    );
    assert!(output.json()["entries"].is_array());
}

#[test]
fn the_cli_exits_2_on_a_bad_command_line_and_3_when_it_cannot_ask() {
    let output = cli(&["--version"], &[], "");
    assert_eq!(output.code, Some(0));
    assert_eq!(
        output.stdout,
        format!("dagq-broker-client {}\n", env!("CARGO_PKG_VERSION"))
    );
    let output = cli(&["git", "push"], &[], "");
    assert_eq!(output.code, Some(2));
    assert!(
        output.stderr.contains("unknown command `git push`"),
        "{}",
        output.stderr
    );
    // Something on the port that closes each connection without an answer
    // (kept bound, so no other test can take the port meanwhile). The client
    // sees an empty answer (protocol) or, when the close resets the
    // connection, a transport error; either is exit 3.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    let output = cli(&["health"], &[(URL_ENV, &url)], "");
    assert_eq!(output.code, Some(3));
    assert!(
        output.stderr.contains("protocol") || output.stderr.contains("transport"),
        "{}",
        output.stderr
    );
    let output = cli(&["git", "status"], &[(URL_ENV, &url)], "");
    assert_eq!(output.code, Some(3));
    assert!(output.stderr.contains(TOKEN_FILE_ENV), "{}", output.stderr);
}

/// The packages `cargo tree` lists for this crate over `edges`.
fn dependencies(edges: &str) -> Vec<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let child = Command::new(cargo)
        .args([
            "tree",
            "--locked",
            "--offline",
            "-p",
            "dagq-broker-client",
            "-e",
            edges,
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (send, receive) = mpsc::channel();
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = send.send(child.wait_with_output());
    });
    let output = receive
        .recv_timeout(Duration::from_secs(120))
        .unwrap_or_else(|_| panic!("cargo tree (pid {pid}) did not finish"))
        .unwrap();
    assert!(
        output.status.success(),
        "cargo tree: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_client_crate_depends_on_no_dagq() {
    // What the binary is built from: the protocol, never dagq nor the server.
    let normal = dependencies("normal,build");
    assert!(
        normal.iter().any(|name| name == "dagq-broker-protocol"),
        "{normal:?}"
    );
    assert!(!normal.iter().any(|name| name == "dagq"), "{normal:?}");
    assert!(
        !normal.iter().any(|name| name == "dagq-broker"),
        "{normal:?}"
    );
    // Its tests start the server in process, and still pull in no dagq.
    let all = dependencies("all");
    assert!(all.iter().any(|name| name == "dagq-broker"), "{all:?}");
    assert!(!all.iter().any(|name| name == "dagq"), "{all:?}");
}
