//! The Git files a development build's script watches (`dagq::build_id`):
//! each is there, so cargo does not rerun the script on every command, and
//! a commit, a checkout and staging change one of them, so the build
//! identifier follows.

use crate::common::{self, Bounded};
use dagq::build_id::{SOURCES, compute};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .arg("-C")
        .arg(dir)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit(root: &Path, contents: &str) {
    std::fs::write(root.join("Cargo.toml"), contents).unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-q", "-m", contents]);
}

/// A fresh repository on `main` with one commit, without `packed-refs`.
fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    run_git(dir.path(), &["init", "-q", "-b", "main"]);
    commit(dir.path(), "first");
    dir
}

/// The Git metadata the build of `root` watches: its `rerun-if-changed`
/// paths but build.rs and [`SOURCES`].
fn git_watched(root: &Path) -> Vec<PathBuf> {
    let build = compute("dagq", "0.4.0-dev", root);
    assert!(
        !build.identifier.ends_with("+unknown"),
        "{}",
        build.identifier
    );
    let sources: Vec<PathBuf> = SOURCES.iter().map(|path| root.join(path)).collect();
    build
        .directives
        .iter()
        .filter_map(|line| line.strip_prefix("cargo:rerun-if-changed="))
        .map(PathBuf::from)
        .filter(|path| path != Path::new("build.rs") && !sources.contains(path))
        .collect()
}

fn assert_all_files(root: &Path) -> Vec<PathBuf> {
    let watched = git_watched(root);
    assert!(!watched.is_empty());
    for path in &watched {
        assert!(path.is_file(), "{} is watched but missing", path.display());
    }
    watched
}

type Snapshot = Vec<(PathBuf, Vec<u8>, SystemTime)>;

fn snapshot(paths: &[PathBuf]) -> Snapshot {
    paths
        .iter()
        .map(|path| {
            let contents = std::fs::read(path).unwrap();
            let modified = std::fs::metadata(path).unwrap().modified().unwrap();
            (path.clone(), contents, modified)
        })
        .collect()
}

/// Whether a file of `before` has other contents or another mtime now (or
/// is gone, which cargo takes as changed too).
fn changed(before: &Snapshot) -> bool {
    before.iter().any(|(path, contents, modified)| {
        match (std::fs::read(path), std::fs::metadata(path)) {
            (Ok(now), Ok(metadata)) => {
                now != *contents || metadata.modified().unwrap() != *modified
            }
            _ => true,
        }
    })
}

/// Each of a commit, a checkout of another commit and staging changes a
/// watched file of the state before it.
fn assert_follows_commit_checkout_and_staging(root: &Path) {
    let before = snapshot(&assert_all_files(root));
    let first = run_git(root, &["rev-parse", "HEAD"]);
    commit(root, "second");
    assert!(changed(&before), "a commit changed no watched file");

    let before = snapshot(&assert_all_files(root));
    run_git(root, &["checkout", "-q", &first]);
    assert!(changed(&before), "a checkout changed no watched file");

    let before = snapshot(&assert_all_files(root));
    std::fs::write(root.join("Cargo.toml"), "staged").unwrap();
    run_git(root, &["add", "Cargo.toml"]);
    assert!(changed(&before), "staging changed no watched file");
    assert_all_files(root);
}

#[test]
fn a_fresh_repository_without_packed_refs_watches_only_files_that_are_there_and_follows() {
    let _waiting = common::test();
    let dir = repository();
    let root = dir.path();
    assert!(!root.join(".git/packed-refs").exists());
    assert_all_files(root);
    assert_follows_commit_checkout_and_staging(root);
}

#[test]
fn a_branch_only_in_packed_refs_watches_only_files_that_are_there_and_follows() {
    let _waiting = common::test();
    let dir = repository();
    let root = dir.path();
    run_git(root, &["pack-refs", "--all"]);
    assert!(!root.join(".git/refs/heads/main").exists());
    assert!(root.join(".git/packed-refs").is_file());
    let watched = assert_all_files(root);
    let reflog = root.join(".git/logs/HEAD");
    assert!(watched.contains(&reflog), "{watched:?}");
    // A commit that leaves the index alone writes the loose ref that was not
    // there to be watched, and the reflog is what tells.
    let before = snapshot(&[reflog]);
    run_git(root, &["commit", "-q", "--allow-empty", "-m", "empty"]);
    assert!(changed(&before), "an empty commit left the reflog alone");
    run_git(root, &["pack-refs", "--all"]);
    assert_follows_commit_checkout_and_staging(root);
}
