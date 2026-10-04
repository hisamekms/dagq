//! `dagq kpi` (ADR-0051 decisions 1–9 and 14–19) on a real queue: the
//! periods, the changes, the host's `[kpi]` settings and targets, and a
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

/// A fix task and a task without a change land today; the host's
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
    ok(&db, &["add", "first", "--change", "fix"]);
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
    assert_eq!(landings["change=fix"]["value"], 1.0);
    assert_eq!(landings["change=unknown"]["value"], 1.0);
    // Claimed by hand: no parallel recorded.
    assert_eq!(landings["parallel=unknown"]["value"], 2.0);
    assert_eq!(today["kpis"]["phase.work"]["all"]["n"], 2);
    assert!(today["kpis"]["lead_time"]["all"]["median"].is_number());
    // Landed without an `integrate` attempt: the slot was never held.
    assert_eq!(today["kpis"]["landing_utilization"]["all"]["value"], 0.0);
    assert_eq!(today["kpis"]["landing_attempt"]["all"]["n"], 0);
    assert_eq!(today["details"]["landing_utilization"]["attempts"], 0);
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
        &["--period", "week", "--last", "1", "--change", "docs"],
    );
    assert_eq!(week["period"], "week");
    let strata = week["periods"][0]["kpis"]["landings"].as_object().unwrap();
    assert!(strata.contains_key("all") && !strata.contains_key("change=fix"));
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
    assert_eq!(
        compare["change_summary"]["fix"]["phase.work"]["after"]["n"],
        1
    );

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
    ok(&db, &["add", "first", "--change", "fix"]);
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
    // The host's load of the period rides along, as `kpi` prints it.
    let periods = json["periods"].as_array().unwrap();
    assert_eq!(periods.last().unwrap()["host"]["samples"], 0, "{json}");
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

/// Any label is a change when dagq.toml names no set (ADR-t980-1):
/// `stats` and `kpi` group the runs by the label as written, `--change` and
/// a target's `change` take it, and a comparison without `--change`
/// summarises every change it saw. The task's kind is gone: `add --kind`,
/// `kpi --kind`, `--by kind` and a target's `kind` are refused.
#[test]
fn kpi_and_stats_group_the_runs_by_any_label() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        dir.path().join("host.toml"),
        "[kpi]\nmin_samples = 1\n[kpi.targets.landings]\nchange = \"frontend\"\nmin = 1\n",
    )
    .unwrap();
    assert!(
        !invoke(&db, &["add", "old", "--kind", "runtime"])
            .status
            .success()
    );
    ok(&db, &["add", "web", "--change", "frontend"]);
    ok(&db, &["add", "crate", "--change", "runtime"]);
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
    assert!(stats.get("kinds").is_none(), "{stats}");
    let changes: Vec<&str> = stats["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["change"].as_str().unwrap_or("none"))
        .collect();
    assert_eq!(changes, ["frontend", "runtime", "none"]);

    let report = kpi_ok(&db, &config, &["--last", "1"]);
    let landings = &report["periods"][0]["kpis"]["landings"];
    assert_eq!(landings["all"]["value"], 3.0);
    for stratum in ["change=frontend", "change=runtime", "change=unknown"] {
        assert_eq!(landings[stratum]["value"], 1.0, "{stratum}");
    }
    let target = &report["targets"][0];
    assert_eq!(target["stratum"], "change=frontend");
    assert!(
        landings
            .as_object()
            .unwrap()
            .keys()
            .all(|stratum| !stratum.starts_with("kind="))
    );

    let only = kpi_ok(&db, &config, &["--last", "1", "--change", "frontend"]);
    let strata = only["periods"][0]["kpis"]["landings"].as_object().unwrap();
    assert!(strata.contains_key("change=frontend") && !strata.contains_key("change=runtime"));
    let refused = kpi(None, &db, &config, &["--change", "Front End"]);
    assert!(!refused.status.success());
    for gone in [&["--kind", "frontend"][..], &["--by", "kind"][..]] {
        assert!(!kpi(None, &db, &config, gone).status.success(), "{gone:?}");
    }

    let compared = kpi_ok(
        &db,
        &config,
        &["--last", "1", "--compare", &mark.to_string()],
    );
    assert!(compared["compare"].get("summary").is_none());
    let summary = compared["compare"]["change_summary"].as_object().unwrap();
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
            "--change",
            "frontend",
        ],
    );
    let summary = asked["compare"]["change_summary"].as_object().unwrap();
    assert_eq!(summary.keys().collect::<Vec<_>>(), ["frontend"]);
}

