//! Terminal credentials, outside worktrees and run directories
//! ([`set_place_allowed`], checked before a set is read or written). The
//! default tries the OS then a private file. Explicit file never opens the
//! Keychain. Each store holds its locks where its set lives
//! ([`CredentialStore::hold`]): the OS store's per OS user, a file's beside
//! its directory.
use crate::application::login::{CredentialStore, Credentials, StoreHold, StoreLock};
use anyhow::{Result, ensure};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// One set at $XDG_CONFIG_HOME/dagq/credentials/login.json, or under
/// ~/.config when unset. Directory 0700, file 0600; atomic same-dir rename.
/// Files and the credential directory may not be symlinks.
pub struct FileCredentials {
    pub directory: PathBuf,
}
impl FileCredentials {
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let root = if let Some(root) = env("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
            PathBuf::from(root)
        } else {
            PathBuf::from(
                env("HOME")
                    .ok_or_else(|| anyhow::anyhow!("HOME or XDG_CONFIG_HOME is required"))?,
            )
            .join(".config")
        };
        ensure!(
            root.is_absolute(),
            "credential config directory must be absolute"
        );
        Ok(Self {
            directory: root.join("dagq/credentials"),
        })
    }
    /// Refuse a directory inside a Git work tree or a run directory
    /// ([`set_place_allowed`]), with the links of its closest existing
    /// ancestor resolved: an `XDG_CONFIG_HOME` there, or a link that leads
    /// there, never gets the person's token or the inbox's in plain text.
    fn check_location(&self) -> Result<()> {
        let existing = self
            .directory
            .ancestors()
            .find(|path| fs::symlink_metadata(path).is_ok())
            .ok_or_else(|| anyhow::anyhow!("the credential directory has no existing ancestor"))?;
        let resolved = existing
            .canonicalize()?
            .join(self.directory.strip_prefix(existing)?);
        let in_worktree = resolved
            .ancestors()
            .any(|path| fs::symlink_metadata(path.join(".git")).is_ok());
        ensure!(
            set_place_allowed(&resolved, &self.directory, in_worktree),
            "the credential directory must be outside Git work trees and run directories: \
             set XDG_CONFIG_HOME elsewhere"
        );
        Ok(())
    }
    fn check_dir(&self) -> Result<()> {
        let meta = fs::symlink_metadata(&self.directory)?;
        ensure!(
            meta.is_dir() && meta.permissions().mode() & 0o077 == 0,
            "credential directory must be private (0700)"
        );
        Ok(())
    }
}
/// Whether the credential directory, as `given` and with its links
/// `resolved`, may hold a set: in no Git work tree (`in_worktree`: an
/// ancestor of the resolved path holds `.git`) and below no directory named
/// `runs` (a queue's run directories, its worktrees under them), as the
/// runner's credential is placed.
pub fn set_place_allowed(resolved: &Path, given: &Path, in_worktree: bool) -> bool {
    let in_runs = |path: &Path| {
        path.components()
            .any(|part| part.as_os_str() == super::location::RUNS_DIR_NAME)
    };
    !in_worktree && !in_runs(resolved) && !in_runs(given)
}
impl CredentialStore for FileCredentials {
    fn load(&self) -> Result<Option<Credentials>> {
        if !self.directory.try_exists()? {
            return Ok(None);
        }
        self.check_location()?;
        self.check_dir()?;
        let mut file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.directory.join("login.json"))
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => anyhow::bail!("cannot open credential file"),
        };
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && private_file_mode(meta.permissions().mode()),
            "credential file must have mode 0600"
        );
        let mut bytes = Vec::new();
        (&mut file).take(256 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 256 * 1024, "credential file is too large");
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| anyhow::anyhow!("invalid credential file; run dagq login"))
    }
    fn save(&self, credentials: &Credentials) -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        self.check_location()?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.directory)?;
        self.check_dir()?;
        let temp = self
            .directory
            .join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&serde_json::to_vec(credentials)?)?;
            file.sync_all()?;
            fs::rename(&temp, self.directory.join("login.json"))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
    fn delete(&self) -> Result<()> {
        if !self.directory.try_exists()? {
            return Ok(());
        }
        self.check_dir()?;
        match fs::remove_file(self.directory.join("login.json")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => anyhow::bail!("cannot remove credential file"),
        }
    }
    /// The file's set is its directory's: the lock is beside it, one for
    /// each `XDG_CONFIG_HOME` (made 0700 when missing).
    fn hold(&self, lock: StoreLock) -> Result<StoreHold> {
        use std::os::unix::fs::DirBuilderExt;
        let parent = self
            .directory
            .parent()
            .ok_or_else(|| anyhow::anyhow!("the credential directory has no parent"))?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        Ok(Box::new(flock(&parent.join(lock_name(lock)))?))
    }
}
/// The file of a store's `lock` in its lock's directory.
fn lock_name(lock: StoreLock) -> &'static str {
    match lock {
        StoreLock::Change => "credentials.lock",
        StoreLock::Issue => "inbox-issue.lock",
    }
}
/// A store's lock held ([`CredentialStore::hold`]): a `flock` of its file,
/// released when dropped.
pub struct CredentialLock(#[allow(dead_code)] fs::File);
/// Wait for the `flock` of the file at `path` (made 0600 when missing, never
/// through a link) and hold it.
fn flock(path: &Path) -> Result<CredentialLock> {
    use std::os::fd::AsRawFd;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    // SAFETY: flock on a descriptor this function owns; it blocks until
    // the lock is free, and goes with the file when dropped.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(CredentialLock(file))
}

/// The person's store `DAGQ_CREDENTIAL_STORE` chooses, with `then`.
fn terminal_store<T>(then: impl FnOnce(&dyn CredentialStore) -> Result<T>) -> Result<T> {
    use crate::application::login::{StoreChoice, Stores};
    let env = |name: &str| std::env::var(name).ok();
    let choice = StoreChoice::parse(env("DAGQ_CREDENTIAL_STORE").as_deref())?;
    let file = FileCredentials::from_env(env)?;
    then(&Stores {
        choice,
        os: &OsCredentials,
        file: &file,
    })
}

/// `then` with the person's store `DAGQ_CREDENTIAL_STORE` chooses, holding
/// its [`StoreLock::Change`] throughout, so a load, a change and its save
/// are one step against every other holder (another `dagq`, a relay's
/// other request, whatever their `XDG_CONFIG_HOME`).
pub fn with_terminal_store<T>(then: impl FnOnce(&dyn CredentialStore) -> Result<T>) -> Result<T> {
    terminal_store(|store| {
        let _held = store.hold(StoreLock::Change)?;
        then(store)
    })
}

/// The person's store's `lock`, held until what it gives is dropped.
pub fn hold_terminal_store(lock: StoreLock) -> Result<StoreHold> {
    terminal_store(|store| store.hold(lock))
}

/// A store whose each save and delete holds its [`StoreLock::Change`]:
/// `dagq login`'s and `logout`'s, which change the set without reading it
/// first, so the browser's flow is not held under the lock.
pub struct WritesLocked<'a> {
    pub store: &'a dyn CredentialStore,
}
impl CredentialStore for WritesLocked<'_> {
    fn load(&self) -> Result<Option<Credentials>> {
        self.store.load()
    }
    fn save(&self, credentials: &Credentials) -> Result<()> {
        let _held = self.store.hold(StoreLock::Change)?;
        self.store.save(credentials)
    }
    fn delete(&self) -> Result<()> {
        let _held = self.store.hold(StoreLock::Change)?;
        self.store.delete()
    }
    fn hold(&self, lock: StoreLock) -> Result<StoreHold> {
        self.store.hold(lock)
    }
}

