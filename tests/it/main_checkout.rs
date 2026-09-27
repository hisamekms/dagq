//! The main checkout, whose `dagq.toml` dagq reads, is the repository's
//! main worktree (`main_checkout_of`), never the worktree dagq runs in: a
//! repository with a separate Git directory is read from its main worktree
//! and stops elsewhere, since Git does not record where that worktree is;
//! a bare one has no main checkout and stops `up`, `supervise`,
//! `integrate` and `doctor`'s repository with why.
use crate::{common, runtime_support};

use dagq::infrastructure::{
    adapters::{main_checkout_of, naming_checkout},
    run_env::load_run_env_table,
};
use runtime_support::*;

fn configure(repo: &Path) {
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "user.email", "test@example.invalid"]);
}

/// A repository whose Git directory `sep.git` is apart from its main
/// worktree `main` (landing on `trunk`, with `[run.env]`), and a linked
/// worktree `run` whose own `dagq.toml` names other values.
fn separate_git_dir(dir: &Path) -> (PathBuf, PathBuf) {
    let main = dir.join("main");
    let sep = dir.join("sep.git");
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "-q", "-b", "trunk", "--separate-git-dir"])
        .arg(&sep)
        .arg(&main)
        .bounded_output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    configure(&main);
    fs::write(
        main.join("dagq.toml"),
        "[repository]\nbranch = \"trunk\"\n\n[run.env]\nFROM = \"main\"\n",
    )
    .unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "seed"]);
    let run = dir.join("run");
    git(
        &main,
        &["worktree", "add", "-q", "-b", "run", run.to_str().unwrap()],
    );
    fs::write(
        run.join("dagq.toml"),
        "[repository]\nbranch = \"run\"\n\n[run.env]\nFROM = \"run\"\n",
    )
    .unwrap();
    git(&run, &["commit", "-q", "-am", "the run's own settings"]);
    (main.canonicalize().unwrap(), run.canonicalize().unwrap())
}

/// A bare repository cloned from `source`, with a linked worktree whose
/// `dagq.toml` names a branch; the worktree is returned.
fn bare_with_worktree(dir: &Path, source: &Path) -> (PathBuf, PathBuf) {
    let bare = dir.join("bare.git");
    let out = Command::new("git")
        .args(["clone", "-q", "--bare"])
        .arg(source)
        .arg(&bare)
        .bounded_output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let worktree = dir.join("bare worktree");
    git(
        &bare,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "work",
            worktree.to_str().unwrap(),
        ],
    );
    configure(&worktree);
    fs::write(
        worktree.join("dagq.toml"),
        "[repository]\nbranch = \"nope\"\n\n[run.env]\nFROM = \"worktree\"\n",
    )
    .unwrap();
    (
        bare.canonicalize().unwrap(),
        worktree.canonicalize().unwrap(),
    )
}

/// Claude Code trusts only `root`.
fn trust_only(fixture: &common::lifecycle::Fixture, root: &Path) {
    fs::write(
        fixture.environment.claude_config.as_ref().unwrap(),
        json!({"projects": {root.to_str().unwrap(): {"hasTrustDialogAccepted": true}}}).to_string(),
    )
    .unwrap();
}

/// A repository whose Git directory is `.git` in the checkout: its parent
/// from a linked worktree and from the Git directory, as before.
#[test]
fn the_parent_of_a_dot_git_is_the_main_checkout_from_anywhere() {
    let (_dir, repo, _db) = fixture();
    let repo = repo.canonicalize().unwrap();
    let linked = _dir.path().join("linked");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    assert_eq!(main_checkout_of(&linked).unwrap(), repo);
    assert_eq!(main_checkout_of(&repo.join(".git")).unwrap(), repo);
    // Without `core.bare`, Git in the Git directory marks the worktree bare.
    git(&repo, &["config", "--unset", "core.bare"]);
    assert_eq!(main_checkout_of(&repo.join(".git")).unwrap(), repo);
    let inspected = GitRepository::inspect(&linked).unwrap();
    assert_eq!(inspected.checkout().unwrap(), repo);
    assert_eq!(inspected.landing_branch().unwrap().name, "main");
    assert_eq!(
        naming_checkout(Some(&repo.join(".git")), Path::new("/elsewhere")),
        repo
    );
    assert_eq!(
        naming_checkout(None, Path::new("/elsewhere")),
        Path::new("/elsewhere")
    );
}

