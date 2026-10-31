//! The areas of the landed runs (ADR-t980-1) on a real queue and Git:
//! `stats` and `kpi` read the `[areas]` of the main checkout's `dagq.toml`
//! and the landed commit's files from Git whenever they read, so a run
//! landed before the map existed gets its areas, and changing the map
//! reclassifies it. Nothing is stored.
// Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
#[cfg(any())]
use crate::runtime_support::*;

/// `dagq kpi` on the queue at `db` in UTC, with the host-wide settings
/// read from `config` rather than the home of the person running the tests.
// Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
#[cfg(any())]
fn kpi(db: &Path, config: &Path, args: &[&str]) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .without_actor_env()
        .env("TZ", "UTC")
        .env("XDG_CONFIG_HOME", config);
    crate::common::cli::reach(&mut command, db, &["kpi"]);
    let output = command
        .arg("--db")
        .arg(db)
        .arg("kpi")
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

// Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
#[cfg(any())]
#[test]
fn stats_and_kpi_split_the_landed_runs_by_the_areas_of_their_commit() {
    let (dir, repo, db, run) = awaiting_run();
    integrate(&db, 1, &repo).unwrap();
    let config = dir.path().join("config");

    // No [areas]: no area anywhere, as before.
    let stats = stats_full(&db);
    assert_eq!(stats["runs"][0]["run_id"], run.id().as_str());
    assert!(stats["runs"][0].get("areas").is_none(), "{stats}");
    assert!(stats.get("areas").is_none());
    let report = kpi(&db, &config, &["--last", "1"]);
    let landings = report["periods"][0]["kpis"]["landings"]
        .as_object()
        .unwrap();
    assert!(landings.keys().all(|stratum| !stratum.starts_with("area=")));

    // The map written after the landing: the run's commit changed
    // change.txt, which two overlapping areas match.
    fs::write(
        repo.join("dagq.toml"),
        "[areas]\nchange = [\"change.txt\"]\ntext = [\"*.txt\"]\ndocs = [\"docs/**\"]\n\n[kpi.targets.landings]\narea = \"change\"\nmin = 1\n",
    )
    .unwrap();
    let stats = stats_full(&db);
    assert_eq!(stats["runs"][0]["areas"], json!(["change", "text"]));
    let areas: Vec<&Value> = stats["areas"].as_array().unwrap().iter().collect();
    assert_eq!(areas.len(), 2);
    assert_eq!(areas[0]["area"], "change");
    assert_eq!(areas[0]["runs"], 1);
    assert_eq!(areas[1]["area"], "text");
    assert_eq!(areas[1]["runs"], 1);
    let report = kpi(&db, &config, &["--last", "1", "--area", "text"]);
    let landings = &report["periods"][0]["kpis"]["landings"];
    assert_eq!(landings["area=text"]["value"], 1.0);
    assert!(landings.get("area=change").is_none(), "{landings}");
    assert_eq!(landings["all"]["value"], 1.0);
    assert_eq!(report["targets"][0]["stratum"], "area=change");
    assert_eq!(report["targets"][0]["periods"][0]["value"], 1.0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let compared = kpi(
        &db,
        &config,
        &["--last", "1", "--compare", &format!("@{}", now - 86_400)],
    );
    let summary = compared["compare"]["area_summary"].as_object().unwrap();
    assert_eq!(
        summary.keys().collect::<Vec<_>>(),
        ["change", "text"],
        "{compared}"
    );

    // A map none of whose areas matches the file: the run is in `other`.
    fs::write(repo.join("dagq.toml"), "[areas]\ndocs = [\"docs/**\"]\n").unwrap();
    let stats = stats_full(&db);
    assert_eq!(stats["runs"][0]["areas"], json!(["other"]));
    let report = kpi(&db, &config, &["--last", "1"]);
    assert_eq!(
        report["periods"][0]["kpis"]["landings"]["area=other"]["value"],
        1.0
    );
}

/// `report` through the queue service on a queue bound to a repository
/// with `[areas]` and a landed run (ADR-t3102-3 decision 5): the client
/// reads the landed commit's files with Git in the main checkout, and the
/// service, whose Git logs each run, runs none, yet the report splits the
/// landing by its areas.
// Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
#[cfg(any())]
#[test]
fn a_report_through_the_service_reads_the_areas_with_the_client_s_git() {
    let (dir, repo, db, _run) = awaiting_run();
    integrate(&db, 1, &repo).unwrap();
    fs::write(
        repo.join("dagq.toml"),
        "[areas]\nchange = [\"change.txt\"]\ntext = [\"*.txt\"]\n",
    )
    .unwrap();
    let config = dir.path().join("config");
    let git_log = crate::common::service::serve_logging_git(
        &db,
        &[("TZ", "UTC"), ("XDG_CONFIG_HOME", config.to_str().unwrap())],
    );
    fs::write(&git_log, "").unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .without_actor_env()
        .env("TZ", "UTC")
        .env("XDG_CONFIG_HOME", &config);
    crate::common::cli::reach(&mut command, &db, &["report"]);
    let output = command
        .arg("--db")
        .arg(&db)
        .args(["report", "--print", "json"])
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let landings = &report["periods"].as_array().unwrap().last().unwrap()["kpis"]["landings"];
    assert_eq!(landings["area=change"]["value"], 1.0, "{landings}");
    assert_eq!(landings["area=text"]["value"], 1.0, "{landings}");
    assert_eq!(
        fs::read_to_string(&git_log).unwrap(),
        "",
        "the service ran Git"
    );
}

/// `report_inputs` and `report` sent straight to the queue service of a
/// queue bound to a repository, naming a directory whose `.git` file's
/// `gitdir:` is under the bound common directory: the service refuses both
/// without reading that directory's `dagq.toml` (one that does not parse),
/// and answers both for the main checkout it finds from the binding, the
/// landed commit with them.
// Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
#[cfg(any())]
#[test]
fn a_report_naming_another_directory_than_the_main_checkout_is_refused_unread() {
    use dagq::domain::queue_service::{API_VERSION, ServiceRequest, UseCase};
    use dagq::infrastructure::queue_service as service;
    let (dir, repo, db, _run) = awaiting_run();
    integrate(&db, 1, &repo).unwrap();
    let common = fs::canonicalize(repo.join(".git")).unwrap();
    fs::create_dir_all(common.join("worktrees/x")).unwrap();
    let forged = dir.path().join("forged");
    fs::create_dir(&forged).unwrap();
    fs::write(
        forged.join(".git"),
        format!("gitdir: {}\n", common.join("worktrees/x").display()),
    )
    .unwrap();
    fs::write(forged.join("dagq.toml"), "[kpi\nnot toml = =\n").unwrap();
    let config = dir.path().join("config");
    let git_log = crate::common::service::serve_logging_git(
        &db,
        &[("TZ", "UTC"), ("XDG_CONFIG_HOME", config.to_str().unwrap())],
    );
    let queue_dir = fs::canonicalize(&db).unwrap().parent().unwrap().to_owned();
    let call = |use_case: UseCase, params: Value| -> Value {
        let response = service::call(
            &service::socket_path(&queue_dir),
            &ServiceRequest {
                api_version: API_VERSION,
                token: None,
                env_role: None,
                queue: None,
                binding: None,
                use_case,
                params,
            },
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        serde_json::to_value(response).unwrap()
    };
    let inputs = |checkout: &Path| json!({"period": "day", "checkout": checkout});
    let report = |checkout: &Path| {
        json!({
            "period": "day",
            "print": true,
            "diagram": {"tasks": [], "d2_source": false, "reason": "not drawn"},
            "checkout": checkout,
        })
    };
    for (use_case, params) in [
        (UseCase::ReportInputs, inputs(&forged)),
        (UseCase::Report, report(&forged)),
    ] {
        let refused = call(use_case, params);
        assert_eq!(refused["ok"], false, "{use_case:?}: {refused}");
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("is not the main checkout"),
            "{use_case:?}: {message}"
        );
    }
    let commit =
        crate::common::service::events(&db, "run_integrated")[0]["payload"]["commit"].clone();
    let answered = call(UseCase::ReportInputs, inputs(&repo));
    assert_eq!(answered["ok"], true, "{answered}");
    assert_eq!(answered["result"]["landed"], json!([commit]), "{answered}");
    let made = call(UseCase::Report, report(&repo));
    assert_eq!(made["ok"], true, "{made}");
    assert_eq!(
        fs::read_to_string(&git_log).unwrap_or_default(),
        "",
        "the service ran Git"
    );
}

/// `dagq` with `args` on the queue at `db` through the queue service, in
/// the host's time zone (the one the test reads the DB in) with the
/// host-wide settings of `config`: the JSON it prints.
// Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
#[cfg(any())]
fn served(db: &Path, config: &Path, args: &[&str]) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.without_actor_env().env("XDG_CONFIG_HOME", config);
    crate::common::cli::reach(&mut command, db, args);
    let output = command
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Give the repository at `repo` a `dagq.toml` with `[areas]` that map the
/// change the landed `run` made and `[conflicts]`, and the queue at `db` a
/// conflict in it, then read `stats --full`, `stats --ledger` and `kpi`
/// through the queue service, whose Git logs each run (ADR-t3102-3
/// decision 5): the service runs no Git, reads that `dagq.toml` (the
/// `[conflicts]` source is the file), and the areas, the conflict hotspots,
/// the recurring drafts and the KPIs' landings are those `stats` and `kpi`
/// on the DB read with Git themselves. `stats` and `kpi` as served.
// Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
#[cfg(any())]
fn served_as_on_the_db(dir: &Path, repo: &Path, db: &Path, run: &TaskRun) -> (Value, Value) {
    use dagq::application::DraftPlannerStore;
    fs::write(
        repo.join("dagq.toml"),
        "[areas]\nchange = [\"change.txt\"]\ntext = [\"*.txt\"]\n\n[conflicts]\nhotspot_conflicts = 2\n",
    )
    .unwrap();
    SqliteQueue::open(db)
        .unwrap()
        .record_task_event(
            run.task_id(),
            dagq::domain::EventKind::IntegrationDeferred,
            json!({"conflicts": ["change.txt"], "main": "m1"}),
        )
        .unwrap();
    let config = dir.join("config");
    let git_log = crate::common::service::serve_logging_git(
        db,
        &[("XDG_CONFIG_HOME", config.to_str().unwrap())],
    );
    fs::write(&git_log, "").unwrap();

    let stats = served(db, &config, &["stats", "--full"]);
    // Stopped while it reads every event of the queue (ADR-t3745-1): task 1671 takes this cfg off.
    #[cfg(any())]
    {
        let ledger = served(db, &config, &["stats", "--ledger"]);
        assert!(ledger.get("fields").is_some(), "{ledger}");
    }
    let kpi = served(db, &config, &["kpi", "--last", "1"]);
    assert_eq!(
        fs::read_to_string(&git_log).unwrap(),
        "",
        "the service ran Git"
    );

    let on_db = runtime::stats(
        db,
        &dagq::domain::stats::StatsQuery {
            full: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(stats["runs"][0]["areas"], on_db["runs"][0]["areas"]);
    assert_eq!(stats["areas"], on_db["areas"]);
    assert_eq!(stats["conflict_hotspots"], on_db["conflict_hotspots"]);
    assert_eq!(stats["conflict_hotspots"]["config"]["source"], "file");
    assert_eq!(stats["recurring"], on_db["recurring"]);
    let kpi_on_db = dagq::runtime::OneShot::system()
        .kpi_of(
            &SqliteQueue::open(db).unwrap(),
            db,
            &dagq::domain::kpi::KpiQuery {
                last: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        kpi["periods"][0]["kpis"]["landings"],
        kpi_on_db["periods"][0]["kpis"]["landings"]
    );
    (stats, kpi)
}

/// [`served_as_on_the_db`] on a repository with `[areas]`, a landed run
/// and a conflict: the run's areas and the conflicted file's hotspot, with
/// the landings main's history counts, are read.
// Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
#[cfg(any())]
#[test]
fn stats_and_kpi_through_the_service_read_areas_and_main_s_history_with_the_client_s_git() {
    let (dir, repo, db, run) = awaiting_run();
    integrate(&db, 1, &repo).unwrap();
    let (stats, kpi) = served_as_on_the_db(dir.path(), &repo, &db, &run);
    assert_eq!(stats["runs"][0]["areas"], json!(["change", "text"]));
    let hotspots = &stats["conflict_hotspots"];
    assert_eq!(hotspots["history"]["status"], "checked", "{hotspots}");
    assert_eq!(hotspots["files"][0]["path"], "change.txt", "{hotspots}");
    assert_eq!(hotspots["files"][0]["state"], "present", "{hotspots}");
    let landings = &kpi["periods"][0]["kpis"]["landings"];
    assert_eq!(landings["area=change"]["value"], 1.0, "{landings}");
    assert_eq!(landings["area=text"]["value"], 1.0, "{landings}");
}

/// [`served_as_on_the_db`] on a repository whose `core.worktree` names
/// another directory: the service finds, with no Git, the main checkout
/// `main_checkout_of` finds with Git on the DB (the directory of the
/// `.git`), and reads its `dagq.toml`.
// Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
#[cfg(any())]
#[test]
fn stats_and_kpi_of_a_core_worktree_repository_through_the_service_are_those_on_the_db() {
    let (dir, repo, db, run) = awaiting_run();
    integrate(&db, 1, &repo).unwrap();
    let elsewhere = dir.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    let git = Command::new(dagq::infrastructure::git_binary::git_executable().unwrap())
        .arg("-C")
        .arg(repo.join(".git"))
        .args(["config", "core.worktree"])
        .arg(&elsewhere)
        .bounded_output()
        .unwrap();
    assert!(git.status.success(), "{git:?}");
    let (stats, _) = served_as_on_the_db(dir.path(), &repo, &db, &run);
    assert!(stats["runs"][0]["areas"].is_array(), "{stats}");
}

/// A conflict recorded between a client's `stats_inputs` and its `stats`
/// (ADR-t3102-3 decision 5): the service refuses the Git reads made for
/// the inputs before it, the client reads Git again for the new inputs and
/// sends them, and the answer's hotspots, the new file's with them, are
/// those of `stats` on the DB; the service runs no Git all along.
// Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
#[cfg(any())]
#[test]
fn a_conflict_between_the_inputs_and_stats_is_read_again_not_mixed() {
    use dagq::application::{DraftPlannerStore, queue_reads::QueueRead};
    use dagq::domain::queue_service::{API_VERSION, ServiceRequest, UseCase};
    use dagq::infrastructure::queue_service as service;
    let (dir, repo, db, run) = awaiting_run();
    integrate(&db, 1, &repo).unwrap();
    let conflict = |file: &str| {
        SqliteQueue::open(&db)
            .unwrap()
            .record_task_event(
                run.task_id(),
                dagq::domain::EventKind::IntegrationDeferred,
                json!({"conflicts": [file], "main": file}),
            )
            .unwrap();
    };
    conflict("change.txt");
    let config = dir.path().join("config");
    let git_log = crate::common::service::serve_logging_git(
        &db,
        &[("TZ", "UTC"), ("XDG_CONFIG_HOME", config.to_str().unwrap())],
    );
    fs::write(&git_log, "").unwrap();
    let socket = service::socket_path(db.parent().unwrap());
    let sends = std::cell::Cell::new(0);
    let read = QueueRead::parse(UseCase::Stats, &json!({"full": true}))
        .unwrap()
        .unwrap();
    let answer = dagq::compose::call_stats_read(&read, |sent| {
        if matches!(sent, QueueRead::Stats(_)) {
            sends.set(sends.get() + 1);
            if sends.get() == 1 {
                conflict("seed.txt");
            }
        }
        let (use_case, params) = sent.request();
        let response = serde_json::to_value(
            service::call(
                &socket,
                &ServiceRequest {
                    api_version: API_VERSION,
                    token: None,
                    env_role: None,
                    queue: None,
                    binding: None,
                    use_case,
                    params,
                },
                std::time::Duration::from_secs(10),
            )
            .unwrap(),
        )
        .unwrap();
        if response["ok"] == true {
            Ok(response["result"].clone())
        } else {
            anyhow::bail!("{}", response["error"])
        }
    })
    .unwrap();
    assert_eq!(sends.get(), 2, "sent again once");
    assert_eq!(
        fs::read_to_string(&git_log).unwrap(),
        "",
        "the service ran Git"
    );
    let on_db = runtime::stats(
        &db,
        &dagq::domain::stats::StatsQuery {
            full: true,
            ..Default::default()
        },
    )
    .unwrap();
    let files: Vec<&Value> = answer["conflict_hotspots"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|file| file["path"] == "seed.txt")
        .collect();
    assert_eq!(files[0]["state"], "present", "{answer}");
    assert_eq!(answer["conflict_hotspots"], on_db["conflict_hotspots"]);
}