/// `dagq kpi` puts the host's load the supervisor recorded under the
/// queue's `host/` next to each period, the `--since`/`--until` window
/// and both sides of `--compare` (task 872): cut at the spans' bounds (the
/// start exclusive), empty where nothing was recorded, and with the reason
/// when the records cannot be read, without failing `kpi` or judging a
/// target on it.
#[test]
fn kpi_summarizes_the_host_load_of_each_span() {
    use dagq::domain::host_metrics::{HostSample, file_name, header, local_day};
    let (dir, db) = queue();
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        dir.path().join("host.toml"),
        "[kpi.targets.landings]\nmin = 5\n",
    )
    .unwrap();
    // Nothing recorded yet: an empty summary per period.
    let empty = kpi_ok(&db, &config, &["--last", "2"]);
    for period in empty["periods"].as_array().unwrap() {
        assert_eq!(period["host"]["samples"], 0, "{period}");
        assert_eq!(period["host"]["metrics"]["load1"], Value::Null);
        assert!(period["host"].get("error").is_none());
        // Nor CPU per landing or load over the cores (goal 72).
        assert_eq!(
            period["kpis"]["cpu_per_landing"]["all"]["value"],
            Value::Null
        );
        assert_eq!(period["unavailable"]["cpu_per_landing"], "no_host_records");
        assert_eq!(
            period["kpis"]["load_per_core"]["all"]["median"],
            Value::Null
        );
    }
    let base: i64 = 1_790_000_000;
    let host = dir.path().join("host");
    std::fs::create_dir_all(&host).unwrap();
    let mut text = format!("{}\n", header());
    for (unix, load) in [
        (base, 1.0),
        (base + 60, 2.0),
        (base + 120, 4.0),
        (base + 180, 8.0),
    ] {
        let mut sample = HostSample::new(unix);
        sample.set("load1", Some(load));
        sample.set("cpu_total", Some(100.0 * load));
        sample.set("cpu_claude", Some(100.0 * load));
        text.push_str(&format!("{}\n", sample.row(0)));
    }
    let day_file = host.join(file_name(local_day(base, 0)));
    std::fs::write(&day_file, text).unwrap();

    let window = kpi_ok(
        &db,
        &config,
        &[
            "--since",
            &format!("@{base}"),
            "--until",
            &format!("@{}", base + 120),
        ],
    );
    let summary = &window["periods"][0]["host"];
    assert_eq!(summary["from"], base + 1);
    assert_eq!(summary["until"], base + 120);
    assert_eq!(summary["samples"], 2, "{summary}");
    assert_eq!(
        summary["metrics"]["load1"],
        json!({"samples": 2, "min": 2.0, "mean": 3.0, "median": 2.0, "max": 4.0, "p90": 4.0})
    );
    // 60 s at 2 and 4 cores: 360 CPU s, and no landing to divide by.
    let cpu = &window["periods"][0]["details"]["cpu_per_landing"];
    assert_eq!(cpu["cpu_secs"]["total"], 360.0, "{cpu}");
    assert_eq!(cpu["cpu_secs"]["by_kind"]["claude"], 360.0);
    let kpis = &window["periods"][0]["kpis"];
    assert_eq!(kpis["cpu_per_landing"]["all"]["value"], Value::Null);
    assert_eq!(kpis["load_per_core"]["all"]["n"], 2);
    assert!(kpis["load_per_core"]["all"]["p90"].is_number(), "{kpis}");
    // The host is no KPI: the target judges only the landings.
    assert!(
        window["targets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|target| target["kpi"] == "landings")
    );

    let compared = kpi_ok(
        &db,
        &config,
        &[
            "--compare",
            &format!("@{}..@{base},@{base}..@{}", base - 60, base + 180),
        ],
    );
    assert_eq!(compared["compare"]["before"]["host"]["samples"], 1);
    assert_eq!(compared["compare"]["after"]["host"]["samples"], 3);
    assert_eq!(
        compared["compare"]["after"]["host"]["metrics"]["load1"]["max"],
        8.0
    );

    // A file that cannot be read: the reason, and `kpi` still answers.
    std::fs::remove_file(&day_file).unwrap();
    std::fs::create_dir(&day_file).unwrap();
    let unreadable = kpi_ok(
        &db,
        &config,
        &[
            "--since",
            &format!("@{base}"),
            "--until",
            &format!("@{}", base + 120),
        ],
    );
    let summary = &unreadable["periods"][0]["host"];
    assert_eq!(summary["samples"], 0);
    assert!(
        summary["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty()),
        "{summary}"
    );
}

/// A worker_question's topics reach `stats` and `kpi` (ADR-t947-2
/// decision 4): per primary topic the asks and the time to the answer,
/// the secondary topic in `codes`, and the answer's wait as
/// `ask.worker_question_wait` by `topic=`.
#[test]
fn the_worker_question_topics_reach_stats_and_kpi() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    ok(&db, &["add", "first"]);
    let asked = ok(
        &db,
        &[
            "ask",
            "--kind",
            "worker_question",
            "--because",
            "scope",
            "--topic",
            "adr_conflict",
            "--topic",
            "out_of_scope_change",
            "--question",
            "Which way?",
            "--task",
            "1",
        ],
    );
    ok(&db, &["answer", &asked["id"].to_string(), "--text", "A"]);

    let topics = &ok(&db, &["stats", "--full"])["worker_question_topics"];
    assert_eq!(topics["asks"], 1, "{topics}");
    assert_eq!(topics["codes"]["out_of_scope_change"], 1, "{topics}");
    let primary = &topics["by_topic"]["adr_conflict"];
    assert_eq!(primary["asks"], 1, "{topics}");
    assert_eq!(primary["by_reason_category"]["scope"], 1, "{topics}");
    assert_eq!(primary["to_answer"]["count"], 1, "{topics}");
    assert_eq!(
        primary["night"]["count"].as_i64().unwrap() + primary["day"]["count"].as_i64().unwrap(),
        1,
        "{topics}"
    );

    let report = kpi_ok(&db, &config, &["--last", "1"]);
    let today = &report["periods"][0];
    let waits = &today["kpis"]["ask.worker_question_wait"];
    assert_eq!(waits["all"]["n"], 1, "{waits}");
    assert_eq!(waits["topic=adr_conflict"]["n"], 1, "{waits}");
    assert_eq!(
        today["details"]["worker_question_topics"]["asks"], 1,
        "{today}"
    );
}

