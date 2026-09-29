use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;

use dagq_broker_protocol::{Operation, TokenClaims, decode};

use super::*;
use crate::config::Limits;

/// A throwaway repository with a first commit on `main` and the worktrees
/// of run-1 and run-2 (`runs/<run>/worktree` on `dagq/<run>`), as the
/// runtime makes them.
struct Fixture {
    _dir: tempfile::TempDir,
    base: PathBuf,
    backend: GitBackend,
    claims: TokenClaims,
    limits: Limits,
}

/// `git <args>` in `dir` as a person would, outside the backend.
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

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        host_git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README"), "hello\n").unwrap();
        host_git(&repo, &["add", "README"]);
        host_git(&repo, &["commit", "-q", "-m", "first"]);
        for run in ["run-1", "run-2"] {
            let worktree = base.join("runs").join(run).join("worktree");
            let branch = format!("dagq/{run}");
            host_git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &branch,
                    worktree.to_str().unwrap(),
                ],
            );
        }
        let workspace = base.join("runs/run-1/worktree");
        let claims: TokenClaims = serde_json::from_value(serde_json::json!({
            "v": 1, "jti": "j", "actor_id": "worker:run-1", "role": "worker",
            "task_id": 833, "run_id": "run-1", "workspace": workspace,
            "branch": "dagq/run-1",
            "committer": {"name": "Worker", "email": "worker@example.com"},
            "capabilities": ["git.read", "git.write"], "iat": 0, "exp": 1
        }))
        .unwrap();
        Self {
            backend: GitBackend::new(vec![base.join("runs")]),
            _dir: dir,
            base,
            claims,
            limits: Limits::default(),
        }
    }

    fn workspace(&self) -> PathBuf {
        PathBuf::from(&self.claims.workspace)
    }

    fn repo(&self) -> PathBuf {
        self.base.join("repo")
    }

    /// Run `body` as `operation`, confining its paths as the server does.
    fn call(&self, operation: Operation, body: serde_json::Value) -> Result<Vec<u8>, Failure> {
        let request = BackendRequest::decode(operation, body.to_string().as_bytes()).unwrap();
        let confined = request
            .paths()
            .into_iter()
            .map(|path| self.claims.confine(Path::new(path)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| Failure::new(error.code(), error.to_string()))?;
        let call = Call {
            operation,
            claims: &self.claims,
            confined,
            limits: &self.limits,
        };
        self.backend.call(&call, request).map(|done| done.body)
    }

    fn status(&self) -> Result<git::StatusResponse, Failure> {
        self.call(Operation::GitStatus, serde_json::json!({}))
            .map(|body| decode(&body).unwrap())
    }

    fn diff(&self, body: serde_json::Value) -> Result<git::DiffResponse, Failure> {
        self.call(Operation::GitDiff, body)
            .map(|body| decode(&body).unwrap())
    }

    fn log(&self, body: serde_json::Value) -> Result<git::LogResponse, Failure> {
        self.call(Operation::GitLog, body)
            .map(|body| decode(&body).unwrap())
    }

    fn add(&self, paths: &[&str]) -> Result<git::AddResponse, Failure> {
        self.call(Operation::GitAdd, serde_json::json!({ "paths": paths }))
            .map(|body| decode(&body).unwrap())
    }

    fn commit(&self, message: &str) -> Result<git::CommitResponse, Failure> {
        self.call(
            Operation::GitCommit,
            serde_json::json!({ "message": message }),
        )
        .map(|body| decode(&body).unwrap())
    }

    fn show(&self, body: serde_json::Value) -> Result<git::ShowResponse, Failure> {
        self.call(Operation::GitShow, body)
            .map(|body| decode(&body).unwrap())
    }

    fn restore(&self, body: serde_json::Value) -> Result<git::RestoreResponse, Failure> {
        self.call(Operation::GitRestore, body)
            .map(|body| decode(&body).unwrap())
    }

    fn head_of(&self, branch: &str) -> String {
        host_git(&self.repo(), &["rev-parse", branch])
    }
}

fn code<T: std::fmt::Debug>(result: Result<T, Failure>) -> ErrorCode {
    result.unwrap_err().code
}

