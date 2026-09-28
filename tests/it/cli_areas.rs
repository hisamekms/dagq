//! The areas of the landed runs (ADR-t980-1) on a real queue and Git:
//! `stats` and `kpi` read the `[areas]` of the main checkout's `dagq.toml`
//! and the landed commit's files from Git whenever they read, so a run
//! landed before the map existed gets its areas, and changing the map
//! reclassifies it. Nothing is stored.
use crate::runtime_support::*;

/// `dagq kpi` on the queue at `db` in UTC, with the host-wide settings
/// read from `config` rather than the home of the person running the tests.
fn kpi(db: &Path, config: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .env("TZ", "UTC")
        .env("XDG_CONFIG_HOME", config)
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
