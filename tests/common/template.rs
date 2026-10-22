//! Templates of the fixtures' queue and repository (task 1044): a queue at
//! the latest schema and a repository with its seed commit, each made once
//! on disk and copied into every fixture, instead of every fixture applying
//! all the migrations and running `git` five times.
//!
//! nextest runs every test in a process of its own, so a template is shared
//! through the disk, not the process: it lives under the target's temporary
//! directory, named after what it was made from (the schema version and a
//! hash of the migrations, or the seed file's content), and the first
//! process that needs it makes it under a file lock and renames it into
//! place. A changed migration names another template, made afresh. The
//! queue is copied as a single file: its WAL is checkpointed and closed
//! first. Tests of the migrations themselves migrate as before.
//!
//! A template is complete only when it holds [`COMPLETE`], written last:
//! CI's rust-cache deletes every file under the target's temporary
//! directory but keeps the directories, and an empty template judged by its
//! directory alone broke main's CI from 2026-09-29.
//!
//! The pitfalls of the templates:
//! - A directory is not a template. The cache's cleanup left an empty
//!   `.git/` or a directory without `stub`, which failed later as a
//!   `hard_link` NotFound or git's "not a git repository", far from the
//!   cause. Judge by the mark ([`made`]), never by `exists()`.
//! - A bad cache stays: rust-cache does not save when its key hits, so a
//!   job that fails on an empty template does not overwrite it, and every
//!   run with the same key restores it again. Making a template right
//!   whatever the cache holds is this module's part; keeping the target's
//!   temporary directory out of the cache is the CI workflow's.
//! - The templates live inside the checkout (`target/tmp`), so a git that
//!   searches above a directory finds this repository and passes a broken
//!   template: [`git_repository`] checks the directory itself, right after
//!   making the repository template and again on each copy.
//! - A test that breaks templates does it under its own root
//!   ([`with_root`]), never the shared one other processes are reading.
//! - A change of how a template is made, other than the migrations and the
//!   seed, raises [`FORMAT`], so an old template is not taken by its name.
//!
//! Templates do not pile up (a run's worktree once held 1720, too many for
//! the cleanup's measure): each one made prunes the others of an old
//! [`FORMAT`] or schema, the incomplete ones and those left by a process
//! that died making them, and beyond [`KEPT`] the least recently used. A
//! stub whose content holds a fixture's temporary path names a template of
//! its own on every run, so the bound is a count, not a list of keys. A
//! process reads a template only under its lock, shared, until its copy is
//! taken, and prunes one only under the same lock, exclusive and without
//! waiting, so it never deletes a template in use or being made.

use dagq::infrastructure::git_binary::git_executable;
use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs,
    os::unix::{fs::MetadataExt, io::AsRawFd},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use dagq::infrastructure::{schema::MIGRATIONS, sqlite::SqliteQueue};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::Bounded;

/// Raised when what makes a template changes other than the migrations and
/// the seed (the steps below, `init` itself), so no old template is used.
pub const FORMAT: u32 = 2;

/// The most complete templates of the current [`FORMAT`] kept under a root
/// when one is made: a full run of the tests uses a few dozen.
pub const KEPT: usize = 128;

/// The kinds of template, the first part of their names.
const KINDS: [&str; 3] = ["script", "repo", "queue"];

/// The file `made` writes last in a template, after `make`: a template
/// without it is incomplete, whatever else it holds.
const COMPLETE: &str = ".complete";