#[test]
fn status_diff_add_commit_and_log_on_the_run_branch() {
    let fx = Fixture::new();
    let status = fx.status().unwrap();
    assert_eq!(status.branch.as_deref(), Some("dagq/run-1"));
    assert!(status.entries.is_empty());

    std::fs::write(fx.workspace().join("README"), "hello\nworld\n").unwrap();
    std::fs::create_dir(fx.workspace().join("src")).unwrap();
    std::fs::write(fx.workspace().join("src/new file.rs"), "fn main() {}\n").unwrap();
    let status = fx.status().unwrap();
    let entries: Vec<_> = status
        .entries
        .iter()
        .map(|entry| (entry.status.as_str(), entry.path.as_str()))
        .collect();
    assert_eq!(entries, [(" M", "README"), ("??", "src/new file.rs")]);

    let diff = fx.diff(serde_json::json!({})).unwrap();
    assert!(diff.diff.contains("+world"), "{}", diff.diff);
    assert!(!diff.truncated);
    let absolute = fx.workspace().join("README");
    let diff = fx.diff(serde_json::json!({ "paths": [absolute] })).unwrap();
    assert!(diff.diff.contains("+world"));
    let diff = fx.diff(serde_json::json!({ "paths": ["src"] })).unwrap();
    assert_eq!(diff.diff, "");

    fx.add(&["README", "src/new file.rs"]).unwrap();
    let staged = fx.diff(serde_json::json!({ "staged": true })).unwrap();
    assert!(staged.diff.contains("+world"));
    assert!(staged.diff.contains("src/new file.rs"));
    assert_eq!(fx.diff(serde_json::json!({})).unwrap().diff, "");

    let main = fx.head_of("main");
    let run_2 = fx.head_of("dagq/run-2");
    let commit = fx.commit("  feat: add a file\n\nbody\n").unwrap().commit;
    assert_eq!(commit.len(), 40);
    assert_eq!(fx.head_of("dagq/run-1"), commit);
    assert_eq!(fx.head_of("main"), main);
    assert_eq!(fx.head_of("dagq/run-2"), run_2);
    assert_eq!(
        host_git(&fx.workspace(), &["log", "-1", "--format=%an <%ae>|%cn|%B"]),
        "Worker <worker@example.com>|Worker|feat: add a file\n\nbody"
    );
    assert!(fx.status().unwrap().entries.is_empty());

    let log = fx.log(serde_json::json!({})).unwrap();
    let subjects: Vec<_> = log.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["feat: add a file", "first"]);
    assert_eq!(log.commits[0].commit, commit);
    assert_eq!(log.commits[0].author, "Worker");
    assert!(log.commits[0].time > 0);
    let log = fx.log(serde_json::json!({ "limit": 1 })).unwrap();
    assert_eq!(log.commits.len(), 1);
    assert!(
        fx.log(serde_json::json!({ "limit": 0 }))
            .unwrap()
            .commits
            .is_empty()
    );
    let log = fx.log(serde_json::json!({ "limit": 100_000 })).unwrap();
    assert_eq!(log.commits.len(), 2);
}

#[test]
fn add_leaves_out_the_temporary_files_a_stopped_broker_left() {
    let fx = Fixture::new();
    let workspace = fx.workspace();
    // What fs.write leaves when the broker stops between writing and
    // renaming: a `.dagq-broker-<uuid>.tmp` next to the target.
    let top = format!(".dagq-broker-{}.tmp", uuid::Uuid::new_v4());
    let nested = format!("src/.dagq-broker-{}.tmp", uuid::Uuid::new_v4());
    std::fs::create_dir(workspace.join("src")).unwrap();
    std::fs::write(workspace.join(&top), "half\n").unwrap();
    std::fs::write(workspace.join(&nested), "half\n").unwrap();
    std::fs::write(workspace.join("src/lib.rs"), "\n").unwrap();
    std::fs::write(workspace.join("README"), "changed\n").unwrap();
    // A name that only looks alike is an ordinary file.
    std::fs::write(workspace.join("src/.dagq-broker-.tmp"), "\n").unwrap();

    let staged = |fx: &Fixture| -> Vec<String> {
        let names = host_git(&fx.workspace(), &["diff", "--cached", "--name-only"]);
        names.lines().map(str::to_owned).collect()
    };
    fx.add(&["src"]).unwrap();
    assert_eq!(staged(&fx), ["src/.dagq-broker-.tmp", "src/lib.rs"]);
    fx.add(&["."]).unwrap();
    assert_eq!(
        staged(&fx),
        ["README", "src/.dagq-broker-.tmp", "src/lib.rs"]
    );
    let untracked = host_git(&workspace, &["status", "--porcelain"]);
    assert!(untracked.contains(&format!("?? {top}")), "{untracked}");
    assert!(untracked.contains(&format!("?? {nested}")), "{untracked}");

    // Named alone, or with other paths, the request is refused and nothing
    // of it is staged.
    std::fs::write(workspace.join("README"), "changed again\n").unwrap();
    for paths in [vec![top.as_str()], vec!["README", nested.as_str()]] {
        let failure = fx.add(&paths).unwrap_err();
        assert_eq!(failure.code, ErrorCode::InvalidRequest, "{failure:?}");
        assert!(failure.message.contains("temporary"), "{failure:?}");
    }
    assert_eq!(
        staged(&fx),
        ["README", "src/.dagq-broker-.tmp", "src/lib.rs"]
    );
    assert_eq!(
        host_git(&workspace, &["diff", "--name-only"]),
        "README",
        "the refused request staged nothing"
    );
    let commit = fx.commit("feat: files").unwrap().commit;
    let tree = host_git(&workspace, &["ls-tree", "-r", "--name-only", &commit]);
    assert!(!tree.contains(&top) && !tree.contains(&nested), "{tree}");
}

