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
        user_config: None,
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

/// `integrate --no-push` pushes nothing and looks at no remote, but records
/// the remote `[repository]` names (the one `push = false` and `doctor`
/// show) with the landing branch; `[repository]` that cannot be read after
/// the landing records `origin` and the landing stands.
#[test]
fn no_push_records_the_configured_remote() {
    let no_push = |db: &Path, repo: &Path| {
        runtime::integrate(db, IntegrateTarget::Task(TaskId::new(1)), repo, None).unwrap()
    };
    let (_dir, repo, db) = renamed("trunk");
    fs::write(
        repo.join("dagq.toml"),
        "[repository]\nbranch = \"trunk\"\nremote = \"upstream\"\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "push to upstream"]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();

    let landed = no_push(&db, &repo);
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_eq!(
        landed["push"],
        json!({"outcome": "skipped", "remote": "upstream", "branch": "trunk", "error": null, "reason": "--no-push"})
    );
    let head = git_out(&repo, &["rev-parse", "trunk"]);
    assert_eq!(
        events_of(&db, run.id(), "push_skipped"),
        [json!({"remote": "upstream", "branch": "trunk", "commit": head, "reason": "--no-push"})]
    );

    // The landed commit leaves `[repository]` unreadable (a remote name Git
    // rejects): the landing began on trunk and stands, and origin is recorded.
    let (_dir, repo, db) = renamed("trunk");
    fs::write(repo.join("dagq.toml"), "[repository]\nbranch = \"trunk\"\n").unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "name the landing branch"]);
    let backend = TestWorkspace::new(
        &db,
        false,
        "printf '[repository]\\nbranch = \"trunk\"\\nremote = \"bad..name\"\\n' > dagq.toml && git add dagq.toml && git commit -q -m break; receipt \"$(git rev-parse HEAD)\"",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let landed = no_push(&db, &repo);
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_eq!(landed["task"]["status"], "completed", "{landed}");
    assert_eq!(
        landed["push"],
        json!({"outcome": "skipped", "remote": "origin", "branch": "trunk", "error": null, "reason": "--no-push"})
    );
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    let head = git_out(&repo, &["rev-parse", "trunk"]);
    assert_eq!(
        events_of(&db, run.id(), "push_skipped"),
        [json!({"remote": "origin", "branch": "trunk", "commit": head, "reason": "--no-push"})]
    );
    let doctor = runtime::doctor(&db, false).unwrap();
    assert!(
        doctor["repository"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("not a valid remote name")),
        "{doctor}"
    );
}

/// A running supervisor resolves the landing branch again only when what
/// the resolution reads changed (task 1078), and still holds the claims at
/// the next pass after the branch stops resolving and resumes them at the
/// next pass after it resolves again (ADR-t615-1): main deleted and
/// created, renamed away and named by origin's HEAD, and a dagq.toml that
/// names a missing branch and then the one there is. The recheck without
/// a change is an hour here, so each stop can only come from the change
/// itself; a branch that does not resolve is resolved again every pass.
#[test]
fn a_running_supervisor_follows_each_change_of_the_landing_branch() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(&db, true, VALID_AGENT));
    let options = SuperviseOptions {
        landing_recheck: Duration::from_secs(3600),
        ..supervise_options(1, false)
    };
    let (telemetry, captured) = Telemetry::capture();
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || telemetry.in_scope(|| supervise_with(&db, &repo, &backend, &options)))
    };
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.supervisors().unwrap().len() == 1
    });
    const HELD: &str = "no task is claimed and no run lands until it resolves";
    const RESUMED: &str = "claiming and landing resume";
    // The `n`th hold and resume, in the order they were logged.
    let transitions = |held: usize, resumed: usize| {
        let started = Instant::now();
        loop {
            let text = captured.text();
            let mut seen = Vec::new();
            for line in text.lines() {
                if line.contains(HELD) {
                    seen.push("held");
                } else if line.contains(RESUMED) {
                    seen.push("resumed");
                }
            }
            let counts = (
                seen.iter().filter(|&&s| s == "held").count(),
                seen.iter().filter(|&&s| s == "resumed").count(),
            );
            if counts == (held, resumed) {
                return seen;
            }
            assert!(
                counts.0 <= held && counts.1 <= resumed,
                "{counts:?} past ({held}, {resumed}): {text}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "({held}, {resumed}) not logged; {counts:?}: {text}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    };
    let stages: [(&str, &dyn Fn()); 6] = [
        ("main deleted", &|| {
            git(&repo, &["update-ref", "-d", "refs/heads/main"])
        }),
        ("main created again", &|| {
            git(&repo, &["update-ref", "refs/heads/main", &seed])
        }),
        ("main renamed to trunk", &|| {
            git(&repo, &["branch", "-m", "main", "trunk"])
        }),
        ("origin's HEAD names trunk", &|| {
            git(&repo, &["update-ref", "refs/remotes/origin/trunk", &seed]);
            git(
                &repo,
                &[
                    "symbolic-ref",
                    "refs/remotes/origin/HEAD",
                    "refs/remotes/origin/trunk",
                ],
            );
        }),
        ("dagq.toml names a missing branch", &|| {
            fs::write(repo.join("dagq.toml"), "[repository]\nbranch = \"nope\"\n").unwrap()
        }),
        ("dagq.toml names trunk", &|| {
            fs::write(repo.join("dagq.toml"), "[repository]\nbranch = \"trunk\"\n").unwrap()
        }),
    ];
    for (index, (stage, change)) in stages.iter().enumerate() {
        change();
        let seen = transitions(index / 2 + 1, index.div_ceil(2));
        assert_eq!(
            seen.last().copied(),
            Some(if index % 2 == 0 { "held" } else { "resumed" }),
            "{stage}"
        );
    }
    options.stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["outcome"], "stopped", "{outcome}");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(queue.show(TaskId::new(1)).unwrap().runs.is_empty());
}
