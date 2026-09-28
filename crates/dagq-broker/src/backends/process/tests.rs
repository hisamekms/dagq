use std::os::unix::fs::symlink;

use dagq_broker_protocol::{Operation, TokenClaims, decode};
use serde_json::json;

use super::*;
use crate::config::Limits;

/// The longest any one wait in these tests takes.
const LIMIT: Duration = Duration::from_secs(20);

/// A mounted root with the workspace of run-1.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    backend: ProcessBackend,
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
            "task_id": 832, "run_id": "run-1", "workspace": workspace,
            "branch": "dagq/run-1", "committer": {"name": "n", "email": "e"},
            "capabilities": ["process.exec"], "iat": 0, "exp": 1
        }))
        .unwrap();
        let limits = Limits {
            exec_allow: ["sh", "echo", "pwd", "env", "cat", "git"]
                .map(str::to_owned)
                .to_vec(),
            exec_env: vec!["ALLOWED".to_owned(), "LANG".to_owned(), "PATH".to_owned()],
            ..Limits::default()
        };
        Self {
            backend: ProcessBackend::new(vec![root.clone()]),
            _dir: dir,
            root,
            claims,
            limits,
        }
    }

    fn workspace(&self) -> PathBuf {
        PathBuf::from(&self.claims.workspace)
    }

    fn call(&self, body: serde_json::Value) -> Result<Done, Failure> {
        let request =
            BackendRequest::decode(Operation::ProcessExec, body.to_string().as_bytes()).unwrap();
        let call = Call {
            operation: Operation::ProcessExec,
            claims: &self.claims,
            confined: Vec::new(),
            limits: &self.limits,
        };
        self.backend.call(&call, request)
    }

    fn exec(&self, body: serde_json::Value) -> process::ExecResponse {
        let done = self
            .call(body)
            .unwrap_or_else(|failure| panic!("{failure:?}"));
        let response: process::ExecResponse = decode(&done.body).unwrap();
        assert_eq!(done.exit_code, response.exit_code);
        response
    }

    fn refused(&self, body: serde_json::Value) -> Failure {
        self.call(body).unwrap_err()
    }
}

