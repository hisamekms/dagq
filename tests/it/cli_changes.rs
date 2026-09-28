//! The change a task declares (ADR-t980-1) on a real queue: `add --change`
//! and `edit --change` record it and `show`, `list` and `search` print it;
//! the `[tasks] changes` of the main checkout's `dagq.toml` holds `add`,
//! `edit`, `lint` and `submit` to its set, and without it any label or none
//! is accepted; `stats` and `kpi` split the landed runs by it.
use crate::common::cli::{invoke, ok, queue, refused, submit_from};
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
fn change_is_added_shown_listed_searched_and_edited_without_a_set() {
    let (_dir, db) = queue();
    let sound = ["--acceptance", "works", "--verify", "cargo test"];
    let added = ok(
        &db,
        &[&["add", "searchable fix", "--change", "fix"][..], &sound].concat(),
    );
    assert_eq!(added["change"], "fix");
    let id = added["id"].to_string();
    assert_eq!(ok(&db, &["show", &id])["task"]["change"], "fix");
    assert_eq!(ok(&db, &["show", &id, "--full"])["task"]["change"], "fix");
    assert_eq!(ok(&db, &["list"])["tasks"][0]["change"], "fix");
    let found = ok(&db, &["search", "searchable"]);
    assert_eq!(found["hits"][0]["change"], "fix", "{found}");
    // Without a set, a task may declare none, and any label is a change.
    let plain = ok(&db, &[&["add", "unsaid"][..], &sound].concat());
    assert_eq!(plain["change"], Value::Null);
    assert_eq!(ok(&db, &["list"])["tasks"][0]["change"], Value::Null);
    assert!(
        ok(&db, &["search", "unsaid"])["hits"][0]
            .get("change")
            .is_none()
    );
    let edited = ok(&db, &["edit", &id, "--change", "front-end_2"]);
    assert_eq!(edited["change"], "front-end_2");
    let event = ok(&db, &["show", &id, "--full"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["kind"] == "task_edited")
        .unwrap()
        .clone();
    assert_eq!(event["payload"]["from"]["change"], "fix");
    assert_eq!(event["payload"]["to"]["change"], "front-end_2");
    let long = "c".repeat(65);
    for args in [
        &["add", "bad", "--change", "Fix"][..],
        &["add", "bad", "--change", "unknown"][..],
        &["add", "bad", "--change", "all"][..],
        &["add", "bad", "--change", &long][..],
        &["edit", &id, "--change", "two words"][..],
    ] {
        let output = invoke(&db, args);
        assert!(!output.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("must be a slug"),
            "{args:?}"
        );
    }
    // Nothing holds lint or submit to a change.
    assert_eq!(ok(&db, &["lint", "2"])["violations"], json!([]));
    assert!(submit_from(&db, None, None, &["2"]).status.success());
    ok(&db, &["ready", &id, "--bypass-review"]);
    assert_eq!(
        refused(&db, &["edit", &id, "--change", "feature"]),
        format!("task {id} is ready; only a draft or submitted task can be edited")
    );
}

#[test]
fn the_set_of_changes_of_dagq_toml_holds_add_edit_lint_and_submit() {
    let (_fixture, repo, db) = fixture();
    let repository = GitRepository::inspect(&repo).unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .bind_repository(
            &dagq::infrastructure::adapters::path_text(&repository.common_dir).unwrap(),
        )
        .unwrap();
    fs::write(
        repo.join("dagq.toml"),
        "[tasks]\nchanges = [\"feature\", \"fix\"]\n",
    )
    .unwrap();
    let outside = refused(&db, &["add", "outside", "--change", "docs"]);
    assert!(outside.contains("not one of [tasks] changes"), "{outside}");
    assert!(outside.contains("feature, fix"), "{outside}");
    let sound = ["--acceptance", "works", "--verify", "x"];
    let declared = ok(
        &db,
        &[&["add", "declared", "--change", "fix"][..], &sound].concat(),
    );
    assert_eq!(declared["change"], "fix");
    let declared = declared["id"].to_string();
    let undeclared = ok(&db, &[&["add", "undeclared"][..], &sound].concat())["id"].to_string();
    let refused_edit = refused(&db, &["edit", &declared, "--change", "docs"]);
    assert!(
        refused_edit.contains("not one of [tasks] changes"),
        "{refused_edit}"
    );

    let linted = ok(&db, &["lint", &declared, &undeclared]);
    let violations = linted["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1, "{linted}");
    assert_eq!(violations[0]["code"], "missing_change");
    assert_eq!(violations[0]["task_id"].to_string(), undeclared);
    let submitted = submit_from(&db, None, None, &[&declared, &undeclared]);
    assert!(!submitted.status.success());
    let stderr = String::from_utf8_lossy(&submitted.stderr);
    assert!(stderr.contains("declares no change"), "{stderr}");
    // Nothing moved: both are still drafts.
    for id in [&declared, &undeclared] {
        assert_eq!(ok(&db, &["show", id])["task"]["status"], "draft");
    }
    ok(&db, &["edit", &undeclared, "--change", "feature"]);
    assert_eq!(
        ok(&db, &["lint", &declared, &undeclared])["violations"],
        json!([])
    );
    assert!(
        submit_from(&db, None, None, &[&declared, &undeclared])
            .status
            .success()
    );

    // A value the set no longer names is linted as outside it.
    fs::write(repo.join("dagq.toml"), "[tasks]\nchanges = [\"feature\"]\n").unwrap();
    let linted = ok(&db, &["lint", &declared]);
    assert_eq!(linted["violations"][0]["code"], "change_outside_set");
    // Without the key, any change or none again.
    fs::write(repo.join("dagq.toml"), "[tasks]\n").unwrap();
    assert_eq!(
        ok(&db, &["add", "free", "--change", "docs"])["change"],
        "docs"
    );
    assert_eq!(ok(&db, &["add", "none"])["change"], Value::Null);
}

#[test]
fn stats_and_kpi_split_the_landed_runs_by_their_change() {
    let (dir, repo, db, run) = awaiting_run();
    // As `add --change` would have recorded it.
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE tasks SET change='fix' WHERE id=1", [])
        .unwrap();
    integrate(&db, 1, &repo).unwrap();
    let config = dir.path().join("config");

    let stats = stats_full(&db);
    assert_eq!(stats["runs"][0]["run_id"], run.id().as_str());
    assert_eq!(stats["runs"][0]["change"], "fix");
    assert_eq!(stats["changes"][0]["change"], "fix");
    assert_eq!(stats["changes"][0]["runs"], 1);

    // Always split by change, without `[areas]` or `--by`.
    let report = kpi(&db, &config, &["--last", "1"]);
    let landings = &report["periods"][0]["kpis"]["landings"];
    assert_eq!(landings["change=fix"]["value"], 1.0, "{landings}");
    let only = kpi(&db, &config, &["--last", "1", "--change", "feature"]);
    let landings = only["periods"][0]["kpis"]["landings"].as_object().unwrap();
    assert!(!landings.contains_key("change=fix"), "{landings:?}");
    assert!(landings.contains_key("all"));
    fs::write(
        repo.join("dagq.toml"),
        "[kpi.targets.landings]\nchange = \"fix\"\nmin = 1\n",
    )
    .unwrap();
    let judged = kpi(&db, &config, &["--last", "1"]);
    assert_eq!(judged["targets"][0]["stratum"], "change=fix");
    assert_eq!(judged["targets"][0]["periods"][0]["value"], 1.0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let compared = kpi(
        &db,
        &config,
        &["--last", "1", "--compare", &format!("@{}", now - 86_400)],
    );
    let summary = compared["compare"]["change_summary"].as_object().unwrap();
    assert_eq!(summary.keys().collect::<Vec<_>>(), ["fix"], "{compared}");
    assert!(
        !invoke(&db, &["kpi", "--change", "Fix"]).status.success(),
        "not a label"
    );
}