#[test]
fn show_shows_the_run_branchs_history_only() {
    let fx = Fixture::new();
    std::fs::write(fx.workspace().join("README"), "hello\nshown-line\n").unwrap();
    std::fs::write(fx.workspace().join("other"), "other\n").unwrap();
    fx.add(&["README", "other"]).unwrap();
    let commit = fx.commit("feat: shown").unwrap().commit;

    let show = fx.show(serde_json::json!({})).unwrap();
    assert_eq!(show.commit, commit);
    assert!(!show.truncated);
    assert!(
        show.show.starts_with(&format!("commit {commit}")),
        "{}",
        show.show
    );
    assert!(show.show.contains("Author: Worker <worker@example.com>"));
    assert!(show.show.contains("feat: shown"));
    assert!(show.show.contains("+shown-line"));
    assert!(show.show.contains("other"));
    let only = fx
        .show(serde_json::json!({ "commit": commit, "paths": ["README"] }))
        .unwrap();
    assert!(only.show.contains("+shown-line"));
    assert!(!only.show.contains("b/other"), "{}", only.show);
    let first = fx.show(serde_json::json!({ "commit": "HEAD~1" })).unwrap();
    assert_eq!(first.commit, fx.head_of("main"));
    assert!(first.show.contains("first"));
    // main is the run branch's base, so in its history.
    assert_eq!(
        fx.show(serde_json::json!({ "commit": "main" }))
            .unwrap()
            .commit,
        fx.head_of("main")
    );

    // Another run's commits are not in this run's history.
    let theirs = fx.base.join("runs/run-2/worktree");
    std::fs::write(theirs.join("secret"), "theirs\n").unwrap();
    host_git(&theirs, &["add", "secret"]);
    host_git(&theirs, &["commit", "-q", "-m", "their work"]);
    for revision in ["dagq/run-2", &fx.head_of("dagq/run-2")] {
        let failure = fx
            .show(serde_json::json!({ "commit": revision }))
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::WorkspaceViolation, "{revision}");
        assert!(!failure.message.contains("theirs"), "{failure:?}");
    }
    for revision in ["--output=/tmp/x", "-p", ""] {
        let failure = fx.show(serde_json::json!({ "commit": revision }));
        assert_eq!(code(failure), ErrorCode::InvalidRequest, "{revision:?}");
    }
    for revision in ["no-such-branch", "HEAD^{tree}", "HEAD:README"] {
        let failure = fx.show(serde_json::json!({ "commit": revision }));
        assert_eq!(code(failure), ErrorCode::BackendError, "{revision:?}");
    }
    let failure = fx.show(serde_json::json!({ "paths": [".git"] }));
    assert_eq!(code(failure), ErrorCode::WorkspaceViolation);
}