/// Whether `pid` is gone (reaped), waiting up to [`LIMIT`].
fn gone(pid: libc::pid_t) -> bool {
    let deadline = Instant::now() + LIMIT;
    while Instant::now() < deadline {
        // SAFETY: signal 0 only asks whether the process exists.
        if unsafe { libc::kill(pid, 0) } < 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn pid_in(path: &Path) -> libc::pid_t {
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "no pid in {}", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn runs_argv_without_a_shell_in_the_workspace() {
    let fixture = Fixture::new();
    let response = fixture.exec(json!({"argv": ["echo", "$HOME; rm -rf *", "`id`"]}));
    assert_eq!(response.stdout, "$HOME; rm -rf * `id`\n");
    assert_eq!(response.stderr, "");
    assert_eq!(response.exit_code, Some(0));

    let response = fixture.exec(json!({"argv": ["pwd"]}));
    assert_eq!(
        response.stdout.trim_end(),
        fixture.workspace().to_str().unwrap()
    );

    let response = fixture.exec(json!({"argv": ["sh", "-c", "echo out; echo err >&2; exit 3"]}));
    assert_eq!(response.exit_code, Some(3));
    assert_eq!(response.stdout, "out\n");
    assert_eq!(response.stderr, "err\n");

    let response = fixture.exec(json!({"argv": ["sh", "-c", "kill -9 $$"]}));
    assert_eq!(response.exit_code, None);
}

#[test]
fn gives_stdin_within_the_limit() {
    let mut fixture = Fixture::new();
    let response = fixture.exec(json!({"argv": ["cat"], "stdin": "line 1\nline 2\n"}));
    assert_eq!(response.stdout, "line 1\nline 2\n");

    // More than a pipe holds at once goes through in pieces.
    let big = "x".repeat(200 * 1024);
    let response = fixture.exec(json!({"argv": ["cat"], "stdin": big}));
    assert_eq!(response.stdout.len(), big.len());

    // A program that does not read its stdin is not held up by it.
    let response = fixture.exec(json!({"argv": ["echo", "ok"], "stdin": big}));
    assert_eq!(response.stdout, "ok\n");

    fixture.limits.output_limit_bytes = 8;
    let failure = fixture.refused(json!({"argv": ["cat"], "stdin": "123456789"}));
    assert_eq!(failure.code, ErrorCode::OutputLimit);
    assert!(
        !failure.message.contains("123456789"),
        "{}",
        failure.message
    );
}

#[test]
fn refuses_what_the_allowlist_does_not_hold() {
    let fixture = Fixture::new();
    let cases = [
        (json!({"argv": []}), ErrorCode::InvalidRequest),
        (json!({"argv": [""]}), ErrorCode::InvalidRequest),
        (
            json!({"argv": ["echo", "a\u{0}b"]}),
            ErrorCode::InvalidRequest,
        ),
        (json!({"argv": ["ls"]}), ErrorCode::CapabilityDenied),
        (
            json!({"argv": ["/bin/sh", "-c", "true"]}),
            ErrorCode::CapabilityDenied,
        ),
        (json!({"argv": ["./sh"]}), ErrorCode::CapabilityDenied),
        (
            json!({"argv": ["git", "push"]}),
            ErrorCode::CapabilityDenied,
        ),
        (
            json!({"argv": ["/usr/bin/git", "status"]}),
            ErrorCode::CapabilityDenied,
        ),
        (
            json!({"argv": ["echo"], "timeout_secs": 0}),
            ErrorCode::InvalidRequest,
        ),
        (
            json!({"argv": ["echo"], "env": {"ALLOWED": "a\u{0}b"}}),
            ErrorCode::InvalidRequest,
        ),
    ];
    for (body, code) in cases {
        let failure = fixture.refused(body.clone());
        assert_eq!(failure.code, code, "{body}: {}", failure.message);
        assert_eq!(failure.exit_code, None);
    }
    let failure = fixture.refused(json!({"argv": ["git", "status"]}));
    assert_eq!(failure.message, "git runs through the git operations only");
    let failure = fixture.refused(json!({"argv": ["ls"]}));
    assert_eq!(failure.message, "ls is not in the exec allowlist");

    let mut fixture = Fixture::new();
    fixture.limits.exec_allow = vec!["no-such-program-dagq".to_owned()];
    let failure = fixture.refused(json!({"argv": ["no-such-program-dagq"]}));
    assert_eq!(failure.code, ErrorCode::BackendError);
    assert!(failure.message.contains("not found"), "{}", failure.message);
}

#[test]
fn the_env_is_the_fixed_one_and_the_allowed_names() {
    let fixture = Fixture::new();
    let response = fixture.exec(json!({
        "argv": ["env"],
        "env": {
            "ALLOWED": "yes",
            "LANG": "en_US.UTF-8",
            "PATH": "/tmp/evil",
            "HOME": "/tmp/evil",
            "OTHER": "dropped-value",
        },
    }));
    let env: BTreeMap<&str, &str> = response
        .stdout
        .lines()
        .map(|line| line.split_once('=').unwrap())
        .collect();
    assert_eq!(
        env.keys().copied().collect::<Vec<_>>(),
        ["ALLOWED", "HOME", "LANG", "PATH", "TERM"]
    );
    assert_eq!(env["ALLOWED"], "yes");
    assert_eq!(env["LANG"], "en_US.UTF-8");
    assert_eq!(env["PATH"], PATH);
    assert_eq!(env["TERM"], "dumb");
    let home = PathBuf::from(env["HOME"]);
    assert_ne!(home, PathBuf::from("/tmp/evil"));
    assert_ne!(Some(home.as_os_str()), std::env::var_os("HOME").as_deref());
    assert!(!response.stdout.contains("dropped-value"));
    // Each exec's HOME is its own and gone afterwards.
    assert!(!home.exists(), "{}", home.display());
    let again = fixture.exec(json!({"argv": ["sh", "-c", "touch \"$HOME/x\"; echo \"$HOME\""]}));
    assert_ne!(again.stdout.trim_end(), env["HOME"]);
    assert!(!Path::new(again.stdout.trim_end()).exists());
}

#[test]
fn a_timeout_stops_the_whole_process_group() {
    let mut fixture = Fixture::new();
    let started = Instant::now();
    let failure = fixture.refused(json!({
        "argv": ["sh", "-c", "sleep 60 & echo $! > bg.pid; sleep 60"],
        "timeout_secs": 3,
    }));
    assert_eq!(failure.code, ErrorCode::Timeout, "{}", failure.message);
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert!(gone(pid_in(&fixture.workspace().join("bg.pid"))));

    // The server's maximum caps what the request asks for.
    fixture.limits.exec_max_timeout_secs = 1;
    let started = Instant::now();
    let failure = fixture.refused(json!({"argv": ["sh", "-c", "sleep 60"], "timeout_secs": 3600}));
    assert_eq!(failure.code, ErrorCode::Timeout);
    assert!(
        failure.message.contains("past 1 seconds"),
        "{}",
        failure.message
    );
    assert!(started.elapsed() < LIMIT);
}

#[test]
fn what_a_finished_program_leaves_in_its_group_is_stopped() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let response =
        fixture.exec(json!({"argv": ["sh", "-c", "sleep 60 & echo $! > bg.pid; echo done"]}));
    assert_eq!(response.stdout, "done\n");
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert!(gone(pid_in(&fixture.workspace().join("bg.pid"))));
}

#[test]
fn output_past_the_limit_stops_the_program() {
    let mut fixture = Fixture::new();
    fixture.limits.output_limit_bytes = 1000;
    let started = Instant::now();
    let failure = fixture.refused(json!({
        "argv": ["sh", "-c", "echo $$ > sh.pid; while :; do echo secret-output; done"],
    }));
    assert_eq!(failure.code, ErrorCode::OutputLimit, "{}", failure.message);
    assert!(!failure.message.contains("secret-output"));
    assert!(started.elapsed() < LIMIT);
    assert!(gone(pid_in(&fixture.workspace().join("sh.pid"))));

    // stderr counts with stdout.
    let failure = fixture.refused(json!({
        "argv": ["sh", "-c", "while :; do echo e >&2; done"],
    }));
    assert_eq!(failure.code, ErrorCode::OutputLimit);

    // Up to the limit is answered whole.
    let response = fixture.exec(json!({"argv": ["sh", "-c", "printf '%01000d' 0"]}));
    assert_eq!(response.stdout.len(), 1000);
}

#[test]
fn a_workspace_that_is_a_symlink_or_outside_the_roots_is_refused() {
    let mut fixture = Fixture::new();
    let elsewhere = fixture.root.join("run-2");
    std::fs::create_dir_all(&elsewhere).unwrap();
    symlink(&elsewhere, fixture.root.join("run-3")).unwrap();
    fixture.claims.workspace = fixture.root.join("run-3").to_string_lossy().into_owned();
    let failure = fixture.refused(json!({"argv": ["pwd"]}));
    assert_eq!(
        failure.code,
        ErrorCode::WorkspaceViolation,
        "{}",
        failure.message
    );

    fixture.claims.workspace = "/tmp".to_owned();
    let failure = fixture.refused(json!({"argv": ["pwd"]}));
    assert_eq!(failure.code, ErrorCode::WorkspaceViolation);

    fixture.claims.workspace = fixture.root.join("missing").to_string_lossy().into_owned();
    let failure = fixture.refused(json!({"argv": ["pwd"]}));
    assert_eq!(failure.code, ErrorCode::BackendError, "{}", failure.message);
}

#[test]
fn only_process_exec_is_its_operation() {
    let fixture = Fixture::new();
    let call = Call {
        operation: Operation::GitLog,
        claims: &fixture.claims,
        confined: Vec::new(),
        limits: &fixture.limits,
    };
    let request = BackendRequest::GitLog(dagq_broker_protocol::git::LogRequest { limit: None });
    let failure = fixture.backend.call(&call, request).unwrap_err();
    assert_eq!(failure.code, ErrorCode::InvalidRequest);
}
