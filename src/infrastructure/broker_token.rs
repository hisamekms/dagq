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

use crate::domain::{ActorContext, ActorRole};

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

/// What a token of `role` may do through the broker. Phase 1 grants all
/// five to a worker and nothing to any other role (ADR-t827-4 decision 5).
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
