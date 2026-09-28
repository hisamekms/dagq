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

use std::{
    fs,
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
    process::Command,
};

use dagq::infrastructure::{schema::MIGRATIONS, sqlite::SqliteQueue};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::Bounded;

/// Raised when what makes a template changes other than the migrations and
/// the seed (the steps below, `init` itself), so no old template is used.
const FORMAT: u32 = 1;

/// A migrated queue at `db`, copied from the template and opened as
/// `SqliteQueue::init` opens an existing queue.
pub fn queue(db: &Path) -> SqliteQueue {
    let mut migrations = Sha256::new();
    for migration in MIGRATIONS {
        migrations.update(migration.as_bytes());
        migrations.update([0]);
    }
    let name = format!(
        "queue-f{FORMAT}-v{}-{}.db",
        SqliteQueue::SCHEMA_VERSION,
        short(&migrations.finalize())
    );
    let template = made(&name, |building| {
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
    fs::copy(template.join("queue.db"), db).unwrap();
    SqliteQueue::init(db).unwrap()
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
    });
    copy_tree(&template.join("repo"), repo);
}

fn short(digest: &[u8]) -> String {
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// The directory of the template `name`, made by `make` in an empty
/// directory by the one process that holds the template's lock, and renamed
/// into place whole, so no process sees a template half made.
fn made(name: &str, make: impl FnOnce(&Path)) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixture-templates");
    let template = root.join(name);
    if template.exists() {
        return template;
    }
    fs::create_dir_all(&root).unwrap();
    let lock = fs::File::create(root.join(format!("{name}.lock"))).unwrap();
    let _waiting = super::within(super::STEP_LIMIT, "the lock of a fixture template");
    // SAFETY: flock(2) takes the descriptor of a file this function holds
    // open; closing it at return releases the lock.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    if !template.exists() {
        // Left by a process that died making it; the lock says none is now.
        let building = root.join(format!("{name}.building"));
        if building.exists() {
            fs::remove_dir_all(&building).unwrap();
        }
        fs::create_dir(&building).unwrap();
        make(&building);
        fs::rename(&building, &template).unwrap();
    }
    template
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
    let result = Command::new("git")
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
