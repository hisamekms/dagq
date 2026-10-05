//! The resource broker's signing key and a run's token ([Broker] token,
//! ADR-t827-2 decision 2): [`ensure_key`] keeps the queue's key at
//! `<queue dir>/broker/key` (32 random bytes, mode 0600, made when missing),
//! and [`issue_run_token`] signs a worker run's claims with it. Only the
//! supervisor issues tokens; no AI actor has a command that does.
//!
//! Neither the key nor a token is ever written to a log, an event or an
//! error: [`SigningKey`] and [`BrokerSessionToken`] redact their `Debug`,
//! and the errors here name the key's path, never its bytes.
//!
//! [Broker]: ../../docs/design/broker.md

use crate::infrastructure::git_binary::git_executable;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use dagq_broker_protocol::{
    BrokerCapability, BrokerRole, BrokerSessionToken, Committer, KEY_LEN, SigningKey,
    TOKEN_VERSION, TokenClaims, sign,
};

use crate::application::broker_run::{Grant, HeldToken, IssuedRun, RunTokens};
use crate::domain::{
    ActorContext, ActorRole, RunId, TaskRun,
    broker_usage::{DIRECT_TOOLS_LOG, ToolUsage},
};

/// The broker's dir under the queue dir.
pub const BROKER_DIR: &str = "broker";
/// The signing key's file in [`BROKER_DIR`].
pub const KEY_FILE: &str = "key";
/// The mode of the key file.
pub const KEY_MODE: u32 = 0o600;
/// How long a token lives: `exp = iat + 12 hours`.
pub const TOKEN_TTL_SECS: u64 = 12 * 60 * 60;

/// `<queue dir>/broker/key`.
pub fn key_path(queue_dir: &Path) -> PathBuf {
    queue_dir.join(BROKER_DIR).join(KEY_FILE)
}

/// The queue's signing key, made when there is none: 32 random bytes in a
/// file of mode 0600, put in place with a link from a temporary file so a
/// reader never sees half a key and two supervisors making one at once end
/// with the same key. A key of wider mode is narrowed to 0600; a key of
/// another length is an error (a person removes it to make a new one, which
/// invalidates every token).
pub fn ensure_key(queue_dir: &Path) -> Result<SigningKey> {
    let path = key_path(queue_dir);
    if let Some(key) = read_key(&path)? {
        return Ok(key);
    }
    let dir = queue_dir.join(BROKER_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut bytes = [0u8; KEY_LEN];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("draw the broker key's random bytes: {error}"))?;
    let temporary = dir.join(format!(".{KEY_FILE}.{}", uuid::Uuid::new_v4()));
    let written = write_new(&temporary, &bytes).and_then(|()| {
        match fs::hard_link(&temporary, &path) {
            Ok(()) => Ok(()),
            // Another supervisor made it first: keep theirs.
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    });
    let _ = fs::remove_file(&temporary);
    written.with_context(|| format!("write the broker key {}", path.display()))?;
    read_key(&path)?.with_context(|| format!("the broker key {} vanished", path.display()))
}

fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(KEY_MODE)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn read_key(path: &Path) -> Result<Option<SigningKey>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read the broker key {}", path.display()));
        }
    };
    let Ok(key) = SigningKey::from_bytes(&bytes) else {
        bail!(
            "the broker key {} is not {KEY_LEN} bytes; remove it to make a new one",
            path.display()
        );
    };
    let mode = fs::metadata(path)
        .with_context(|| format!("stat the broker key {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o777 != KEY_MODE {
        fs::set_permissions(path, fs::Permissions::from_mode(KEY_MODE))
            .with_context(|| format!("narrow the broker key {} to 0600", path.display()))?;
    }
    Ok(Some(key))
}

/// What a token of `role` may do through the broker: every capability to a
/// worker (Phase 1's five and `package.install`, which runs only the
/// commands of `[broker.package]`) and nothing to any other role
/// (ADR-t827-4 decision 5).
pub fn broker_grants(role: ActorRole) -> BTreeSet<BrokerCapability> {
    match role {
        ActorRole::Worker => BrokerCapability::ALL.into_iter().collect(),
        _ => BTreeSet::new(),
    }
}

