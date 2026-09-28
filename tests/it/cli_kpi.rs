//! `dagq kpi` (ADR-0051 decisions 1–9 and 14–19) on a real queue: the
//! periods, the kinds, the host's `[kpi]` settings and targets, and a
//! comparison across a mark.
use dagq::domain::EventKind;
use dagq::domain::LeaseToken;
use std::{path::Path, process::Command};

use dagq::{
    domain::{ClaimOutcome, CommitSha, DraftOrigin, TaskId},
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};

use crate::common::{Bounded, WithoutActor, cli::*};

/// `dagq kpi` in UTC, with the host-wide `host.toml` read from `config`
/// rather than the home of the person running the tests.
fn kpi(role: Option<&str>, db: &Path, config: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.without_actor_env();
    command.env("TZ", "UTC").env("XDG_CONFIG_HOME", config);
    if let Some(role) = role {
        command.env("DAGQ_ROLE", role);
    }
    command
        .arg("--db")
        .arg(db)
        .arg("kpi")
        .args(args)
        .bounded_output()
        .unwrap()
}

fn kpi_ok(db: &Path, config: &Path, args: &[&str]) -> Value {
    let output = kpi(None, db, config, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// A runtime task and a task without a kind land today; the host's
/// `host.toml` sets `min_samples` and a target; a mark splits a comparison.
#[test]
fn kpi_reads_the_queue_the_host_settings_and_the_marks() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    std::fs::create_dir_all(config.join("dagq")).unwrap();
    std::fs::write(
        config.join("dagq/host.toml"),
        "[push]\ncommand = [\"true\"]\n[kpi]\nmin_samples = 1\nbreach_periods = 2\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("host.toml"),
        "[kpi.targets.landings]\nmin = 5\n",
    )
    .unwrap();
    ok(&db, &["add", "first", "--kind", "runtime"]);
    ok(&db, &["add", "second"]);
    ok(&db, &["ready", "1", "--bypass-review"]);
    ok(&db, &["ready", "2", "--bypass-review"]);
    let mark = ok(&db, &["mark", "sccache"])["id"].as_i64().unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    for _ in 0..2 {
        let ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor(&base, &LeaseToken::new("t"))
            .unwrap()
        else {
            panic!("nothing to claim");
        };
        for (kind, payload) in [
            (EventKind::ReceiptObserved, json!({})),
            (
                EventKind::ValidationFinished,
                json!({"status": "awaiting_integration"}),
            ),
            (EventKind::RunIntegrated, json!({"status": "integrated"})),
        ] {
            queue.record_runtime_event(run.id(), kind, payload).unwrap();
        }
    }

    let report = kpi_ok(&db, &config, &["--last", "2", "--by", "parallel"]);
    assert_eq!(report["period"], "day");
    assert_eq!(report["utc_offset_secs"], 0);
    assert_eq!(report["config"]["min_samples"], 1);
    assert_eq!(report["config"]["sources"]["min_samples"], "host");
    assert_eq!(report["config"]["sources"]["breach_weeks"], "default");
    let periods = report["periods"].as_array().unwrap();
    assert_eq!(periods.len(), 2);
    let today = &periods[1];
    assert_eq!(today["partial"], true);
    assert_eq!(today["runs"], 2);
    let landings = &today["kpis"]["landings"];
    assert_eq!(landings["all"], json!({"n": 2, "value": 2.0}));
    assert_eq!(landings["kind=runtime"]["value"], 1.0);
    assert_eq!(landings["kind=unknown"]["value"], 1.0);
    // Claimed by hand: no parallel recorded.
    assert_eq!(landings["parallel=unknown"]["value"], 2.0);
    assert_eq!(today["kpis"]["phase.work"]["all"]["n"], 2);
    assert!(today["kpis"]["lead_time"]["all"]["median"].is_number());
    assert_eq!(
        today["unavailable"]["improvement_proposals"],
        "not_recorded"
    );
    assert_eq!(today["marks"][0]["label"], "sccache");
    assert_eq!(today["comparison"]["landings"]["all"]["previous"], 0.0);
    assert_eq!(periods[0]["partial"], false);
    // The queue's host.toml adds the target; yesterday, without a landing,
    // is judged off it (min_samples 1), today is not judged yet.
    let target = &report["targets"][0];
    assert_eq!(target["kpi"], "landings");
    assert_eq!(target["stratum"], "all");
    assert_eq!(target["source"], "host");
    assert_eq!(target["state"], "breach");
    assert_eq!(target["periods"][1]["reason"], "partial");

    let week = kpi_ok(
        &db,
        &config,
        &["--period", "week", "--last", "1", "--kind", "docs"],
    );
    assert_eq!(week["period"], "week");
    let strata = week["periods"][0]["kpis"]["landings"].as_object().unwrap();
    assert!(strata.contains_key("all") && !strata.contains_key("kind=runtime"));
    assert!(week["periods"][0]["label"].as_str().unwrap().contains("-W"));

    let compared = kpi_ok(
        &db,
        &config,
        &["--last", "1", "--compare", &mark.to_string()],
    );
    let compare = &compared["compare"];
    assert_eq!(compare["split"]["marks"][0]["label"], "sccache");
    assert_eq!(compare["split"]["separable"], true);
    assert_eq!(compare["after"]["runs"], 2);
    assert_eq!(compare["before"]["runs"], 0);
    assert_eq!(compare["strata"]["landings"]["all"]["after"]["value"], 2.0);
    assert_eq!(compare["summary"]["runtime"]["phase.work"]["after"]["n"], 1);

    let window = kpi_ok(
        &db,
        &config,
        &["--since", "@1", "--until", &mark.to_string()],
    );
    assert_eq!(window["period"], "window");
    assert_eq!(
        window["periods"][0]["kpis"]["landings"]["all"]["value"],
        0.0
    );

    // The observer and the jobs read it.
    for role in ["observer", "reviewer"] {
        assert!(
            kpi(Some(role), &db, &config, &["--last", "1"])
                .status
                .success()
        );
    }
    for args in [
        &["--period", "month"][..],
        &["--compare", "x"],
        &["--last", "0"],
        &["--at", "1", "--since", "1"],
    ] {
        assert!(!kpi(None, &db, &config, args).status.success(), "{args:?}");
    }
    std::fs::write(dir.path().join("host.toml"), "[kpi]\nmin_samples = 0\n").unwrap();
    let output = kpi(None, &db, &config, &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("host.toml:2"));
}

/// A job's draft next to a landing: `drafts_per_landing` and
/// `draft_backlog` read the draft's origin from the queue, as `stats`'
/// `draft_flow` does, and a `[kpi]` target judges them (task 611).
#[test]
fn kpi_counts_the_drafts_registered_per_landing() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    std::fs::write(
        dir.path().join("host.toml"),
        "[kpi.targets.draft_backlog]\nmax = 0\n",
    )
    .unwrap();
    ok(&db, &["add", "landed"]);
    ok(&db, &["ready", "1", "--bypass-review"]);
    let gap = ok(&db, &["add", "gap"])["id"].as_i64().unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_draft_origin(TaskId::new(gap), DraftOrigin::GoalGap, &json!({}))
        .unwrap();
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&base, &LeaseToken::new("t"))
        .unwrap()
    else {
        panic!("nothing to claim");
    };
    queue
        .record_runtime_event(
            run.id(),
            EventKind::RunIntegrated,
            json!({"status": "integrated"}),
        )
        .unwrap();

    let report = kpi_ok(&db, &config, &["--last", "1"]);
    let today = &report["periods"][0];
    assert_eq!(
        today["kpis"]["drafts_per_landing"],
        json!({"all": {"n": 1, "value": 1.0}})
    );
    let backlog = &today["kpis"]["draft_backlog"]["all"];
    assert_eq!(backlog["value"], 1.0);
    assert!(backlog["max"].is_number());
    let drafts = &today["details"]["drafts"];
    assert_eq!(drafts["by_origin"]["goal_gap"]["drafts_per_landing"], 1.0);
    assert!(drafts["by_origin"].get("follow_up").is_none());
    let flow = &ok(&db, &["stats", "--full"])["draft_flow"];
    assert_eq!(flow["drafts_per_landing"], 1.0);
    assert_eq!(flow["backlog"].as_f64(), backlog["value"].as_f64());
    let target = &report["targets"][0];
    assert_eq!(target["kpi"], "draft_backlog");
    assert_eq!(target["periods"][0]["reason"], "partial");
}