thread_local! {
    /// The directory of the templates in place of the target's, set by
    /// [`with_root`] for the tests of the templates themselves.
    static ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Runs `body` with the templates made and read under `root` on this
/// thread, so a test can break its own templates and not the shared ones.
pub fn with_root<T>(root: &Path, body: impl FnOnce() -> T) -> T {
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ROOT.with(|cell| *cell.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(ROOT.with(|cell| cell.borrow_mut().replace(root.to_path_buf())));
    body()
}

/// A migrated queue at `db`, copied from the template and opened as
/// `SqliteQueue::init` opens an existing queue.
pub fn queue(db: &Path) -> SqliteQueue {
    let template = made(&queue_name(), |building| {
        let db = building.join("queue.db");
        drop(SqliteQueue::init(&db).unwrap());
        let conn = Connection::open(&db).unwrap();
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
        drop(conn);
        for side in ["queue.db-wal", "queue.db-shm"] {
            assert!(!building.join(side).exists(), "{side} left after close");
        }
    });
    fs::copy(template.dir.join("queue.db"), db).unwrap();
    drop(template);
    SqliteQueue::init(db).unwrap()
}

/// The name of the queue template of these migrations: any other queue
/// template is of another schema.
fn queue_name() -> String {
    let mut migrations = Sha256::new();
    for migration in MIGRATIONS {
        migrations.update(migration.as_bytes());
        migrations.update([0]);
    }
    format!(
        "queue-f{FORMAT}-v{}-{}.db",
        SqliteQueue::SCHEMA_VERSION,
        short(&migrations.finalize())
    )
}

/// Fills the empty directory `repo` with a repository on `main` whose one
/// commit, `seed`, adds `seed.txt` holding `seed`, by `test`
/// <test@example.invalid>, who is also the repository's configured user.
/// Every copy of one seed has the same commit hash.
pub fn repository(repo: &Path, seed: &str) {
    let name = format!("repo-f{FORMAT}-{}", short(&Sha256::digest(seed.as_bytes())));
    let template = made(&name, |building| {
        let repo = building.join("repo");
        fs::create_dir(&repo).unwrap();
        // No template: the sample hooks are most of what a copy would copy.
        git(&repo, &["init", "-q", "--template=", "-b", "main"]);
        git(&repo, &["config", "user.name", "test"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        fs::write(repo.join("seed.txt"), seed).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
        // Before the mark, so a repository git cannot read is never complete.
        if let Err(message) = git_repository(&repo) {
            panic!("the repository template: {message}");
        }
    });
    copy_tree(&template.dir.join("repo"), repo);
    drop(template);
    if let Err(message) = git_repository(repo) {
        panic!("the copy of the repository template: {message}");
    }
}

/// Whether the git that `git_executable` resolves takes `dir` itself for a
/// repository (`rev-parse --absolute-git-dir`, not searching above `dir`:
/// the templates live inside the checkout); the error names the dir, that
/// git and its stderr.
pub fn git_repository(dir: &Path) -> Result<(), String> {
    let git = git_executable().expect("git executable");
    let canonical = dir
        .canonicalize()
        .map_err(|error| format!("{}: {error}", dir.display()))?;
    let result = Command::new(&git)
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--absolute-git-dir"])
        .env(
            "GIT_CEILING_DIRECTORIES",
            canonical.parent().unwrap_or(&canonical),
        )
        .bounded_output()
        .unwrap();
    let found = String::from_utf8_lossy(&result.stdout).trim().to_string();
    let found = Path::new(&found).canonicalize().ok();
    if result.status.success() && found.as_deref() == Some(canonical.join(".git").as_path()) {
        return Ok(());
    }
    Err(format!(
        "{} is not a git repository to {} ({}, git dir {:?}), git stderr: {}",
        dir.display(),
        git.display(),
        result.status,
        found,
        String::from_utf8_lossy(&result.stderr).trim()
    ))
}

/// Install an immutable, already executed shell stub. Hardlinks preserve the
/// fixture path through canonicalize (symlinks would move `$0` and sidecars to
/// the cache). Call this again to replace a stub; never write through its link.
/// The content key and `made` lock share the inode across nextest processes too.
pub fn script(path: &Path, script: impl AsRef<str>) {
    use std::os::unix::fs::PermissionsExt;
    let script = script.as_ref();
    let (shebang, body) = script.split_once('\n').expect("stub shebang");
    assert!(shebang.starts_with("#!/bin/sh"), "shell stub: {shebang}");
    let content =
        format!("{shebang}\nif [ \"${{DAGQ_TEST_STUB_WARMUP:-}}\" = 1 ]; then exit 0; fi\n{body}");
    let name = format!("script-f{FORMAT}-{:x}", Sha256::digest(content.as_bytes()));
    let template = made(&name, |building| {
        let stub = building.join("stub");
        fs::write(&stub, &content).unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o555)).unwrap();
        assert!(
            Command::new(&stub)
                .env("DAGQ_TEST_STUB_WARMUP", "1")
                .bounded_status()
                .unwrap()
                .success()
        );
    });
    // Publish replacements atomically, without modifying a shared inode.
    let link = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
    let link = link.into_temp_path();
    fs::remove_file(&link).unwrap();
    fs::hard_link(template.dir.join("stub"), &link).unwrap();
    drop(template);
    fs::rename(&link, path).unwrap();
}

/// Per-fixture shell environment, sourced after the warmup guard. Values are
/// quoted as data, and are not put in the test process's global environment.
pub fn script_env(path: &Path, body: impl AsRef<str>, env: &[(&str, &str)]) {
    let mut values = String::new();
    for (key, value) in env {
        assert!(key.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'));
        values.push_str(&format!("{key}='{}'\n", value.replace('\'', "'\\''")));
    }
    let sidecar = path.with_file_name(format!(
        "{}.env",
        path.file_name().unwrap().to_str().unwrap()
    ));
    fs::write(sidecar, values).unwrap();
    let (shebang, body) = body.as_ref().split_once('\n').unwrap();
    script(path, format!("{shebang}\n. \"$0.env\"\n{body}"));
}

fn short(digest: &[u8]) -> String {
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// A complete template, its lock held (shared, or exclusive by the process
/// that made it) until the copy is taken from it and this is dropped.
struct Template {
    dir: PathBuf,
    _lock: fs::File,
}

/// The template `name`, made by `make` in an empty directory by the one
/// process that holds the template's lock, marked complete last, and
/// renamed into place whole, so no process sees a template half made. A
/// template without the mark (its files deleted by a cache's cleanup) is
/// deleted and made again. Making one prunes the others ([`prune`]).
fn made(name: &str, make: impl FnOnce(&Path)) -> Template {
    let root = ROOT
        .with(|cell| cell.borrow().clone())
        .unwrap_or_else(|| Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixture-templates"));
    let template = root.join(name);
    fs::create_dir_all(&root).unwrap();
    let _waiting = super::within(super::STEP_LIMIT, "the lock of a fixture template");
    let shared = lock(&root, name, libc::LOCK_SH).unwrap();
    if template.join(COMPLETE).exists() {
        return used(template, shared);
    }
    drop(shared);
    let lock = lock(&root, name, libc::LOCK_EX).unwrap();
    if !template.join(COMPLETE).exists() {
        // No process reads a template without the mark: it is safe to drop.
        if template.exists() {
            fs::remove_dir_all(&template).unwrap();
        }
        // Left by a process that died making it; the lock says none is now.
        let building = root.join(format!("{name}.building"));
        if building.exists() {
            fs::remove_dir_all(&building).unwrap();
        }
        fs::create_dir(&building).unwrap();
        make(&building);
        fs::write(building.join(COMPLETE), "").unwrap();
        fs::rename(&building, &template).unwrap();
        prune(&root, name);
    }
    used(template, lock)
}

/// Marks the template used now (its lock's mtime, which [`prune`] orders
/// by) and hands it out with its lock.
fn used(dir: PathBuf, lock: fs::File) -> Template {
    lock.set_modified(SystemTime::now()).unwrap();
    Template { dir, _lock: lock }
}

/// The lock of the template `name` under `root`, taken by `flock(2)` with
/// `operation`; `None` when it has `LOCK_NB` and another process holds the
/// lock. [`prune`] deletes the lock file it holds, so a lock taken on a
/// file no longer at the path is let go and taken again on the new one.
fn lock(root: &Path, name: &str, operation: libc::c_int) -> Option<fs::File> {
    let path = root.join(format!("{name}.lock"));
    loop {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        // SAFETY: flock(2) takes the descriptor of a file this function
        // holds open; closing it releases the lock.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EWOULDBLOCK) if operation & libc::LOCK_NB != 0 => return None,
                _ => panic!("the lock {}: {error}", path.display()),
            }
        }
        let held = file.metadata().unwrap();
        if let Ok(at_path) = fs::metadata(&path)
            && (at_path.dev(), at_path.ino()) == (held.dev(), held.ino())
        {
            return Some(file);
        }
    }
}

/// Deletes, besides `made`, every template under `root` that is of another
/// [`FORMAT`] or (a queue) of other migrations, that is not complete, or
/// that is beyond the [`KEPT`] most recently used, with what it left (its
/// `.building` and its lock). One whose lock another process holds is in
/// use or being made, and stays.
fn prune(root: &Path, made: &str) {
    let queue = queue_name();
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap().file_name();
        let Some(entry) = entry.to_str() else {
            continue;
        };
        let name = entry
            .strip_suffix(".lock")
            .or_else(|| entry.strip_suffix(".building"))
            .unwrap_or(entry);
        if name != made
            && KINDS
                .iter()
                .any(|kind| name.starts_with(&format!("{kind}-")))
        {
            names.insert(name.to_owned());
        }
    }
    let mut current = Vec::new();
    for name in names {
        let of_this_format = KINDS
            .iter()
            .any(|kind| name.starts_with(&format!("{kind}-f{FORMAT}-")));
        let stale = !of_this_format
            || (name.starts_with("queue-") && name != queue)
            || !root.join(&name).join(COMPLETE).exists();
        if stale {
            delete(root, &name);
        } else {
            let used = fs::metadata(root.join(format!("{name}.lock")))
                .and_then(|lock| lock.modified())
                .unwrap_or(UNIX_EPOCH);
            current.push((used, name));
        }
    }
    current.sort_by(|a, b| b.cmp(a));
    for (_, name) in current.iter().skip(KEPT.saturating_sub(1)) {
        delete(root, name);
    }
}

/// Deletes the template `name` and what it left, unless another process
/// holds its lock.
fn delete(root: &Path, name: &str) {
    let Some(lock) = lock(root, name, libc::LOCK_EX | libc::LOCK_NB) else {
        return;
    };
    for dir in [root.join(name), root.join(format!("{name}.building"))] {
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
    }
    // Last, while held: a process waiting on this file takes it again anew.
    fs::remove_file(root.join(format!("{name}.lock"))).unwrap();
    drop(lock);
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn git(repo: &Path, args: &[&str]) {
    let result = Command::new(git_executable().expect("git executable"))
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
}