#[test]
fn restore_puts_back_the_worktree_or_the_index_only() {
    let mut fx = Fixture::new();
    let readme = fx.workspace().join("README");
    std::fs::write(&readme, "changed\n").unwrap();
    fx.restore(serde_json::json!({ "paths": ["README"] }))
        .unwrap();
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), "hello\n");

    // Unstage: the index goes back to HEAD, the worktree keeps the change.
    std::fs::write(&readme, "staged\n").unwrap();
    fx.add(&["README"]).unwrap();
    fx.restore(serde_json::json!({ "staged": true, "paths": ["README"] }))
        .unwrap();
    let status = fx.status().unwrap();
    assert_eq!(status.entries[0].status, " M");
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), "staged\n");
    // The whole workspace.
    fx.restore(serde_json::json!({ "paths": ["."] })).unwrap();
    assert!(fx.status().unwrap().entries.is_empty());

    assert_eq!(
        code(fx.restore(serde_json::json!({ "paths": [] }))),
        ErrorCode::InvalidRequest
    );
    for path in [".git", "../run-2/worktree/README"] {
        let failure = fx.restore(serde_json::json!({ "paths": [path] }));
        assert_eq!(code(failure), ErrorCode::WorkspaceViolation, "{path}");
    }
    let theirs = fx.base.join("runs/run-2/worktree");
    std::fs::write(theirs.join("README"), "their change\n").unwrap();
    symlink(&theirs, fx.workspace().join("other")).unwrap();
    let failure = fx.restore(serde_json::json!({ "paths": ["other/README"] }));
    assert_eq!(code(failure), ErrorCode::BackendError);
    assert_eq!(
        std::fs::read_to_string(theirs.join("README")).unwrap(),
        "their change\n"
    );
    assert_eq!(
        code(fx.restore(serde_json::json!({ "paths": ["no-such-file"] }))),
        ErrorCode::BackendError
    );

    // A long show is cut at the limit.
    fx.limits.output_limit_bytes = 300;
    let long: String = (0..100).map(|line| format!("line é {line}\n")).collect();
    std::fs::write(&readme, long).unwrap();
    fx.add(&["README"]).unwrap();
    fx.commit("long").unwrap();
    let show = fx.show(serde_json::json!({})).unwrap();
    assert!(show.truncated);
    assert!(show.show.len() <= 300);
}

#[test]
fn a_commit_is_made_only_on_the_run_branch() {
    let fx = Fixture::new();
    std::fs::write(fx.workspace().join("a"), "a\n").unwrap();
    fx.add(&["a"]).unwrap();
    assert_eq!(code(fx.commit(" \n ")), ErrorCode::InvalidRequest);

    let before = fx.head_of("dagq/run-1");
    host_git(&fx.workspace(), &["checkout", "-q", "-b", "elsewhere"]);
    let failure = fx.commit("m").unwrap_err();
    assert_eq!(failure.code, ErrorCode::WorkspaceViolation);
    assert!(
        failure.message.contains("refs/heads/elsewhere"),
        "{failure:?}"
    );
    host_git(&fx.workspace(), &["checkout", "-q", "--detach"]);
    let failure = fx.commit("m").unwrap_err();
    assert_eq!(failure.code, ErrorCode::WorkspaceViolation);
    assert!(failure.message.contains("detached"), "{failure:?}");
    assert_eq!(fx.status().unwrap().branch, None);
    let run_2 = fx.head_of("dagq/run-2");
    host_git(
        &fx.workspace(),
        &["symbolic-ref", "HEAD", "refs/heads/dagq/run-2"],
    );
    assert_eq!(code(fx.commit("m")), ErrorCode::WorkspaceViolation);
    assert_eq!(fx.head_of("dagq/run-2"), run_2);
    assert_eq!(fx.head_of("dagq/run-1"), before);
    assert_eq!(fx.head_of("elsewhere"), before);

    host_git(
        &fx.workspace(),
        &["symbolic-ref", "HEAD", "refs/heads/dagq/run-1"],
    );
    fx.commit("a").unwrap();
    let failure = fx.commit("again").unwrap_err();
    assert_eq!(failure.code, ErrorCode::BackendError);
    assert!(failure.message.contains("nothing to commit"), "{failure:?}");
}