/// A target's `kind` is refused with a pointer to `change` and `area`
/// (ADR-t980-1), in the host's `host.toml` as in the repository's
/// `dagq.toml`.
#[test]
fn a_target_bound_by_kind_is_refused_with_its_replacement() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        dir.path().join("host.toml"),
        "[kpi.targets.landings]\nkind = \"runtime\"\nmin = 1\n",
    )
    .unwrap();
    let refused = kpi(None, &db, &config, &["--last", "1"]);
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(
        error.contains("kind of a target was removed") && error.contains("change or area"),
        "{error}"
    );
}

/// The throughput review's jobs (task 1173) reach `stats --full` and `kpi`
/// per mode: a skipped hour is no job, `failed` and `error` are failures,
/// a mode without a job is listed too, and `--compare` reads them.
#[test]
fn the_throughput_review_jobs_reach_stats_and_kpi_per_mode() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    ok(&db, &["goal", "add", "a goal"]);
    let mark = ok(&db, &["mark", "hourly agent"])["id"].as_i64().unwrap();
    let queue = SqliteQueue::open(&db).unwrap();
    let record = |kind: EventKind, mode: &str, period: &str, extra: Value| {
        let mut payload = json!({"mode": mode, "period": period});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        queue.record_queue_event(kind, payload).unwrap();
    };
    let finished = EventKind::ThroughputReviewFinished;
    let started = EventKind::ThroughputReviewStarted;
    record(
        finished,
        "hourly",
        "2026-09-29T01",
        json!({"outcome": "skipped"}),
    );
    for (mode, period, session, outcome, secs) in [
        ("hourly", "2026-09-29T02", "h2", "succeeded", 40),
        ("hourly", "2026-09-29T03", "h3", "failed", 20),
        ("daily", "2026-09-28", "d", "error", 900),
    ] {
        record(
            started,
            mode,
            period,
            json!({"session_id": session, "launch": {"provider": "claude"}}),
        );
        record(
            finished,
            mode,
            period,
            json!({"session_id": session, "outcome": outcome, "duration_secs": secs}),
        );
    }

    let jobs = &ok(&db, &["stats", "--full"])["jobs"]["throughput_review"];
    assert_eq!(jobs["count"], 3, "{jobs}");
    assert_eq!(jobs["failed"], 2, "{jobs}");
    assert_eq!(jobs["failed_rate"], 0.667, "{jobs}");
    assert_eq!(jobs["by_provider"]["claude"]["count"], 3, "{jobs}");
    assert_eq!(jobs["by_model"]["unknown"]["count"], 3, "{jobs}");
    let hourly = &jobs["by_mode"]["hourly"];
    assert_eq!(
        (&hourly["count"], &hourly["failed"], &hourly["failed_rate"]),
        (&json!(2), &json!(1), &json!(0.5)),
        "{jobs}"
    );
    assert_eq!(hourly["secs"]["total"], 60, "{jobs}");
    assert_eq!(jobs["by_mode"]["daily"]["secs"]["total"], 900, "{jobs}");
    assert_eq!(jobs["by_mode"]["weekly"]["count"], 0, "{jobs}");
    // A goal has none of the queue's reviews.
    let goal = &ok(&db, &["stats", "--goal", "1", "--full"])["jobs"]["throughput_review"];
    assert_eq!(goal["count"], 0, "{goal}");

    let report = kpi_ok(&db, &config, &["--last", "1"]);
    let kpis = &report["periods"][0]["kpis"];
    let count = &kpis["job.count.throughput_review"];
    for (stratum, value) in [
        ("all", 3.0),
        ("provider=claude", 3.0),
        ("model=unknown", 3.0),
        ("mode=hourly", 2.0),
        ("mode=daily", 1.0),
        ("mode=weekly", 0.0),
    ] {
        assert_eq!(count[stratum]["value"], value, "{stratum}: {count}");
    }
    assert_eq!(
        kpis["job.failed_rate.throughput_review"]["mode=hourly"]["value"],
        0.5
    );
    assert_eq!(
        kpis["job.secs.throughput_review"]["mode=daily"]["median"],
        900.0
    );

    let compared = kpi_ok(
        &db,
        &config,
        &["--last", "1", "--compare", &mark.to_string()],
    );
    let strata = &compared["compare"]["strata"]["job.count.throughput_review"];
    assert_eq!(strata["mode=hourly"]["after"]["value"], 2.0, "{strata}");
    assert_eq!(strata["mode=hourly"]["before"]["value"], 0.0, "{strata}");
}