/// macOS uses Security.framework directly through security-framework (no
/// command-line secret). Linux secret-service clients require an external
/// D-Bus library or an async runtime, so Linux and other OSes use file storage.
/// No OS call is made by any unit test; the process test explicitly uses file.
pub struct OsCredentials;
#[cfg(target_os = "macos")]
impl CredentialStore for OsCredentials {
    fn load(&self) -> Result<Option<Credentials>> {
        match security_framework::passwords::get_generic_password("dagq.login", "terminal") {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| anyhow::anyhow!("invalid OS credentials")),
            Err(e) if e.code() == -25300 => Ok(None),
            Err(_) => anyhow::bail!("OS credential store unavailable"),
        }
    }
    fn save(&self, credentials: &Credentials) -> Result<()> {
        security_framework::passwords::set_generic_password(
            "dagq.login",
            "terminal",
            &serde_json::to_vec(credentials)?,
        )
        .map_err(|_| anyhow::anyhow!("OS credential store unavailable"))
    }
    fn delete(&self) -> Result<()> {
        match security_framework::passwords::delete_generic_password("dagq.login", "terminal") {
            Ok(()) => Ok(()),
            Err(e) if e.code() == -25300 => Ok(()),
            Err(_) => anyhow::bail!("OS credential store unavailable"),
        }
    }
    /// The Keychain's set is one item of this OS user's, whatever the
    /// environment: the lock is this user's too ([`os_lock_dir`]).
    fn hold(&self, lock: StoreLock) -> Result<StoreHold> {
        let path = os_lock_dir().join(lock_name(lock));
        super::queue_service::prepare_socket_dir(&path)?;
        Ok(Box::new(flock(&path)?))
    }
}
/// Where the OS store's locks are: this OS user's directory of the machine
/// (`/tmp/dagq-<uid>`, mode 0700, the one of the sockets that do not fit
/// under their queue), named by the uid alone, never by `HOME` or
/// `XDG_CONFIG_HOME`, as the OS store's set is one per user.
pub fn os_lock_dir() -> PathBuf {
    super::queue_service::short_socket_dir()
}
#[cfg(not(target_os = "macos"))]
impl CredentialStore for OsCredentials {
    fn load(&self) -> Result<Option<Credentials>> {
        Err(crate::application::login::OsStoreUnavailable.into())
    }
    fn save(&self, _: &Credentials) -> Result<()> {
        Err(crate::application::login::OsStoreUnavailable.into())
    }
    fn delete(&self) -> Result<()> {
        Err(crate::application::login::OsStoreUnavailable.into())
    }
    /// No OS store here keeps a set, so there is nothing to hold.
    fn hold(&self, _: StoreLock) -> Result<StoreHold> {
        Ok(Box::new(()))
    }
}