#[test]
fn only_the_tokens_worktree_is_reached() {
    let fx = Fixture::new();
    let theirs = fx.base.join("runs/run-2/worktree");
    std::fs::write(theirs.join("theirs"), "x\n").unwrap();
    for path in [
        "../run-2/worktree/theirs".to_owned(),
        theirs.join("theirs").to_string_lossy().into_owned(),
        fx.repo().join("README").to_string_lossy().into_owned(),
    ] {
        assert_eq!(
            code(fx.add(&[&path])),
            ErrorCode::WorkspaceViolation,
            "{path}"
        );
        let diff = fx.diff(serde_json::json!({ "paths": [path] }));
        assert_eq!(code(diff), ErrorCode::WorkspaceViolation);
    }
    for path in [".git", ".GIT/config", "./.git"] {
        assert_eq!(
            code(fx.add(&[path])),
            ErrorCode::WorkspaceViolation,
            "{path}"
        );
    }
    // A symlink out is added as a link, never followed.
    symlink(&theirs, fx.workspace().join("other")).unwrap();
    assert_eq!(code(fx.add(&["other/theirs"])), ErrorCode::BackendError);
    assert_eq!(
        host_git(&theirs, &["status", "--porcelain"]),
        "?? theirs",
        "run-2's index is untouched"
    );
    assert_eq!(code(fx.add(&[])), ErrorCode::InvalidRequest);

    // A .git rewritten to another worktree's gitdir, or to the main
    // checkout's, is refused before git touches it.
    let dot_git = fx.workspace().join(".git");
    let own = std::fs::read_to_string(&dot_git).unwrap();
    let run_2_gitdir = std::fs::read_to_string(theirs.join(".git")).unwrap();
    std::fs::write(&dot_git, &run_2_gitdir).unwrap();
    let failure = fx.status().unwrap_err();
    assert_eq!(failure.code, ErrorCode::WorkspaceViolation);
    assert!(failure.message.contains("another worktree"), "{failure:?}");
    assert_eq!(code(fx.commit("m")), ErrorCode::WorkspaceViolation);
    let common = fx.repo().join(".git");
    std::fs::write(&dot_git, format!("gitdir: {}\n", common.display())).unwrap();
    assert_eq!(code(fx.add(&["README"])), ErrorCode::WorkspaceViolation);

    // A .git that is a symlink, a directory or missing.
    std::fs::remove_file(&dot_git).unwrap();
    assert_eq!(code(fx.status()), ErrorCode::BackendError);
    symlink(theirs.join(".git"), &dot_git).unwrap();
    assert_eq!(code(fx.status()), ErrorCode::WorkspaceViolation);
    std::fs::remove_file(&dot_git).unwrap();
    std::fs::create_dir(&dot_git).unwrap();
    assert_eq!(code(fx.status()), ErrorCode::WorkspaceViolation);
    std::fs::remove_dir(&dot_git).unwrap();
    std::fs::write(&dot_git, own).unwrap();
    assert!(fx.status().is_ok());

    // The workspace itself swapped for a symlink.
    let run_1 = fx.base.join("runs/run-1");
    std::fs::rename(run_1.join("worktree"), run_1.join("moved")).unwrap();
    symlink(run_1.join("moved"), run_1.join("worktree")).unwrap();
    assert_eq!(code(fx.status()), ErrorCode::WorkspaceViolation);
}