#[test]
fn kpi_cross_reaches_periods_and_compare_without_replacing_single_axes() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    for args in [
        vec!["add", "codex", "--provider", "codex", "--change", "fix"],
        vec!["add", "claude headless", "--headless", "--change", "fix"],
        vec![
            "add",
            "claude interactive",
            "--interactive",
            "--change",
            "fix",
        ],
        vec!["add", "different change", "--headless", "--change", "test"],
    ] {
        ok(&db, &args);
    }
    for id in ["1", "2", "3", "4"] {
        ok(&db, &["ready", id, "--bypass-review"]);
    }
    let mark = ok(&db, &["mark", "cross comparison"])["id"].to_string();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    for _ in 0..4 {
        let ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor_in_order(
                &base,
                &LeaseToken::new("t"),
                &[],
                None,
                &dagq::domain::worker_model::WorkerTrial::default(),
                &dagq::domain::provider_switch::WorkerRoute::direct(
                    &dagq::domain::worker::Worker::ALL,
                ),
            )
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
    let args = [
        "--last",
        "1",
        "--change",
        "fix",
        "--by",
        "route",
        "--by",
        "provider",
        "--compare",
        &mark,
    ];
    let plain = kpi_ok(&db, &config, &args);
    let mut cross_args = args.to_vec();
    cross_args.push("--cross");
    let crossed = kpi_ok(&db, &config, &cross_args);
    let keys = [
        "cross:change=fix|provider=codex|route=headless",
        "cross:change=fix|provider=claude|route=headless",
        "cross:change=fix|provider=claude|route=interactive",
    ];
    for key in keys {
        assert_eq!(
            crossed["periods"][0]["kpis"]["landings"][key],
            json!({"n": 1, "value": 1.0})
        );
        assert_eq!(
            crossed["compare"]["strata"]["landings"][key]["after"],
            json!({"n": 1, "value": 1.0})
        );
        assert!(plain["periods"][0]["kpis"]["landings"].get(key).is_none());
        assert!(plain["compare"]["strata"]["landings"].get(key).is_none());
    }
    for key in [
        "all",
        "change=fix",
        "provider=claude",
        "provider=codex",
        "route=headless",
        "route=interactive",
    ] {
        assert_eq!(
            crossed["periods"][0]["kpis"]["landings"][key],
            plain["periods"][0]["kpis"]["landings"][key]
        );
        assert_eq!(
            crossed["compare"]["strata"]["landings"][key],
            plain["compare"]["strata"]["landings"][key]
        );
    }
    assert_eq!(
        crossed["periods"][0]["kpis"]["landings"]["route=headless"]["n"],
        3
    );
}

