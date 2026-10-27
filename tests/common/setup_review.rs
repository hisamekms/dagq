//! The repository and queue the `setup-review` tests share: a repository
//! whose one commit has `dagq.toml` ([`CONFIG`]) and a README, a queue for
//! it initialized, and the `dagq` and fake agents run in it.

use super::{Bounded, WithoutActor};

use dagq::infrastructure::git_binary::git_executable;
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

pub const CONFIG: &str = "[run.env]\nA = \"1\"\n";

pub fn git(repo: &Path, args: &[&str]) -> String {
    let result = Command::new(git_executable().unwrap())
        .arg("-C")
        .arg(repo)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

/// A repository whose one commit has `dagq.toml` ([`CONFIG`]) and a
/// README, and a queue for it beside it, initialized.
pub struct Fixture {
    pub dir: TempDir,
    pub repo: PathBuf,
    pub db: PathBuf,
}

impl Drop for Fixture {
    /// No service outlives its test.
    fn drop(&mut self) {
        super::service::unserve(&self.db);
    }
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "test"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        fs::write(repo.join("dagq.toml"), CONFIG).unwrap();
        fs::write(repo.join("README.md"), "# A project\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
        let db = dir.path().join("queue").join("queue.db");
        let fixture = Self { dir, repo, db };
        fixture.ok(&[], &["init"]);
        // The job's token is issued by the queue's service at the
        // caller's request, so `setup-review` needs it running.
        super::service::serve(&fixture.db);
        fixture
    }

    /// `dagq --db <queue> args` in the repository as `env`'s actor (the
    /// user without), reaching the queue's service.
    pub fn command(&self, env: &[(&str, &str)], args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
        command
            .without_actor_env()
            .current_dir(&self.repo)
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .envs(env.iter().copied())
            .arg("--db")
            .arg(&self.db)
            .args(args);
        // `setup-review` sends its steps to the queue service, as the
        // command line does (ADR-t3102-2 decision 6).
        super::cli::reach(&mut command, &self.db, args);
        command
    }

    /// Run [`Self::command`], its stdin no terminal.
    pub fn invoke(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        self.command(env, args).bounded_output().unwrap()
    }

    pub fn ok(&self, env: &[(&str, &str)], args: &[&str]) -> Value {
        let output = self.invoke(env, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    pub fn error(&self, env: &[(&str, &str)], args: &[&str]) -> Value {
        let output = self.invoke(env, args);
        assert!(!output.status.success(), "{env:?} {args:?} succeeded");
        serde_json::from_slice(&output.stderr).unwrap()
    }

    /// A fake `claude` that runs `body` in its working directory and
    /// answers as a headless Claude job does.
    pub fn agent(&self, name: &str, body: &str) -> String {
        let path = self.dir.path().join(name);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\ncat >/dev/null\n{body}\necho '{{\"type\":\"result\",\"result\":\"done\"}}'\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_owned()
    }

    pub fn events(&self) -> i64 {
        rusqlite::Connection::open(&self.db)
            .unwrap()
            .query_row("SELECT count(*) FROM run_events", [], |row| row.get(0))
            .unwrap()
    }

    pub fn commits(&self) -> String {
        git(&self.repo, &["rev-list", "--all"])
    }

    /// Whether the temporary checkouts are all gone: no directory under
    /// the queue's and no worktree but the main checkout.
    pub fn places_gone(&self) -> bool {
        let places = self.db.parent().unwrap().join("setup-review");
        let empty = fs::read_dir(&places).map_or(true, |mut dir| dir.next().is_none());
        let worktrees = git(&self.repo, &["worktree", "list", "--porcelain"]);
        empty && worktrees.matches("worktree ").count() == 1
    }
}