/// The person's token for their client-mode `dagq` to a control side
/// (`DAGQ_CONTROL_URL`): the credential set of the store
/// `DAGQ_CREDENTIAL_STORE` chooses, read by
/// [`crate::application::login::read_token`] for the URL the call goes to,
/// which refuses a set of another control side (`dagq login`'s guide) and
/// renews once on a refusal. The store is opened only when a call is sent,
/// never by a command refused before it. The command line alone uses it: a
/// runner or a CI gives its own token.
pub struct TerminalBearer;
impl crate::infrastructure::queue_service::BearerSource for TerminalBearer {
    fn token(&self, url: &str, renew: bool) -> Result<String> {
        use crate::application::login::{self, Rejected};
        let rejected = if renew { Rejected::First } else { Rejected::No };
        let now = crate::application::unix_seconds(std::time::SystemTime::now());
        let http = crate::infrastructure::oidc_keys::HttpsDocumentSource::for_login();
        // A renewal saves the set it loaded: under the lock, so it does not
        // bring back a set a `logout` removed meanwhile.
        with_terminal_store(|store| Ok(login::read_token(url, rejected, now, &http, store)?.token))
    }
}

/// The person's token for the relay's parent, of `person` alone: the
/// store's set for the URL is read and renewed as [`TerminalBearer`] does,
/// and refused ([`crate::application::login::PERSON_CHANGED`]) once
/// another person logged in there, so a session never asks or revokes
/// with someone else's token.
pub struct PersonBearer {
    pub person: crate::domain::oidc::OidcIdentity,
}
impl crate::infrastructure::queue_service::BearerSource for PersonBearer {
    fn token(&self, url: &str, renew: bool) -> Result<String> {
        self.token_read(url, renew).map(|(token, _)| token)
    }
    /// Tells a renewal of an expired token at the first read, so the call
    /// renews the person's token once in all
    /// ([`crate::application::login::read_person_token`]).
    fn token_read(&self, url: &str, renew: bool) -> Result<(String, bool)> {
        use crate::application::login::{self, Rejected};
        let rejected = if renew { Rejected::First } else { Rejected::No };
        let now = crate::application::unix_seconds(std::time::SystemTime::now());
        let http = crate::infrastructure::oidc_keys::HttpsDocumentSource::for_login();
        with_terminal_store(|store| {
            login::read_person_token(url, &self.person, rejected, now, &http, store)
        })
    }
}