/// `dagq report` in UTC with the host-wide `host.toml` under `config`.
fn report(role: Option<&str>, db: &Path, config: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.without_actor_env();
    command.env("TZ", "UTC").env("XDG_CONFIG_HOME", config);
    if let Some(role) = role {
        command.env("DAGQ_ROLE", role);
    }
    command
        .arg("--db")
        .arg(db)
        .arg("report")
        .args(args)
        .bounded_output()
        .unwrap()
}

/// `dagq report` writes the report of a day or week as the supervisor does,
/// today's apart as partial, and returns the paths; it records nothing.
#[test]
fn report_writes_the_html_and_json_and_returns_their_paths() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    ok(&db, &["add", "first", "--kind", "runtime"]);
    let run = |args: &[&str]| -> Value {
        let output = report(None, &db, &config, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let today = run(&[]);
    assert_eq!(today["period"], "day");
    assert_eq!(today["partial"], true);
    let label = today["label"].as_str().unwrap();
    let html = Path::new(today["html"].as_str().unwrap());
    let reports = db.parent().unwrap().join("reports");
    assert_eq!(html, reports.join(format!("daily/{label}.partial.html")));
    let page = std::fs::read_to_string(html).unwrap();
    for external in [
        "<script", "<link", "<img", "@import", "url(", "src=", "http://", "https://",
    ] {
        assert!(!page.contains(external), "{external}");
    }
    let json: Value =
        serde_json::from_slice(&std::fs::read(today["json"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(json["report"]["label"], label);
    assert!(Path::new(today["index"].as_str().unwrap()).exists());

    // A finished week, into another directory.
    let out = dir.path().join("out");
    let week = run(&[
        "--period",
        "week",
        "--at",
        "2026-09-16T12:00:00Z",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(week["label"], "2026-W38");
    assert_eq!(week["partial"], false);
    assert!(out.join("weekly/2026-W38.html").exists());
    assert!(out.join("index.html").exists());

    let printed = run(&["--print", "json", "--at", "2026-09-23T12:00:00Z"]);
    assert_eq!(printed["report"]["label"], "2026-09-23");
    assert_eq!(printed["period"], "day");
    assert!(!reports.join("daily/2026-09-23.json").exists());

    // Nothing is recorded: the supervisor still writes its own.
    let events = ok(&db, &["events", "--kind", "report_written"]);
    assert_eq!(events["events"], json!([]), "{events}");
    // The observer and the jobs do not write files.
    for role in ["observer", "reviewer"] {
        assert!(!report(Some(role), &db, &config, &[]).status.success());
    }
    assert!(
        !report(None, &db, &config, &["--period", "month"])
            .status
            .success()
    );
    std::fs::write(
        dir.path().join("host.toml"),
        "[report]\nkeep_daily_days = 0\n",
    )
    .unwrap();
    assert!(!report(None, &db, &config, &[]).status.success());
}

/// Any label is a kind (ADR-t624-1): `stats` and `kpi` group the runs by
/// the label as written, `--kind` and a target's `kind` take it, and a
/// comparison without `--kind` summarises every kind it saw, a kind of
/// the four this repository uses included.
#[test]
fn kpi_and_stats_group_the_runs_by_any_label() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        dir.path().join("host.toml"),
        "[kpi]\nmin_samples = 1\n[kpi.targets.landings]\nkind = \"frontend\"\nmin = 1\n",
    )
    .unwrap();
    ok(&db, &["add", "web", "--kind", "frontend"]);
    ok(&db, &["add", "crate", "--kind", "runtime"]);
    ok(&db, &["add", "plain"]);
    for id in ["1", "2", "3"] {
        ok(&db, &["ready", id, "--bypass-review"]);
    }
    let mark = ok(&db, &["mark", "split"])["id"].as_i64().unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    for _ in 0..3 {
        let ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor(&base, &LeaseToken::new("t"))
            .unwrap()
        else {
            panic!("nothing to claim");
        };
        for (kind, payload) in [
            (EventKind::ReceiptObserved, json!({})),
            (
                EventKind::ValidationFinished,
                json!({"status": "awaiting_integration"}),
            ),
            (EventKind::RunIntegrated, json!({"status": "integrated"})),
        ] {
            queue.record_runtime_event(run.id(), kind, payload).unwrap();
        }
    }

    let stats = ok(&db, &["stats"]);
    let kinds: Vec<&str> = stats["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| kind["kind"].as_str().unwrap_or("none"))
        .collect();
    assert_eq!(kinds, ["frontend", "runtime", "none"]);

    let report = kpi_ok(&db, &config, &["--last", "1"]);
    let landings = &report["periods"][0]["kpis"]["landings"];
    assert_eq!(landings["all"]["value"], 3.0);
    for stratum in ["kind=frontend", "kind=runtime", "kind=unknown"] {
        assert_eq!(landings[stratum]["value"], 1.0, "{stratum}");
    }
    let target = &report["targets"][0];
    assert_eq!(target["stratum"], "kind=frontend");

    let only = kpi_ok(&db, &config, &["--last", "1", "--kind", "frontend"]);
    let strata = only["periods"][0]["kpis"]["landings"].as_object().unwrap();
    assert!(strata.contains_key("kind=frontend") && !strata.contains_key("kind=runtime"));
    let refused = kpi(None, &db, &config, &["--kind", "Front End"]);
    assert!(!refused.status.success());

    let compared = kpi_ok(
        &db,
        &config,
        &["--last", "1", "--compare", &mark.to_string()],
    );
    let summary = compared["compare"]["summary"].as_object().unwrap();
    let summarised: Vec<&String> = summary.keys().collect();
    assert_eq!(summarised, ["frontend", "runtime", "unknown"]);
    assert_eq!(summary["frontend"]["phase.work"]["after"]["n"], 1);
    let asked = kpi_ok(
        &db,
        &config,
        &[
            "--last",
            "1",
            "--compare",
            &mark.to_string(),
            "--kind",
            "frontend",
        ],
    );
    let summary = asked["compare"]["summary"].as_object().unwrap();
    assert_eq!(summary.keys().collect::<Vec<_>>(), ["frontend"]);
}
