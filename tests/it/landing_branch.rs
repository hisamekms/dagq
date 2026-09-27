//! The landing branch (ADR-t615-1): a repository whose default branch is
//! `master` and has no origin claims, lands, reports stats and opens
//! planners on `master`; `[repository] branch` of dagq.toml names another
//! one; and one that resolves none stops `supervise` and `up` with a hint,
//! while `doctor` shows what resolved. The `main` repositories of the other
//! tests keep landing on `main`.
use crate::{common, runtime_support};

use runtime_support::*;

/// The runtime fixture with its only branch renamed from `main` to `to`.
fn renamed(to: &str) -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    git(&repo, &["branch", "-m", "main", to]);
    (dir, repo, db)
}

fn has_branch(repo: &Path, name: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ])
        .bounded_output()
        .unwrap()
        .status
        .success()
}

/// Claimed from `master`, rebased onto it, landed on it and never pushed
/// (no origin); a second run that conflicts with the first landing is
/// counted by `stats`, whose history of the landing branch is read; and
/// `doctor` reports the branch and where its name came from.
#[test]
fn a_master_repository_without_origin_lands_on_master() {
    let (_dir, repo, db) = renamed("master");
    let seed = git_out(&repo, &["rev-parse", "master"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let first = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(first.base_commit().as_str(), seed);

    let landed = integrate(&db, 1, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_eq!(landed["push"]["outcome"], "skipped", "{landed}");
    assert_eq!(landed["push"]["branch"], "master", "{landed}");
    assert_eq!(
        landed["push"]["reason"],
        "the repository has no remote origin"
    );
    let head = git_out(&repo, &["rev-parse", "master"]);
    assert_ne!(head, seed);
    assert_eq!(
        git_out(&repo, &["rev-parse", &format!("{head}^")]),
        seed,
        "one squash commit on master"
    );
    assert!(!has_branch(&repo, "main"));

    // Both runs rewrote the same file: the second's rebase onto master
    // conflicts, and its reason names the branch.
    let parked = integrate(&db, 2, &repo).unwrap();
    assert_eq!(parked["outcome"], "needs_session", "{parked}");
    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    let reason = run.last_error().unwrap_or_default().to_owned();
    assert!(reason.contains("rebase onto master "), "{reason}");

    let stats = runtime::stats(&db, &Default::default()).unwrap();
    let history = &stats["conflict_hotspots"]["history"];
    assert_eq!(history["status"], "checked", "{stats}");

    let doctor = runtime::doctor(&db, false).unwrap();
    assert_eq!(
        doctor["repository"],
        json!({
            "branch": "master", "branch_source": "master", "remote": "origin",
            "remote_source": "default", "remote_exists": false, "push": true,
        }),
        "{doctor}"
    );
}

/// No main, no master and no origin: `supervise` stops with the hint and
/// `doctor` shows why; `[repository] branch` of dagq.toml names the branch,
/// and the run lands on it.
#[test]
fn a_branch_named_in_dagq_toml_is_where_runs_land() {
    let (_dir, repo, db) = renamed("trunk");
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let error = format!("{:#}", supervise(&db, &repo, &backend).unwrap_err());
    assert!(
        error.contains("cannot resolve the landing branch") && error.contains("[repository]"),
        "{error}"
    );

    fs::write(repo.join("dagq.toml"), "[repository]\nbranch = \"nope\"\n").unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "name no branch"]);
    let error = format!("{:#}", supervise(&db, &repo, &backend).unwrap_err());
    assert!(error.contains("nope"), "{error}");

    fs::write(repo.join("dagq.toml"), "[repository]\nbranch = \"trunk\"\n").unwrap();
    git(&repo, &["commit", "-am", "name the landing branch"]);
    let base = git_out(&repo, &["rev-parse", "trunk"]);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let landed = integrate(&db, 1, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_eq!(
        git_out(&repo, &["rev-parse", "trunk^"]),
        base,
        "landed on trunk"
    );
    assert_eq!(
        runtime::doctor(&db, true).unwrap()["repository"],
        json!({
            "branch": "trunk", "branch_source": "config", "remote": "origin",
            "remote_source": "default", "remote_exists": false, "push": true,
        })
    );

    fs::write(
        repo.join("dagq.toml"),
        "[repository]\nbranch = 'refs/heads/trunk'\n",
    )
    .unwrap();
    let doctor = runtime::doctor(&db, false).unwrap();
    let error = doctor["repository"]["error"].as_str().unwrap();
    assert!(error.contains("without refs/heads/"), "{doctor}");

    // A configured push remote that is missing is an error too.
    fs::write(
        repo.join("dagq.toml"),
        "[repository]\nbranch = 'trunk'\nremote = 'upstream'\n",
    )
    .unwrap();
    let doctor = runtime::doctor(&db, false).unwrap();
    let error = doctor["repository"]["error"].as_str().unwrap();
    assert!(error.contains("the remote upstream"), "{doctor}");
    assert_eq!(doctor["repository"]["branch"], "trunk", "{doctor}");
    assert_eq!(doctor["repository"]["remote"], "upstream", "{doctor}");
    assert_eq!(doctor["repository"]["remote_exists"], false, "{doctor}");
}

/// `up` reports the landing branch of a master repository and refuses,
/// before starting a supervisor, one that resolves none; `plan` opens a
/// planner in the master repository.
#[test]
fn up_checks_the_landing_branch_and_plan_opens_on_master() {
    use common::lifecycle::{FakeCmux, FakeLaunchd, FakeProcesses, fixture, try_up, up};
    let fixture = fixture();
    git(&fixture.repo, &["branch", "-m", "main", "master"]);
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(
        report["repository"],
        json!({
            "branch": "master", "branch_source": "master", "remote": "origin",
            "remote_source": "default", "remote_exists": false, "push": true,
        })
    );

    let runner = fixture._dir.path().join("dagq-binary");
    fs::write(&runner, "#!/bin/sh\n").unwrap();
    let options = dagq::lifecycle::PlanOptions {
        claude: fixture.options.claude.clone(),
        plugin_dir: fixture.options.plugin_dir.clone(),
        runner,
    };
    let planned = dagq::lifecycle::plan(&fixture.location, &fixture.repo, &cmux, &options).unwrap();
    assert_eq!(planned["planner"]["id"], 1, "{planned}");

    let other = common::lifecycle::fixture();
    git(&other.repo, &["branch", "-m", "main", "feature"]);
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&other.location.db);
    let error = format!(
        "{:#}",
        try_up(&other, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(
        error.contains("cannot resolve the landing branch")
            && error.contains("[repository]")
            && error.contains("the supervisor was not started"),
        "{error}"
    );
    assert!(
        SqliteQueue::open(&other.location.db)
            .unwrap()
            .supervisors()
            .unwrap()
            .is_empty()
    );
}

/// `up` refuses, before starting a supervisor, a repository whose
/// `[repository] remote` names a remote it does not have, unless `push =
/// false`, or is not a remote name; it reports the configured remote once
/// it exists, whose HEAD names the guessed landing branch.
#[test]
fn up_and_doctor_check_the_configured_push_remote() {
    use common::lifecycle::{FakeCmux, FakeLaunchd, FakeProcesses, fixture, try_up, up};
    let fixture = fixture();
    let config = fixture.repo.join("dagq.toml");
    fs::write(&config, "[repository]\nremote = \"upstream\"\n").unwrap();
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let error = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(
        error.contains("the remote upstream")
            && error.contains("[repository]")
            && error.contains("the supervisor was not started"),
        "{error}"
    );
    fs::write(
        &config,
        "[repository]\nremote = \"upstream\"\npush = false\n",
    )
    .unwrap();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(
        report["repository"],
        json!({
            "branch": "main", "branch_source": "main", "remote": "upstream",
            "remote_source": "config", "remote_exists": false, "push": false,
        })
    );

    // With the remote and its HEAD on `trunk`, the guess follows it.
    fs::write(&config, "[repository]\nremote = \"upstream\"\n").unwrap();
    git(&fixture.repo, &["branch", "trunk"]);
    git(
        &fixture.repo,
        &["remote", "add", "upstream", "/nonexistent/upstream.git"],
    );
    git(
        &fixture.repo,
        &[
            "symbolic-ref",
            "refs/remotes/upstream/HEAD",
            "refs/remotes/upstream/trunk",
        ],
    );
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(
        report["repository"],
        json!({
            "branch": "trunk", "branch_source": "remote_head", "remote": "upstream",
            "remote_source": "config", "remote_exists": true, "push": true,
        }),
        "{report}"
    );

    fs::write(&config, "[repository]\nremote = \"bad name\"\n").unwrap();
    let error = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(error.contains("not a valid remote name"), "{error}");
}

/// A run whose commit rewrites `[repository] branch` of dagq.toml: the
/// landing resolves its branch once, so the fast-forward of the checkout
/// (which rewrites the checkout's dagq.toml) and the push both go to the
/// branch the landing began on, not the one the landed commit names.
#[test]
fn a_landing_that_renames_the_branch_still_lands_and_pushes_on_its_branch() {
    let (dir, repo, db) = renamed("trunk");
    fs::write(repo.join("dagq.toml"), "[repository]\nbranch = \"trunk\"\n").unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "name the landing branch"]);
    git(&repo, &["branch", "other"]);
    let origin = dir.path().join("origin.git");
    git(&repo, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&repo, &["push", "-q", "origin", "trunk", "other"]);
    let other = git_out(&repo, &["rev-parse", "other"]);
    let base = git_out(&repo, &["rev-parse", "trunk"]);

    let backend = TestWorkspace::new(
        &db,
        false,
        "printf '[repository]\\nbranch = \"other\"\\n' > dagq.toml && git add dagq.toml && git commit -q -m rename; receipt \"$(git rev-parse HEAD)\"",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let landed = integrate(&db, 1, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_eq!(landed["push"]["outcome"], "pushed", "{landed}");
    assert_eq!(landed["push"]["branch"], "trunk", "{landed}");
    let head = git_out(&repo, &["rev-parse", "trunk"]);
    assert_eq!(
        git_out(&repo, &["rev-parse", "trunk^"]),
        base,
        "landed on trunk"
    );
    assert_eq!(
        fs::read_to_string(repo.join("dagq.toml")).unwrap(),
        "[repository]\nbranch = \"other\"\n",
        "the checkout of trunk moved with the landing"
    );
    assert_eq!(git_out(&repo, &["rev-parse", "other"]), other);
    assert_eq!(git_out(&origin, &["rev-parse", "refs/heads/trunk"]), head);
    assert_eq!(git_out(&origin, &["rev-parse", "refs/heads/other"]), other);
}