/// Claim the next task with `attributes` in its `run_claimed` and land it;
/// returns the claim's event id.
fn land_claimed(db: &Path, queue: &mut SqliteQueue, attributes: Value) -> i64 {
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor_in_order(
            &base,
            &LeaseToken::new("t"),
            &[],
            Some(&attributes),
            &dagq::domain::worker_model::WorkerTrial::default(),
            &dagq::domain::provider_switch::WorkerRoute::direct(&dagq::domain::worker::Worker::ALL),
        )
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
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT id FROM run_events WHERE kind = 'run_claimed' AND run_id = ?1",
            [run.id().to_string()],
            |row| row.get(0),
        )
        .unwrap()
}

/// The queue's events a few milliseconds apart, so that each mark has a
/// time of its own.
fn tick() {
    std::thread::sleep(std::time::Duration::from_millis(5));
}

/// Routine marks (ADR-t1381-1) — a supervisor's start at the same
/// `parallel`, its stop and a derived build — are confounders only: a
/// person's mark next to them is a change of its own, close marks with
/// them between are still one change, naming one splits at it alone, and
/// a claim that derives a build and a `parallel` names the `parallel`.
#[test]
fn kpi_compare_leaves_routine_marks_out_of_every_change() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    std::fs::create_dir_all(config.join("dagq")).unwrap();
    std::fs::write(config.join("dagq/host.toml"), "[kpi]\nmin_samples = 1\n").unwrap();
    for index in 0..5 {
        ok(&db, &["add", &format!("task {index}")]);
        ok(&db, &["ready", &(index + 1).to_string(), "--bypass-review"]);
    }
    let mut queue = SqliteQueue::open(&db).unwrap();
    let start = |queue: &SqliteQueue, parallel: i64, build: &str| {
        tick();
        queue
            .record_queue_event(
                EventKind::SupervisorStarted,
                json!({"supervisor": "s", "parallel": parallel, "dagq_version": build, "handoff": true}),
            )
            .unwrap()
            .as_i64()
    };
    let mark = |label: &str| {
        tick();
        ok(&db, &["mark", label])["id"].as_i64().unwrap()
    };
    // The first start sets `parallel`: a change, a run before the mark.
    start(&queue, 3, "b1");
    land_claimed(
        &db,
        &mut queue,
        json!({"dagq_version": "b1", "parallel": 3}),
    );
    let a = mark("a");
    let handoff = start(&queue, 3, "b2");
    tick();
    queue
        .record_queue_event(
            EventKind::SupervisorStopped,
            json!({"supervisor": "s", "dagq_version": "b2"}),
        )
        .unwrap();
    land_claimed(
        &db,
        &mut queue,
        json!({"dagq_version": "b2", "parallel": 3}),
    );
    tick();
    queue
        .record_queue_event(EventKind::RunEnvChanged, json!({"changed": ["X"]}))
        .unwrap();
    start(&queue, 3, "b2");
    let b = mark("b");
    land_claimed(
        &db,
        &mut queue,
        json!({"dagq_version": "b2", "parallel": 3}),
    );
    tick();
    let both = land_claimed(
        &db,
        &mut queue,
        json!({"dagq_version": "b3", "parallel": 4}),
    );
    tick();
    let build = land_claimed(
        &db,
        &mut queue,
        json!({"dagq_version": "b4", "parallel": 4}),
    );
    let compare = |cursor: i64| {
        kpi_ok(
            &db,
            &config,
            &["--last", "1", "--compare", &cursor.to_string()],
        )["compare"]
            .clone()
    };
    let kinds = |marks: &Value| -> Vec<String> {
        marks
            .as_array()
            .unwrap()
            .iter()
            .map(|mark| mark["kind"].as_str().unwrap().to_owned())
            .collect()
    };
    let confounders = |compare: &Value| -> Vec<(String, String)> {
        compare["confounders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["position"].as_str().unwrap().to_owned(),
                    c["kind"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };

    // A person's mark with only routine marks around it splits alone.
    let at_a = compare(a);
    assert_eq!(kinds(&at_a["split"]["marks"]), ["mark_recorded"], "{at_a}");
    assert_eq!(at_a["split"]["separable"], true);
    assert_eq!(at_a["split"]["marks"][0]["id"], a);
    assert_eq!(at_a["before"]["end"], at_a["split"]["start"]);
    assert_eq!(at_a["after"]["start"], at_a["split"]["start"]);
    let around = confounders(&at_a);
    for routine in [
        ("before", "supervisor_started"),
        ("after", "supervisor_started"),
        ("after", "supervisor_stopped"),
        ("after", "derived:dagq_version"),
    ] {
        let routine = (routine.0.to_owned(), routine.1.to_owned());
        assert!(around.contains(&routine), "{routine:?}: {at_a}");
    }
    // `[run.env]` and the person's mark after it, a routine start between
    // and no run: one change.
    let at_b = compare(b);
    assert_eq!(
        kinds(&at_b["split"]["marks"]),
        ["run_env_changed", "mark_recorded"],
        "{at_b}"
    );
    assert_eq!(at_b["split"]["separable"], false);
    let overlapping = at_b["overlapping"].as_array().unwrap();
    assert_eq!(overlapping.len(), 1, "{at_b}");
    assert_eq!(kinds(&overlapping[0]), ["run_env_changed", "mark_recorded"]);
    // Naming a routine handoff splits at it alone.
    let at_handoff = compare(handoff);
    assert_eq!(
        kinds(&at_handoff["split"]["marks"]),
        ["supervisor_started"],
        "{at_handoff}"
    );
    assert_eq!(at_handoff["split"]["marks"][0]["id"], handoff);
    assert_eq!(at_handoff["split"]["separable"], true);
    assert!(confounders(&at_handoff).contains(&("before".into(), "mark_recorded".into())));
    // A claim that derives a build and a `parallel` names the `parallel`;
    // its build sits after.
    let at_both = compare(both);
    assert_eq!(
        kinds(&at_both["split"]["marks"]),
        ["derived:parallel"],
        "{at_both}"
    );
    assert_eq!(at_both["split"]["separable"], true);
    let same_claim: Vec<&Value> = at_both["confounders"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["detail"]["claim_event"] == both)
        .collect();
    assert_eq!(same_claim.len(), 1, "{at_both}");
    assert_eq!(same_claim[0]["kind"], "derived:dagq_version");
    assert_eq!(same_claim[0]["position"], "after");
    // A claim that derives only a build names that mark alone.
    let at_build = compare(build);
    assert_eq!(
        kinds(&at_build["split"]["marks"]),
        ["derived:dagq_version"],
        "{at_build}"
    );
    assert_eq!(at_build["split"]["separable"], true);
    assert_eq!(
        at_build["split"]["marks"][0]["detail"]["claim_event"],
        build
    );
}