/// A token issued now and the claims it carries (for the event
/// `broker_token_issued`, which records the claims' `jti`, capabilities and
/// `exp`, never the token).
#[derive(Debug, Clone)]
pub struct IssuedToken {
    pub claims: TokenClaims,
    pub token: BrokerSessionToken,
}

/// Sign the token of the worker `actor` (with its run and task) working in
/// `workspace`, committing as `committer`, at `now` (UNIX seconds). The
/// capabilities are [`broker_grants`] of the actor's role, the workspace is
/// canonicalized, the branch is `dagq/<run id>` and `jti` a new UUID v4.
pub fn issue_run_token(
    key: &SigningKey,
    actor: &ActorContext,
    workspace: &Path,
    committer: Committer,
    now: u64,
) -> Result<IssuedToken> {
    if actor.role() != ActorRole::Worker {
        bail!(
            "a broker token is issued to a worker, not {}",
            actor.role().as_str()
        );
    }
    let (Some(run), Some(task)) = (actor.run_id(), actor.task_id()) else {
        bail!("a broker token needs the worker's run and task");
    };
    let task_id = u64::try_from(task.as_i64())
        .with_context(|| format!("task {task} is not a positive id"))?;
    if !workspace.is_absolute() {
        bail!(
            "the broker token's workspace {} is not absolute",
            workspace.display()
        );
    }
    let workspace = fs::canonicalize(workspace)
        .with_context(|| format!("resolve the workspace {}", workspace.display()))?;
    let workspace = workspace
        .to_str()
        .with_context(|| format!("the workspace {} is not UTF-8", workspace.display()))?
        .to_owned();
    let claims = TokenClaims {
        v: TOKEN_VERSION,
        jti: uuid::Uuid::new_v4().to_string(),
        actor_id: actor.actor_id().to_owned(),
        role: BrokerRole::Worker,
        task_id,
        run_id: run.to_string(),
        workspace,
        branch: format!("dagq/{run}"),
        committer,
        capabilities: broker_grants(actor.role()),
        iat: now,
        exp: now + TOKEN_TTL_SECS,
    };
    let token = sign(key, &claims).map_err(|error| anyhow::anyhow!("sign the token: {error}"))?;
    Ok(IssuedToken { claims, token })
}

/// The runs' token files under [`BROKER_DIR`], outside every mount.
pub const TOKENS_DIR: &str = "tokens";
/// The active marks under [`BROKER_DIR`] (`<jti>` holding the run's id),
/// which the broker reads for every request.
pub const ACTIVE_DIR: &str = "active";
/// The broker's audit's dir in [`BROKER_DIR`].
const AUDIT_DIR: &str = "audit";
/// The mode of a token file and an active mark.
pub const TOKEN_MODE: u32 = 0o600;

/// `<queue dir>/broker/tokens/<run id>`.
pub fn token_path(queue_dir: &Path, run: &str) -> PathBuf {
    queue_dir.join(BROKER_DIR).join(TOKENS_DIR).join(run)
}

fn active_dir(queue_dir: &Path) -> PathBuf {
    queue_dir.join(BROKER_DIR).join(ACTIVE_DIR)
}

/// Put `bytes` at `path` with mode 0600 through a new temporary file in
/// the same dir and a rename, so a reader sees the old file or the new.
fn put_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no dir", path.display()))?;
    crate::application::RunFiles::create_dir_all(&super::run_files::LocalRunFiles, dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let (dir, name) = super::agent_dir::Directory::parent(path)?;
    dir.replace(name, |file| {
        file.write_all(bytes)?;
        file.sync_all()
    })
    .map(drop)
    .with_context(|| format!("write {}", path.display()))
}