/// A script that leaves `marker` when anything runs it.
fn tripwire(dir: &Path, name: &str, marker: &Path) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho {name} >> '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn hooks_credential_helpers_and_the_repositorys_programs_never_run() {
    let fx = Fixture::new();
    let marker = fx.base.join("ran");
    let bin = fx.base.join("bin");
    let hooks = fx.base.join("hooks");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&hooks).unwrap();
    let common_hooks = fx.repo().join(".git/hooks");
    for hook in [
        "pre-commit",
        "commit-msg",
        "post-commit",
        "reference-transaction",
        "post-index-change",
        "pre-push",
        "fsmonitor-watchman",
    ] {
        tripwire(&hooks, hook, &marker);
        tripwire(&common_hooks, hook, &marker);
    }
    let program = |name: &str| tripwire(&bin, name, &marker).display().to_string();
    let config = [
        ("core.hooksPath", hooks.display().to_string()),
        ("credential.helper", format!("!{}", program("credential"))),
        ("core.askPass", program("askpass")),
        ("core.fsmonitor", program("fsmonitor")),
        ("core.pager", program("pager")),
        ("core.editor", program("editor")),
        ("diff.external", program("external-diff")),
        ("diff.evil.textconv", program("textconv")),
        ("diff.evil.command", program("diff-command")),
        ("filter.evil.clean", program("clean")),
        ("filter.evil.smudge", program("smudge")),
        ("filter.evil.required", "true".to_owned()),
        ("filter.deep.dotted.process", program("process")),
        ("commit.gpgSign", "true".to_owned()),
        ("gpg.program", program("gpg")),
        ("log.showSignature", "true".to_owned()),
        ("gc.auto", "1".to_owned()),
        (
            "remote.origin.url",
            fx.base.join("upstream").display().to_string(),
        ),
    ];
    for (key, value) in &config {
        host_git(&fx.repo(), &["config", key, value]);
    }
    std::fs::write(
        fx.workspace().join(".gitattributes"),
        "* filter=evil diff=evil\n*.deep filter=deep.dotted\n",
    )
    .unwrap();
    std::fs::write(fx.workspace().join("README"), "changed\n").unwrap();
    std::fs::write(fx.workspace().join("a.deep"), "deep\n").unwrap();

    fx.status().unwrap();
    fx.add(&[".gitattributes", "README", "a.deep"]).unwrap();
    fx.diff(serde_json::json!({ "staged": true })).unwrap();
    fx.diff(serde_json::json!({})).unwrap();
    let commit = fx.commit("with every trap set").unwrap().commit;
    fx.log(serde_json::json!({})).unwrap();
    fx.show(serde_json::json!({})).unwrap();
    // restore writes the worktree from the index: the smudge filter's turn.
    std::fs::write(fx.workspace().join("README"), "again\n").unwrap();
    std::fs::write(fx.workspace().join("a.deep"), "again\n").unwrap();
    fx.restore(serde_json::json!({ "paths": ["README", "a.deep"] }))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(fx.workspace().join("README")).unwrap(),
        "changed\n"
    );
    fx.status().unwrap();

    assert!(
        !marker.exists(),
        "ran: {}",
        std::fs::read_to_string(&marker).unwrap_or_default()
    );
    // The content went in as it is, unfiltered and unsigned.
    assert_eq!(
        host_git(&fx.repo(), &["cat-file", "-p", &format!("{commit}:README")]),
        "changed"
    );
    let raw = host_git(&fx.repo(), &["cat-file", "-p", &commit]);
    assert!(!raw.contains("gpgsig"), "{raw}");

    // The traps are live: git without the broker's settings springs them.
    std::fs::write(fx.workspace().join("b"), "b\n").unwrap();
    host_git(&fx.workspace(), &["add", "b"]);
    assert!(marker.exists());
}

#[test]
fn a_long_diff_is_truncated_and_other_output_is_bounded() {
    let mut fx = Fixture::new();
    let long: String = (0..400).map(|line| format!("line é {line}\n")).collect();
    std::fs::write(fx.workspace().join("README"), &long).unwrap();
    fx.limits.output_limit_bytes = 1001;
    let diff = fx.diff(serde_json::json!({})).unwrap();
    assert!(diff.truncated);
    assert!(
        diff.diff.len() <= 1001 && diff.diff.len() >= 999,
        "{}",
        diff.diff.len()
    );
    assert!(!diff.diff.contains('\u{fffd}'));

    for n in 0..40 {
        std::fs::write(fx.workspace().join(format!("untracked-{n}")), "").unwrap();
    }
    fx.limits.output_limit_bytes = 200;
    let failure = fx.status().unwrap_err();
    assert_eq!(failure.code, ErrorCode::OutputLimit, "{failure:?}");
}

