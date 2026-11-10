//! A real Git round trip from a production range's binary copy to an
//! isolated case tree after the run branch is gone.
use crate::plan_review::git;
use crate::runtime_support::git_out;
use dagq::application::role_snapshot::{ReviewChange, rebuild};
use dagq::application::{Repository, RepositoryReads};
use dagq::infrastructure::{adapters::GitRepository, run_files::LocalRunFiles};
use std::fs;

#[test]
fn review_change_copy_rebuilds_the_reviewed_tree() {
    for no_prefix in [false, true] {
        roundtrip_with_prefix_setting(no_prefix);
    }
}

fn roundtrip_with_prefix_setting(no_prefix: bool) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("repo");
    fs::create_dir_all(source.join(".dagq/agents/demo/evals")).unwrap();
    git(&source, &["init", "-q", "--initial-branch=main"]);
    git(&source, &["config", "user.name", "test"]);
    git(&source, &["config", "user.email", "test@localhost"]);
    git(
        &source,
        &[
            "config",
            "diff.noprefix",
            if no_prefix { "true" } else { "false" },
        ],
    );
    fs::write(source.join("changed.txt"), b"old\n").unwrap();
    fs::write(source.join("removed.txt"), b"remove this\n").unwrap();
    fs::write(source.join("binary.bin"), b"\0\xffold\0").unwrap();
    // Shift-JIS text without NUL: Git diffs it as text, so the copy is no
    // UTF-8.
    fs::write(source.join("sjis.txt"), b"\x82\xa0\n").unwrap();
    fs::write(
        source.join(".dagq/agents/demo/evals/dev.json"),
        b"secret expectations",
    )
    .unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "base"]);
    let base = git_out(&source, &["rev-parse", "HEAD"]);
    git(&source, &["switch", "-q", "-c", "run"]);
    let changed = b"the entire changed file\nsecond line\n";
    let added = b"new file in full\n";
    let binary = b"\0\xffnew\0\x80binary\n";
    let sjis = b"\x82\xa2\x82\xa4\n";
    fs::write(source.join("sjis.txt"), sjis).unwrap();
    fs::write(source.join("changed.txt"), changed).unwrap();
    fs::write(source.join("added.txt"), added).unwrap();
    fs::write(source.join("binary.bin"), binary).unwrap();
    fs::remove_file(source.join("removed.txt")).unwrap();
    git(&source, &["add", "--all"]);
    git(&source, &["commit", "-q", "-m", "run"]);
    let head = git_out(&source, &["rev-parse", "HEAD"]);
    let repository = GitRepository::inspect(&source).unwrap();
    let patch = dir.path().join("change.patch");
    repository
        .diff_binary_to_file(&base, &head, &patch)
        .unwrap();
    let bytes = fs::read(&patch).unwrap();
    assert!(std::str::from_utf8(&bytes).is_err());
    let copy = String::from_utf8_lossy(&bytes);
    assert!(copy.contains("GIT binary patch"));
    assert!(copy.contains("diff --git a/changed.txt b/changed.txt"));
    let tree = repository.tree_id(&head).unwrap();
    let change = ReviewChange {
        base,
        head: head.clone(),
        tree_commit: head,
        tree: Some(tree.clone()),
        patch: "change.patch".into(),
    };
    git(&source, &["switch", "-q", "main"]);
    git(&source, &["branch", "-D", "run"]);
    assert!(!repository.reachable_commit(&change.head).unwrap());
    // The copy committed as a case's patch is read back byte for byte.
    fs::create_dir_all(source.join(".dagq/agent-cases/patches")).unwrap();
    fs::write(source.join(".dagq/agent-cases/patches/case.patch"), &bytes).unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "case"]);
    let main = git_out(&source, &["rev-parse", "HEAD"]);
    let read = repository
        .file_bytes_in(&main, ".dagq/agent-cases/patches/case.patch")
        .unwrap()
        .unwrap();
    assert_eq!(read, bytes);
    let patch = dir.path().join("read.patch");
    fs::write(&patch, &read).unwrap();
    let case = dir.path().join("case");
    let full = rebuild(
        &repository,
        &LocalRunFiles,
        &change,
        &patch,
        &case,
        &[".dagq/agents/*/evals", ".dagq/agent-cases"],
    )
    .unwrap()
    .unwrap();
    // The full reconstruction is checked before hiding, per ADR-t3948-1
    // decision 2b. The case's own commits contain no hidden material.
    assert_eq!(repository.tree_id(full.as_str()).unwrap(), tree);
    assert_eq!(fs::read(case.join("added.txt")).unwrap(), added);
    assert_eq!(fs::read(case.join("changed.txt")).unwrap(), changed);
    assert_eq!(fs::read(case.join("binary.bin")).unwrap(), binary);
    assert_eq!(fs::read(case.join("sjis.txt")).unwrap(), sjis);
    assert!(!case.join("removed.txt").exists());
    assert!(!case.join(".dagq/agents/demo/evals").exists());
    assert!(case.join(".git").is_dir());
    assert!(!case.join(".git/objects/info/alternates").exists());
    assert_eq!(
        git_out(&case, &["for-each-ref", "--format=%(refname)"]),
        "refs/heads/main"
    );
    assert_eq!(git_out(&case, &["rev-list", "--all"]).lines().count(), 2);
    assert!(
        !git_out(&case, &["log", "--all", "--root", "-p", "--", ".dagq"])
            .contains("secret expectations")
    );
    assert_ne!(
        git_out(&case, &["rev-parse", "--absolute-git-dir"]),
        git_out(&source, &["rev-parse", "--absolute-git-dir"])
    );
    fs::remove_dir_all(&source).unwrap();
    // Objects and refs remain readable even with the source Git removed.
    assert_eq!(
        git_out(&case, &["show", "HEAD:changed.txt"]),
        std::str::from_utf8(changed).unwrap().trim_end()
    );
    git(&case, &["fsck", "--full"]);
}