/// Reject any group/other permission, executable bit or special mode.
pub fn private_file_mode(mode: u32) -> bool {
    mode & 0o7777 == 0o600
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_location_and_permissions_are_private_and_outside_the_checkout() {
        let home =
            FileCredentials::from_env(|name| (name == "HOME").then(|| "/home/person".into()))
                .unwrap();
        assert_eq!(
            home.directory,
            PathBuf::from("/home/person/.config/dagq/credentials")
        );
        let xdg = FileCredentials::from_env(|name| {
            Some(
                if name == "XDG_CONFIG_HOME" {
                    "/personal/config"
                } else {
                    "/ignored"
                }
                .into(),
            )
        })
        .unwrap();
        assert_eq!(
            xdg.directory,
            PathBuf::from("/personal/config/dagq/credentials")
        );
        assert!(FileCredentials::from_env(|_| None).is_err());
        assert!(FileCredentials::from_env(|_| Some("relative".into())).is_err());
        assert!(private_file_mode(0o100600));
        for mode in [0o644, 0o660, 0o400, 0o700, 0o4600] {
            assert!(!private_file_mode(mode));
        }
    }

    /// A set's place: in no Git work tree and below no `runs` directory,
    /// as given or as its links resolve.
    #[test]
    fn a_sets_place_is_outside_work_trees_and_run_directories() {
        let home = Path::new("/home/person/.config/dagq/credentials");
        let run = Path::new("/data/dagq/hash/runs/r1/tmp/dagq/credentials");
        assert!(set_place_allowed(home, home, false));
        assert!(!set_place_allowed(home, home, true));
        assert!(!set_place_allowed(run, run, false));
        // A link from an ordinary path to a run directory, and back.
        assert!(!set_place_allowed(run, home, false));
        assert!(!set_place_allowed(home, run, false));
    }

    /// With the file store, `XDG_CONFIG_HOME` in a Git work tree or a run
    /// directory, or a link to one, is refused before anything is written
    /// or read there, so neither the person's token nor the inbox's ever
    /// lands there; an ordinary place takes the set. The fixture is under
    /// `/tmp`, as a worker's `TMPDIR` is itself in its run directory.
    #[test]
    fn a_set_is_never_kept_in_a_work_tree_or_a_run_directory_even_through_a_link() {
        use crate::application::login::{Destination, Identity, InboxToken};
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let base = root.path().canonicalize().unwrap();
        fs::create_dir_all(base.join("repo/.git")).unwrap();
        fs::create_dir_all(base.join("repo/sub")).unwrap();
        fs::create_dir_all(base.join("queue/runs/r1/worktree")).unwrap();
        fs::create_dir_all(base.join("home")).unwrap();
        std::os::unix::fs::symlink(base.join("repo/sub"), base.join("to-repo")).unwrap();
        std::os::unix::fs::symlink(base.join("queue/runs/r1"), base.join("to-run")).unwrap();
        let set = Credentials {
            destination: Destination {
                control_url: "https://control.example".into(),
                issuer: "https://idp.example".into(),
                client_id: "cli".into(),
            },
            token: "person-secret".into(),
            refresh_token: None,
            identity: Identity {
                issuer: "https://idp.example".into(),
                subject: "a".into(),
                role: "member".into(),
                expires_at: 100,
                display_name: None,
            },
            inbox: Some(InboxToken {
                token: "inbox-secret".into(),
                expires_at: 100,
            }),
        };
        let store = |config: &Path| {
            let config = config.display().to_string();
            FileCredentials::from_env(|name| (name == "XDG_CONFIG_HOME").then(|| config.clone()))
                .unwrap()
        };
        for refused in [
            base.join("repo/sub/config"),
            base.join("to-repo"),
            base.join("queue/runs/r1/worktree"),
            base.join("to-run/config"),
        ] {
            let file = store(&refused);
            let error = file.save(&set).unwrap_err().to_string();
            assert!(
                error.contains("outside Git work trees"),
                "{refused:?}: {error}"
            );
            assert!(!file.directory.exists(), "{refused:?}");
        }
        // A set already there is not read either.
        let planted = base.join("queue/runs/r1/worktree/dagq/credentials");
        fs::create_dir_all(&planted).unwrap();
        fs::write(planted.join("login.json"), b"{}").unwrap();
        assert!(store(&base.join("queue/runs/r1/worktree")).load().is_err());
        let home = store(&base.join("home"));
        home.save(&set).unwrap();
        let kept = home.load().unwrap().unwrap();
        assert_eq!(kept.token, "person-secret");
        assert_eq!(
            kept.inbox.map(|inbox| inbox.token).as_deref(),
            Some("inbox-secret")
        );
    }

    /// Each store holds the lock where its set lives: a file's beside its
    /// directory, so two `XDG_CONFIG_HOME` hold two locks, and the OS's in
    /// this OS user's directory named by the uid alone, the same for every
    /// `XDG_CONFIG_HOME`, as the Keychain's one item is (its real lock is
    /// not taken here: no unit test opens the OS's store).
    #[test]
    fn each_store_holds_its_lock_where_its_set_lives() {
        use std::os::fd::AsRawFd;
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let store = |name: &str| FileCredentials {
            directory: root.path().join(name).join("dagq/credentials"),
        };
        let (first, second) = (store("first"), store("second"));
        let held = first.hold(StoreLock::Issue).unwrap();
        assert!(root.path().join("first/dagq/inbox-issue.lock").is_file());
        // The second config directory's lock is another file, free while
        // the first is held.
        let second_held = second.hold(StoreLock::Issue).unwrap();
        assert!(root.path().join("second/dagq/inbox-issue.lock").is_file());
        // The first's lock is held: another descriptor of its file cannot
        // take it.
        let other = fs::File::open(root.path().join("first/dagq/inbox-issue.lock")).unwrap();
        // SAFETY: a non-blocking flock on a descriptor this test owns.
        let taken = unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_ne!(taken, 0, "the first config directory's lock is held");
        drop((held, second_held));
        let change = first.hold(StoreLock::Change).unwrap();
        assert!(root.path().join("first/dagq/credentials.lock").is_file());
        drop(change);
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        assert_eq!(os_lock_dir(), PathBuf::from(format!("/tmp/dagq-{uid}")));
        assert_ne!(lock_name(StoreLock::Change), lock_name(StoreLock::Issue));
    }
}