#[test]
fn a_git_that_does_not_finish_is_stopped() {
    let mut fx = Fixture::new();
    let slow = fx.base.join("slow-git");
    std::fs::write(&slow, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    fx.backend = GitBackend::with_program(vec![fx.base.join("runs")], slow);
    fx.limits.exec_timeout_secs = 1;
    let started = Instant::now();
    assert_eq!(code(fx.status()), ErrorCode::Timeout);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn what_git_leaves_holding_its_pipes_is_stopped_too() {
    let mut fx = Fixture::new();
    let lingering = fx.base.join("lingering-git");
    let pid_file = fx.base.join("lingering.pid");
    std::fs::write(
        &lingering,
        format!(
            "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nexit 0\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&lingering, std::fs::Permissions::from_mode(0o755)).unwrap();
    fx.backend = GitBackend::with_program(vec![fx.base.join("runs")], lingering);
    // Long enough for the script to start and write the pid under load.
    fx.limits.exec_timeout_secs = 5;
    let started = Instant::now();
    // Git itself ended well; what it left behind is killed at the deadline
    // instead of holding the answer.
    let _ = fx.status();
    assert!(started.elapsed() < Duration::from_secs(20));
    let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    // SAFETY: signal 0 only asks whether the process exists.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "the leftover {pid} is still alive"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_gitdir_the_worker_made_in_its_workspace_is_refused() {
    let fx = Fixture::new();
    let marker = fx.base.join("ran");
    let evil = fx.workspace().join("evil");
    host_git(&fx.base, &["init", "-q", "--bare", evil.to_str().unwrap()]);
    host_git(
        &evil,
        &[
            "config",
            "core.fsmonitor",
            &format!("!touch {}", marker.display()),
        ],
    );
    host_git(&evil, &["config", "protocol.ext.allow", "always"]);
    std::fs::write(
        evil.join("gitdir"),
        format!("{}\n", fx.workspace().join(".git").display()),
    )
    .unwrap();
    let dot_git = fx.workspace().join(".git");
    std::fs::write(&dot_git, format!("gitdir: {}\n", evil.display())).unwrap();
    for failure in [fx.status().unwrap_err(), fx.add(&["README"]).unwrap_err()] {
        assert_eq!(failure.code, ErrorCode::WorkspaceViolation, "{failure:?}");
        assert!(failure.message.contains("inside the runs"), "{failure:?}");
    }
    // Made like a worktree's, under a `worktrees` dir in the workspace.
    let fake = fx.workspace().join("repo/worktrees/x");
    std::fs::create_dir_all(&fake).unwrap();
    std::fs::write(fake.join("commondir"), "../..\n").unwrap();
    std::fs::write(fake.join("gitdir"), format!("{}\n", dot_git.display())).unwrap();
    std::fs::write(&dot_git, format!("gitdir: {}\n", fake.display())).unwrap();
    assert_eq!(code(fx.status()), ErrorCode::WorkspaceViolation);
    // Relative, or not a gitdir line at all.
    for pointer in ["gitdir: ../x\n", "not a pointer\n", ""] {
        std::fs::write(&dot_git, pointer).unwrap();
        assert_eq!(
            code(fx.status()),
            ErrorCode::WorkspaceViolation,
            "{pointer:?}"
        );
    }
    assert!(!marker.exists());
}

#[test]
fn refuses_what_is_not_a_git_operation_and_a_workspace_outside_the_roots() {
    let mut fx = Fixture::new();
    let request = BackendRequest::decode(Operation::FsRead, br#"{"path":"a"}"#).unwrap();
    let call = Call {
        operation: Operation::FsRead,
        claims: &fx.claims,
        confined: vec![fx.workspace().join("a")],
        limits: &fx.limits,
    };
    assert_eq!(
        code(fx.backend.call(&call, request)),
        ErrorCode::InvalidRequest
    );

    fx.backend = GitBackend::new(vec![fx.base.join("elsewhere")]);
    assert_eq!(code(fx.status()), ErrorCode::WorkspaceViolation);
}

#[test]
fn the_filters_of_the_config_are_switched_off_by_name() {
    let fx = Fixture::new();
    host_git(&fx.repo(), &["config", "filter.a.clean", "x"]);
    host_git(&fx.repo(), &["config", "filter.b.c.smudge", "y"]);
    host_git(&fx.repo(), &["config", "filter.a.required", "true"]);
    let call_claims = &fx.claims;
    let call = Call {
        operation: Operation::GitStatus,
        claims: call_claims,
        confined: Vec::new(),
        limits: &fx.limits,
    };
    let repo = fx.backend.open(&call).unwrap();
    let filters: Vec<_> = repo
        .config
        .iter()
        .filter(|entry| entry.starts_with("filter."))
        .map(String::as_str)
        .collect();
    assert_eq!(
        filters,
        [
            "filter.a.clean=",
            "filter.a.smudge=",
            "filter.a.process=",
            "filter.a.required=false",
            "filter.b.c.clean=",
            "filter.b.c.smudge=",
            "filter.b.c.process=",
            "filter.b.c.required=false",
        ]
    );
    assert!(repo.home.is_dir());
    std::fs::remove_dir_all(&repo.home).unwrap();
}