/// With a separate Git directory, the main worktree reads its own
/// `dagq.toml`; the run's worktree reads none, not even its own, and says
/// why; the Git directory names the repository in a notification.
#[test]
fn a_separate_git_dir_repository_reads_only_its_main_worktree() {
    let _test = common::test();
    let dir = tempfile::tempdir().unwrap();
    let (main, run) = separate_git_dir(dir.path());

    let repository = GitRepository::inspect(&main).unwrap();
    assert_eq!(repository.checkout().unwrap(), main);
    assert_eq!(repository.landing_branch().unwrap().name, "trunk");
    assert_eq!(
        load_run_env_table(repository.checkout().unwrap()).unwrap(),
        Some(vec![("FROM".to_owned(), "main".to_owned())])
    );

    let repository = GitRepository::inspect(&run).unwrap();
    let error = format!("{:#}", repository.checkout().unwrap_err());
    assert!(
        error.contains("--separate-git-dir")
            && error.contains("run the command in the main worktree"),
        "{error}"
    );
    let error = format!("{:#}", repository.landing_branch().unwrap_err());
    assert!(error.contains("--separate-git-dir"), "{error}");
    assert!(!repository.is_dagq_source());

    let sep = dir.path().join("sep.git").canonicalize().unwrap();
    assert!(main_checkout_of(&sep).is_err());
    assert_eq!(naming_checkout(Some(&sep), &run), sep);
}

/// `up` in the main worktree of a separate Git directory checks Claude
/// Code's trust of that worktree and starts; in the run's worktree it
/// stops before starting anything.
#[test]
fn up_in_a_separate_git_dir_repository_trusts_the_main_worktree() {
    use common::lifecycle::{FakeCmux, FakeLaunchd, FakeProcesses, fixture, try_up, up};
    let mut fixture = fixture();
    let (main, run) = separate_git_dir(fixture._dir.path());
    trust_only(&fixture, &main);
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();

    fixture.repo = run;
    let error = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(
        error.contains("--separate-git-dir") && error.contains("the supervisor was not started"),
        "{error}"
    );

    fixture.repo = main;
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report["repository"]["branch"], "trunk", "{report}");
}

/// A bare repository with a worktree: `up` stops with why and what to do
/// before starting a supervisor, `supervise` and `integrate` stop at the
/// start, and `doctor` shows the error, none reading the worktree's
/// `dagq.toml`.
#[test]
fn a_bare_repository_stops_up_supervise_integrate_and_doctor() {
    use common::lifecycle::{FakeCmux, FakeLaunchd, FakeProcesses, fixture, try_up};
    let mut fixture = fixture();
    let (bare, worktree) = bare_with_worktree(fixture._dir.path(), &fixture.repo.clone());
    trust_only(&fixture, &worktree);
    fixture.repo = worktree.clone();
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let error = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(
        error.contains("is bare")
            && error.contains("clone that is not bare")
            && error.contains("the supervisor was not started")
            && !error.contains("nope"),
        "{error}"
    );
    let db = fixture.location.db.clone();
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .supervisors()
            .unwrap()
            .is_empty()
    );

    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let error = format!("{:#}", supervise(&db, &worktree, &backend).unwrap_err());
    assert!(
        error.contains("is bare") && !error.contains("nope"),
        "{error}"
    );
    let error = format!("{:#}", integrate(&db, 1, &worktree).unwrap_err());
    assert!(
        error.contains("is bare") && !error.contains("nope"),
        "{error}"
    );

    SqliteQueue::open(&db)
        .unwrap()
        .bind_repository(bare.to_str().unwrap())
        .unwrap();
    let doctor = runtime::doctor(&db, false).unwrap();
    let error = doctor["repository"]["error"].as_str().unwrap_or_default();
    assert!(error.contains("is bare"), "{doctor}");
    assert_eq!(naming_checkout(Some(&bare), &worktree), bare);
}
