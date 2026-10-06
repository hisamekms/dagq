//! The templates of `tests/common/template.rs` made again after a cache's
//! cleanup emptied them, and the check that a directory is a repository.
use crate::common::{Bounded, template};
use dagq::infrastructure::git_binary::git_executable;
use rusqlite::Connection;
use std::{fs, path::Path, process::Command};

/// What rust-cache's cleanTargetDir leaves: every directory, no file.
fn delete_files(dir: &Path) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            delete_files(&entry.path());
        } else {
            fs::remove_file(entry.path()).unwrap();
        }
    }
}

fn templates(root: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_dir())
        .map(|entry| entry.file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn use_each(root: &Path, fixture: &Path) {
    template::with_root(root, || {
        template::script(&fixture.join("stub"), "#!/bin/sh\necho stubbed\n");
        fs::create_dir(fixture.join("repo")).unwrap();
        template::repository(&fixture.join("repo"), "seed of the cleanup test\n");
        drop(template::queue(&fixture.join("queue.db")));
    });
    let stub = Command::new(fixture.join("stub")).bounded_output().unwrap();
    assert!(stub.status.success(), "{stub:?}");
    assert_eq!(String::from_utf8(stub.stdout).unwrap(), "stubbed\n");
    let log = Command::new(git_executable().unwrap())
        .arg("-C")
        .arg(fixture.join("repo"))
        .args(["log", "--format=%s %an"])
        .bounded_output()
        .unwrap();
    assert!(log.status.success(), "{log:?}");
    assert_eq!(String::from_utf8(log.stdout).unwrap(), "seed test\n");
    assert_eq!(
        fs::read_to_string(fixture.join("repo/seed.txt")).unwrap(),
        "seed of the cleanup test\n"
    );
    let conn = Connection::open(fixture.join("queue.db")).unwrap();
    let tables: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(tables > 0, "the queue copy has no table");
}

#[test]
fn templates_emptied_by_a_cache_cleanup_are_made_again() {
    let root = tempfile::tempdir().unwrap();
    let fixtures = tempfile::tempdir().unwrap();
    let first = fixtures.path().join("first");
    fs::create_dir(&first).unwrap();
    use_each(root.path(), &first);
    let made = templates(root.path());
    assert_eq!(made.len(), 3, "{made:?}");
    assert!(made.iter().any(|name| name.starts_with("script-")));
    assert!(made.iter().any(|name| name.starts_with("repo-")));
    assert!(made.iter().any(|name| name.starts_with("queue-")));

    delete_files(root.path());
    // The cleanup's shape: the repository template's .git/ is left empty.
    let repo = made.iter().find(|name| name.starts_with("repo-")).unwrap();
    assert!(root.path().join(repo).join("repo/.git/objects").is_dir());
    assert!(template::git_repository(&root.path().join(repo).join("repo")).is_err());

    let second = fixtures.path().join("second");
    fs::create_dir(&second).unwrap();
    use_each(root.path(), &second);
    assert_eq!(templates(root.path()), made);
}

#[test]
fn the_repository_check_names_the_dir_the_git_and_its_stderr() {
    let git = git_executable().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let hollow = tempfile::tempdir().unwrap();
    fs::create_dir_all(hollow.path().join(".git/objects")).unwrap();
    // A hollow repository inside another, as the templates sit in the checkout.
    let outer = tempfile::tempdir().unwrap();
    template::repository(outer.path(), "seed of the outer repository\n");
    let nested = outer.path().join("nested");
    fs::create_dir_all(nested.join(".git/objects")).unwrap();
    for dir in [empty.path(), hollow.path(), nested.as_path()] {
        let message = template::git_repository(dir).unwrap_err();
        assert!(message.contains(&dir.display().to_string()), "{message}");
        assert!(message.contains(&git.display().to_string()), "{message}");
        // Only git's stderr gives this: the message's own words do not.
        assert!(message.contains("git stderr: fatal: "), "{message}");
    }

    let repo = tempfile::tempdir().unwrap();
    template::repository(repo.path(), "seed of the check test\n");
    template::git_repository(repo.path()).unwrap();
}