fn remove_if_there(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

/// The identity a run's broker commits as: `user.name` and `user.email`
/// of the repository at `dir` (the container has no `~/.gitconfig`).
pub fn git_committer(dir: &Path) -> Result<Committer> {
    let read = |key: &str| -> Result<String> {
        let output = crate::infrastructure::adapters::unpiped_output(
            std::process::Command::new(git_executable()?)
                .arg("-C")
                .arg(dir)
                .args(["config", "--get", key]),
        )
        .with_context(|| format!("run git config {key}"))?;
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() || value.is_empty() {
            bail!(
                "git config {key} is not set for {}: the broker commits as it",
                dir.display()
            );
        }
        Ok(value)
    };
    Ok(Committer {
        name: read("user.name")?,
        email: read("user.email")?,
    })
}

/// The runs' tokens of the queue at `queue_dir` ([`RunTokens`]).
pub struct QueueRunTokens {
    pub queue_dir: PathBuf,
}

impl RunTokens for QueueRunTokens {
    fn issue(&self, run: &TaskRun, grant: &Grant, now: u64) -> Result<IssuedRun> {
        let workspace = Path::new(run.worktree_path().context("the run has no worktree")?);
        let run_dir = Path::new(run.run_dir().context("the run has no run dir")?);
        let key = ensure_key(&self.queue_dir)?;
        let actor = ActorContext::worker(run.id(), run.task_id());
        let issued = issue_run_token(&key, &actor, workspace, git_committer(workspace)?, now)?;
        let jti = issued.claims.jti.clone();
        // The mark first: a token file never names a token the broker
        // would refuse as revoked.
        put_private(
            &active_dir(&self.queue_dir).join(&jti),
            run.id().as_str().as_bytes(),
        )?;
        let token_file = token_path(&self.queue_dir, run.id().as_str());
        put_private(
            &token_file,
            format!("{}\n", issued.token.expose()).as_bytes(),
        )?;
        let config = crate::application::broker_run::mcp_config(
            &grant.client,
            grant.port,
            &token_file,
            grant.receipt.as_deref(),
        );
        put_private(
            &crate::application::broker_run::mcp_config_path(run_dir),
            serde_json::to_string_pretty(&config)?.as_bytes(),
        )?;
        Ok(IssuedRun {
            jti,
            capabilities: issued
                .claims
                .capabilities
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
            exp: issued.claims.exp,
        })
    }

    fn revoke(&self, run: &RunId, run_dir: Option<&Path>) -> Result<Vec<String>> {
        let mut revoked = Vec::new();
        for held in self.held()? {
            if held.run_id == run.as_str() {
                self.retire(&held.jti)?;
                revoked.push(held.jti);
            }
        }
        remove_if_there(&token_path(&self.queue_dir, run.as_str()))?;
        if let Some(run_dir) = run_dir {
            let dir = run_dir.join(crate::application::broker_run::RUN_BROKER_DIR);
            match crate::application::RunFiles::remove_dir_all(
                &super::run_files::LocalRunFiles,
                &dir,
            ) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("remove {}", dir.display()));
                }
            }
        }
        Ok(revoked)
    }

    fn retire(&self, jti: &str) -> Result<()> {
        remove_if_there(&active_dir(&self.queue_dir).join(jti))
    }

    fn held(&self) -> Result<Vec<HeldToken>> {
        let dir = active_dir(&self.queue_dir);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
        };
        let mut held = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("read {}", dir.display()))?;
            let Some(jti) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // A temporary file of a mark being put.
            if jti.starts_with('.') {
                continue;
            }
            let run_id = match fs::read_to_string(entry.path()) {
                Ok(text) => text.trim().to_owned(),
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("read {}", entry.path().display()));
                }
            };
            let exp = fs::read_to_string(token_path(&self.queue_dir, &run_id))
                .ok()
                .and_then(|text| {
                    BrokerSessionToken::new(text.trim().to_owned())
                        .unverified_claims()
                        .ok()
                })
                .filter(|claims| claims.jti == jti)
                .map(|claims| claims.exp);
            held.push(HeldToken { jti, run_id, exp });
        }
        held.sort_by(|a, b| a.jti.cmp(&b.jti));
        Ok(held)
    }

    fn token_files(&self) -> Result<Vec<String>> {
        let dir = self.queue_dir.join(BROKER_DIR).join(TOKENS_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
        };
        let mut runs = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("read {}", dir.display()))?;
            // Only files are tokens.
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            match entry.file_name().to_str() {
                // A temporary file of a token being put.
                Some(name) if !name.starts_with('.') => runs.push(name.to_owned()),
                _ => {}
            }
        }
        runs.sort();
        Ok(runs)
    }

    fn usage(&self, run: &TaskRun) -> Result<ToolUsage> {
        let direct = match run.run_dir() {
            Some(dir) => {
                // The worker writes in its run dir: the log is read as
                // the supervisor reads every run file there, through its
                // pinned dir without following a link, only as a regular
                // file opened without waiting (a FIFO never holds the
                // sweep) and up to `agent_dir::FILE_BYTES`. Anything else
                // is an error, which the sweep warns of.
                let log = Path::new(dir).join(DIRECT_TOOLS_LOG);
                match super::agent_dir::read_file(&log).and_then(super::agent_dir::read_bounded) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
                    Err(error) => {
                        return Err(error).with_context(|| format!("read {}", log.display()));
                    }
                }
            }
            None => String::new(),
        };
        let dir = self.queue_dir.join(BROKER_DIR).join(AUDIT_DIR);
        let audit = crate::application::broker_admin::audit(
            &dir,
            &crate::application::broker_admin::AuditQuery {
                run: Some(run.id().as_str().to_owned()),
                // The day files from the run's start on.
                since: crate::domain::stats::timestamp_millis(run.created_at())
                    .map(|millis| millis - millis.rem_euclid(86_400_000)),
                limit: Some(usize::MAX),
                ..Default::default()
            },
        )
        .with_context(|| format!("read the broker's audit {}", dir.display()))?;
        Ok(ToolUsage::count(&direct, &audit.entries, run.id()))
    }

    /// The key read or made, the dirs of the marks and the token files
    /// made, and the repository's committer read: what [`Self::issue`]
    /// needs for any run of the repository (a run's worktree reads the
    /// repository's config).
    fn ready(&self, repository: &Path) -> Result<()> {
        ensure_key(&self.queue_dir)?;
        for dir in [
            active_dir(&self.queue_dir),
            self.queue_dir.join(BROKER_DIR).join(TOKENS_DIR),
        ] {
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        git_committer(repository)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{RunId, TaskId};
    use dagq_broker_protocol::{TokenError, verify};

    fn committer() -> Committer {
        Committer {
            name: "A".to_owned(),
            email: "a@example.com".to_owned(),
        }
    }

    fn worker(run: &str) -> ActorContext {
        ActorContext::worker(&RunId::new(run).unwrap(), TaskId::new(829))
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_missing_key_is_made_with_mode_0600_and_kept() {
        let queue = tempfile::tempdir().unwrap();
        let path = key_path(queue.path());
        assert!(!path.exists());
        let key = ensure_key(queue.path()).unwrap();
        assert_eq!(fs::read(&path).unwrap().len(), KEY_LEN);
        assert_eq!(mode(&path), 0o600);
        // The same key the next time; no temporary file is left behind.
        assert_eq!(ensure_key(queue.path()).unwrap(), key);
        let names: Vec<_> = fs::read_dir(queue.path().join(BROKER_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [KEY_FILE]);
        // Another queue draws another key.
        let other = tempfile::tempdir().unwrap();
        assert_ne!(ensure_key(other.path()).unwrap(), key);
    }

    #[test]
    fn a_key_of_wider_mode_is_narrowed_and_a_short_one_refused() {
        let queue = tempfile::tempdir().unwrap();
        let key = ensure_key(queue.path()).unwrap();
        let path = key_path(queue.path());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(ensure_key(queue.path()).unwrap(), key);
        assert_eq!(mode(&path), 0o600);

        fs::write(&path, b"short-secret").unwrap();
        let error = format!("{:#}", ensure_key(queue.path()).unwrap_err());
        assert!(error.contains("is not 32 bytes"), "{error}");
        assert!(!error.contains("short-secret"), "{error}");
    }

    /// `required` claims only when a token could be issued (ADR-t838-1):
    /// `ready` makes the key and the dirs, and fails on a key it cannot
    /// use, naming no secret, and on a repository that names no committer.
    #[test]
    fn ready_makes_what_an_issue_needs_and_refuses_a_bad_key_or_no_committer() {
        let queue = tempfile::tempdir().unwrap();
        let repo = queue.path().join("repo");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        fs::create_dir_all(&repo).unwrap();
        git(&["init", "-q"]);
        git(&["config", "user.name", "A"]);
        git(&["config", "user.email", "a@example.com"]);
        let tokens = QueueRunTokens {
            queue_dir: queue.path().to_path_buf(),
        };
        tokens.ready(&repo).unwrap();
        assert_eq!(mode(&key_path(queue.path())), 0o600);
        assert!(active_dir(queue.path()).is_dir());
        assert!(queue.path().join(BROKER_DIR).join(TOKENS_DIR).is_dir());
        tokens.ready(&repo).unwrap();

        git(&["config", "user.name", ""]);
        let error = format!("{:#}", tokens.ready(&repo).unwrap_err());
        assert!(error.contains("git config user.name is not set"), "{error}");
        git(&["config", "user.name", "A"]);

        fs::write(key_path(queue.path()), b"short-secret").unwrap();
        let error = format!("{:#}", tokens.ready(&repo).unwrap_err());
        assert!(error.contains("is not 32 bytes"), "{error}");
        assert!(!error.contains("short-secret"), "{error}");
    }

    #[test]
    fn a_worker_token_verifies_and_holds_the_runs_claims() {
        let queue = tempfile::tempdir().unwrap();
        let key = ensure_key(queue.path()).unwrap();
        let workspace = queue.path().join("runs/run-1/worktree");
        fs::create_dir_all(&workspace).unwrap();
        let issued =
            issue_run_token(&key, &worker("run-1"), &workspace, committer(), 1_000).unwrap();
        let claims = verify(&key, &issued.token, 1_000).unwrap();
        assert_eq!(claims, issued.claims);
        assert_eq!(claims.run_id, "run-1");
        assert_eq!(claims.task_id, 829);
        assert_eq!(claims.actor_id, "worker:run-1");
        assert_eq!(claims.branch, "dagq/run-1");
        assert_eq!(
            Path::new(&claims.workspace),
            fs::canonicalize(&workspace).unwrap()
        );
        assert_eq!(claims.capabilities, broker_grants(ActorRole::Worker));
        assert_eq!(claims.capabilities.len(), BrokerCapability::ALL.len());
        assert_eq!(claims.exp, 1_000 + TOKEN_TTL_SECS);
        assert_eq!(
            verify(&key, &issued.token, 1_000 + TOKEN_TTL_SECS),
            Err(TokenError::Expired)
        );
        // Another run's worktree is outside this token's workspace.
        let other = fs::canonicalize(queue.path())
            .unwrap()
            .join("runs/run-2/worktree/src/lib.rs");
        assert_eq!(claims.confine(&other), Err(TokenError::WorkspaceViolation));
        // Each token gets its own jti.
        let again =
            issue_run_token(&key, &worker("run-1"), &workspace, committer(), 1_000).unwrap();
        assert_ne!(again.claims.jti, claims.jti);
        // Debug names neither the token nor the key.
        let debug = format!("{issued:?} {key:?}");
        assert!(!debug.contains(issued.token.expose()), "{debug}");
        let key_bytes = fs::read(key_path(queue.path())).unwrap();
        assert!(!debug.contains(&format!("{key_bytes:?}")), "{debug}");
    }

    /// A run's usage counts its log and its audit lines. The log, which
    /// the worker can replace, is read only as a regular file: a link
    /// (to a file or to `/dev/zero`) and a FIFO are refused at once, and
    /// a missing log is no direct call.
    #[test]
    fn usage_reads_the_log_only_as_a_regular_file_and_never_waits() {
        let queue = tempfile::tempdir().unwrap();
        let run_dir = queue.path().join("runs/run-1");
        fs::create_dir_all(&run_dir).unwrap();
        let run = TaskRun::restore(crate::domain::RunRecord {
            id: RunId::new("run-1").unwrap(),
            task_id: TaskId::new(839),
            status: crate::domain::RunStatus::Succeeded,
            requested_provider: crate::domain::Provider::Claude,
            actual_provider: crate::domain::Provider::Claude,
            worker_mode: crate::domain::worker::WorkerMode::Headless,
            base_commit: crate::domain::CommitSha::try_from("a".repeat(40)).unwrap(),
            branch: None,
            worktree_path: None,
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: Some(run_dir.to_string_lossy().into_owned()),
            last_error: None,
            workspace_closed_at: None,
            created_at: "2026-10-05 00:00:00".into(),
        })
        .unwrap();
        let tokens = QueueRunTokens {
            queue_dir: queue.path().to_path_buf(),
        };
        let audit = queue.path().join(BROKER_DIR).join(AUDIT_DIR);
        fs::create_dir_all(&audit).unwrap();
        fs::write(
            audit.join("2026-10-05.jsonl"),
            "{\"ts\":\"2026-10-05T01:00:00.000Z\",\"run_id\":\"run-1\",\"op\":\"fs.read\"}\n",
        )
        .unwrap();
        // No log: the audit's line only.
        let usage = tokens.usage(&run).unwrap();
        assert_eq!((usage.brokered, usage.direct), (1, 0));
        let log = run_dir.join(DIRECT_TOOLS_LOG);
        fs::write(&log, "Read\nBash\n").unwrap();
        assert_eq!(tokens.usage(&run).unwrap().direct, 2);

        // A link, to a regular file or to a device, is not followed.
        let elsewhere = queue.path().join("elsewhere.log");
        fs::write(&elsewhere, "Read\n").unwrap();
        for target in [elsewhere.as_path(), Path::new("/dev/zero")] {
            fs::remove_file(&log).unwrap();
            std::os::unix::fs::symlink(target, &log).unwrap();
            let error = format!("{:#}", tokens.usage(&run).unwrap_err());
            assert!(error.contains(DIRECT_TOOLS_LOG), "{error}");
        }
        // A FIFO is refused without waiting for a writer.
        fs::remove_file(&log).unwrap();
        let fifo = std::ffi::CString::new(log.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        let error = format!("{:#}", tokens.usage(&run).unwrap_err());
        assert!(error.contains("not a regular file"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn only_a_worker_with_a_run_gets_a_token() {
        let queue = tempfile::tempdir().unwrap();
        let key = ensure_key(queue.path()).unwrap();
        let workspace = queue.path();
        let error = issue_run_token(&key, &ActorContext::user(), workspace, committer(), 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("issued to a worker"), "{error}");
        let bare = ActorContext::new(ActorRole::Worker, "worker");
        let error = issue_run_token(&key, &bare, workspace, committer(), 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("run and task"), "{error}");
        let error = issue_run_token(
            &key,
            &worker("run-1"),
            Path::new("relative"),
            committer(),
            1,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("not absolute"), "{error}");
        let negative = ActorContext::worker(&RunId::new("run-1").unwrap(), TaskId::new(-1));
        assert!(issue_run_token(&key, &negative, workspace, committer(), 1).is_err());
        assert!(broker_grants(ActorRole::Planner).is_empty());
        assert!(broker_grants(ActorRole::Supervisor).is_empty());
    }
}
