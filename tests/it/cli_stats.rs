//! `dagq stats` and `dagq mark` / `marks` over a queue: the CLI reading the
//! queue, the host's metrics files and the marks recorded in SQLite. What
//! `stats` makes of a sequence of events is
//! `domain::stats::event_sequence_tests` (task 1709).

use crate::common;

use common::cli::*;

mod stats {
    use std::collections::HashMap;

    use serde_json::{Value, json};

    use super::{invoke, ok};

    #[test]
    fn cli_stats_reads_the_queue_and_since_returns_only_new_runs() {
        use dagq::{
            domain::{ClaimOutcome, CommitSha, LeaseToken},
            infrastructure::sqlite::SqliteQueue,
        };
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue.db");
        crate::common::template::queue(&db);
        let empty = ok(&db, &["stats"]);
        assert_eq!(empty["runs"], json!([]));
        assert_eq!(empty["alerts"], json!([]));
        assert_eq!(empty["overall"]["work"]["median"], Value::Null);
        ok(&db, &["goal", "add", "measured"]);
        ok(&db, &["add", "first", "--goal", "1", "--change", "fix"]);
        ok(&db, &["add", "second"]);
        ok(&db, &["ready", "1", "--bypass-review"]);
        ok(&db, &["ready", "2", "--bypass-review"]);
        let base = "0123456789abcdef0123456789abcdef01234567";
        let finish = |queue: &mut SqliteQueue| {
            let ClaimOutcome::Claimed { run } = queue
                .claim_for_supervisor(&CommitSha::try_from(base).unwrap(), &LeaseToken::new("t"))
                .unwrap()
            else {
                panic!("nothing to claim");
            };
            queue
                .record_runtime_event(
                    run.id(),
                    dagq::domain::EventKind::ReceiptObserved,
                    json!({}),
                )
                .unwrap();
            queue
                .record_runtime_event(
                    run.id(),
                    dagq::domain::EventKind::ValidationFinished,
                    json!({"status": "failed"}),
                )
                .unwrap();
            run.id().clone()
        };
        let mut queue = SqliteQueue::open(&db).unwrap();
        let first = finish(&mut queue);
        let report = ok(&db, &["stats"]);
        assert_eq!(report["runs"][0]["run_id"], first.as_str());
        assert_eq!(report["runs"][0]["goal_id"], 1);
        assert_eq!(report["runs"][0]["failed"], 1);
        assert!(report["runs"][0]["work"].is_i64());
        assert!(report["runs"][0].get("kind").is_none());
        assert!(report.get("kinds").is_none());
        assert_eq!(report["runs"][0]["change"], "fix");
        assert_eq!(report["changes"][0]["change"], "fix");
        assert_eq!(report["changes"][0]["runs"], 1);
        let cursor = report["next_cursor"].as_i64().unwrap();
        assert_eq!(cursor, ok(&db, &["status"])["cursor"].as_i64().unwrap());

        let second = finish(&mut queue);
        let since = ok(&db, &["stats", "--since", &cursor.to_string()]);
        assert_eq!(since["runs"].as_array().unwrap().len(), 1);
        assert_eq!(since["runs"][0]["run_id"], second.as_str());
        assert!(since["next_cursor"].as_i64().unwrap() > cursor);
        let full = ok(&db, &["stats", "--full"]);
        assert_eq!(full["runs"].as_array().unwrap().len(), 2);
        // The tasks' changes read from the queue; their order is
        // `domain::stats::event_sequence_tests::
        // runs_are_grouped_by_change_by_name_and_the_unchanged_last`.
        let changes: Vec<&Value> = full["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| &group["change"])
            .collect();
        assert_eq!(changes, [&json!("fix"), &Value::Null]);
        let goal = ok(&db, &["stats", "--goal", "1"]);
        assert_eq!(goal["runs"].as_array().unwrap().len(), 1);
        assert_eq!(goal["goals"][0]["goal_id"], 1);
        assert!(!invoke(&db, &["stats", "--since", "x"]).status.success());
    }

    /// `stats` summarizes the host's load the supervisor recorded under
    /// the queue's `host/` over its window (task 516): the files read and
    /// the window `--since` / `--until` give. The summary's mean, maximum,
    /// p90 and pageouts per minute are
    /// `domain::host_metrics::tests::the_summary_reads_mean_max_p90_and_the_pageout_rate`.
    #[test]
    fn cli_stats_summarizes_the_host_load_of_its_window() {
        use dagq::domain::host_metrics::{HostSample, file_name, header, local_day};
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue.db");
        crate::common::template::queue(&db);
        // No file yet: an empty summary.
        let empty = ok(&db, &["stats"]);
        assert_eq!(empty["host"]["samples"], 0);
        assert_eq!(empty["host"]["metrics"]["load1"], Value::Null);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let now = i64::try_from(now).unwrap();
        let host = dir.path().join("host");
        std::fs::create_dir_all(&host).unwrap();
        let mut by_file: HashMap<String, String> = HashMap::new();
        // Four samples in the window a minute apart, one long before it.
        for (unix, load, pageouts) in [
            (now - 4000, 99.0, 0.0),
            (now - 240, 1.0, 100.0),
            (now - 180, 2.0, 160.0),
            (now - 120, 3.0, 220.0),
            (now - 60, 10.0, 280.0),
        ] {
            let mut sample = HostSample::new(unix);
            sample.set("load1", Some(load));
            sample.set("pageouts", Some(pageouts));
            by_file
                .entry(file_name(local_day(unix, 0)))
                .or_insert_with(|| format!("{}\n", header()))
                .push_str(&format!("{}\n", sample.row(0)));
        }
        for (name, text) in by_file {
            std::fs::write(host.join(name), text).unwrap();
        }
        let report = ok(&db, &["stats", "--since", &format!("@{}", now - 300)]);
        let summary = &report["host"];
        assert_eq!(summary["from"], now - 300);
        assert_eq!(summary["samples"], 4, "{summary}");
        assert_eq!(summary["first"], now - 240);
        assert_eq!(
            summary["metrics"]["load1"],
            json!({"samples": 4, "min": 1.0, "mean": 4.0, "median": 2.0, "max": 10.0, "p90": 10.0})
        );
        assert_eq!(summary["metrics"]["pageouts_per_min"]["mean"], 60.0);
        // `--until` ends the window at its time.
        let until = ok(
            &db,
            &[
                "stats",
                "--since",
                &format!("@{}", now - 300),
                "--until",
                &format!("@{}", now - 150),
            ],
        );
        assert_eq!(until["host"]["samples"], 2);
        assert_eq!(until["host"]["until"], now - 150);
    }
}

/// `dagq mark` records a person's or a planner's mark in the queue,
/// `--retract` a retraction, and `dagq marks` lists them with the marks
/// derived from the claims the queue holds: a change of the host's
/// toolchain between two claims is a `derived:toolchain` mark and writes no
/// event (ADR-0051 decisions 10, 12). The rules are unit tests (task 1709):
/// the label and the window (`--since` and `--until` as event ids or times)
/// `domain::marks::tests::a_mark_needs_a_short_label`
/// and `marks_are_listed_by_the_time_they_took_effect_within_the_window`,
/// the derived marks `derived_marks_follow_the_claims_and_skip_what_a_start_announced`,
/// a retraction `retracting_takes_a_retractable_mark_once`, a time in the
/// future `application::marks::tests::a_mark_one_millisecond_after_the_clock_is_refused_and_not_recorded`,
/// and the roles that may not mark
/// `domain::authorization::tests::the_jobs_read_and_write_nothing` and
/// `the_observer_records_findings_and_asks_on_one_only`.
#[test]
fn marks_are_recorded_retracted_and_derived_from_the_claims() {
    use dagq::domain::{ClaimOutcome, CommitSha, LeaseToken};
    use dagq::infrastructure::sqlite::SqliteQueue;
    use serde_json::json;
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    ok(&db, &["add", "second"]);
    ok(&db, &["ready", "1", "--bypass-review"]);
    ok(&db, &["ready", "2", "--bypass-review"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    for (host, at) in [
        ("x86_64-apple-darwin", "01"),
        ("aarch64-apple-darwin", "02"),
    ] {
        let ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor(&base, &LeaseToken::new("t"))
            .unwrap()
        else {
            panic!("nothing to claim");
        };
        // The attributes a supervisor's claim records (task 197).
        conn.execute(
            "UPDATE run_events SET created_at=?3, payload=json_set(payload,
               '$.dagq_version','v1','$.parallel',3,'$.rustc_release','1.90.0','$.rustc_host',?2)
             WHERE run_id=?1 AND kind='run_claimed'",
            [
                run.id().as_str(),
                host,
                &format!("2026-09-26T{at}:00:00.000Z"),
            ],
        )
        .unwrap();
    }
    let events_before = ok(&db, &["events", "--after", "0", "--all"])["cursor"].clone();

    let marked = ok_as(
        "planner",
        &db,
        &[
            "mark",
            "parallel 4→3",
            "--note",
            "load",
            "--at",
            "2026-09-26T09:30:00+09:00",
        ],
    );
    assert_eq!(marked["kind"], "mark_recorded");
    assert_eq!(marked["label"], "parallel 4→3");
    assert_eq!(marked["at"], "2026-09-26T00:30:00.000Z");
    assert_eq!(marked["detail"]["by"], "planner");
    assert_eq!(marked["detail"]["note"], "load");
    let id = marked["id"].as_i64().unwrap();
    assert_eq!(id, events_before.as_i64().unwrap() + 1);

    let listed = ok(&db, &["marks"])["marks"].clone();
    let kinds: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|mark| mark["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["mark_recorded", "derived:toolchain"]);
    assert_eq!(listed[1]["id"], serde_json::Value::Null);
    assert_eq!(listed[1]["at"], "2026-09-26T02:00:00.000Z");
    assert_eq!(listed[1]["detail"]["from"], "1.90.0 x86_64-apple-darwin");
    assert_eq!(listed[1]["detail"]["to"], "1.90.0 aarch64-apple-darwin");

    let retracted = ok(&db, &["mark", "--retract", &id.to_string()]);
    assert_eq!(retracted["kind"], "mark_retracted");
    assert_eq!(retracted["detail"]["mark"], json!(id));
    assert_eq!(retracted["detail"]["by"], "human");
    let listed = ok(&db, &["marks"])["marks"].clone();
    assert_eq!(listed[0]["retracted_by"], retracted["id"]);
    // The retraction read back from the queue refuses another.
    assert!(refused(&db, &["mark", "--retract", &id.to_string()]).contains("already retracted"));

    // A job reads marks but may not record one.
    assert!(
        !invoke_as(Some("reviewer"), &db, &["mark", "x"])
            .status
            .success()
    );
    assert_eq!(ok_as("reviewer", &db, &["marks"])["marks"], listed);
}
