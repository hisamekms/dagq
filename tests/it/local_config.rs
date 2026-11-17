//! The main checkout's uncommitted, expiring `dagq.local.toml`
//! (ADR-t5108-1 decision 7) as `dagq status --role inbox` and `dagq
//! doctor` show it: the queue service reads the bound repository's files
//! as they are on disk, beside the queue's newest record of the layer.
//! What the report holds and how values compare are the unit tests' of
//! `domain::local_config` and `infrastructure::run_env`.
use crate::common::cli::ok;
use crate::runtime_support::*;
use dagq::domain::{EventKind, local_config::LocalLayerTracker};

/// `dagq.local.toml` with `body` under an `expires_at` `secs` from now.
fn layer(repo: &Path, secs: i64, body: &str) {
    let expires = dagq::domain::transcript::millis_text(
        dagq::application::unix_millis(std::time::SystemTime::now()) + secs * 1000,
    );
    fs::write(
        repo.join("dagq.local.toml"),
        format!("[local]\nexpires_at = '{expires}'\n{body}"),
    )
    .unwrap();
}

#[test]
fn status_for_the_inbox_and_doctor_show_the_main_checkouts_local_layer() {
    let (_dir, repo, db) = fixture();
    // Bound to the repository as `init` in it binds the queue.
    let common_dir = dagq::infrastructure::adapters::git_common_dir(&repo).unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .bind_repository(common_dir.to_str().unwrap())
        .unwrap();
    fs::write(
        repo.join("dagq.toml"),
        "[disk]\nreserve = '5%'\n[stall]\nidle_without_receipt_secs = 100\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-q", "-m", "committed settings"]);
    // Within the warning: it expires in 12 hours.
    layer(
        &repo,
        12 * 60 * 60,
        "[disk]\nreserve = '12%'\n[stall]\nidle_without_receipt_secs = 1_00\n",
    );
    // A supervisor's record of the layer as it began, made by the
    // production tracker from the production read.
    let now = dagq::application::unix_millis(std::time::SystemTime::now());
    let read = dagq::infrastructure::run_env::inspect_local_config(&repo, now);
    let record = LocalLayerTracker::default().observe(read, now).unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .record_queue_event(
            EventKind::SupervisorLocalConfigChanged,
            serde_json::to_value(&record).unwrap(),
        )
        .unwrap();
    // A landing changes the committed value under the layer.
    fs::write(
        repo.join("dagq.toml"),
        "[disk]\nreserve = '8%'\n[stall]\nidle_without_receipt_secs = 100\n",
    )
    .unwrap();
    git(&repo, &["commit", "-q", "-am", "a landing"]);
    fs::write(repo.join(".gitignore"), "!dagq.local.toml\n").unwrap();

    let status = ok(&db, &["status", "--role", "inbox"])["local_config"].clone();
    let doctor = ok(&db, &["doctor"])["local_config"].clone();
    for shown in [&status, &doctor] {
        assert_eq!(shown["state"], "effective", "{shown}");
        assert!(shown["hash"].is_string(), "{shown}");
        assert!(shown["expires_at"].is_string(), "{shown}");
        assert_eq!(shown["expiring_soon"], true, "{shown}");
        let key = |name: &str| {
            shown["keys"]
                .as_array()
                .unwrap()
                .iter()
                .find(|key| key["key"] == name)
                .unwrap_or_else(|| panic!("{name}: {shown}"))
                .clone()
        };
        let reserve = key("reserve");
        assert_eq!(reserve["table"], "disk");
        assert_eq!(reserve["committed"], "\"8%\"", "{reserve}");
        assert_eq!(reserve["local"], "\"12%\"", "{reserve}");
        assert_eq!(reserve["redundant"], false, "{reserve}");
        assert_eq!(
            reserve["committed_changed"],
            json!({"from": "'5%'", "to": "'8%'"}),
            "{reserve}"
        );
        let idle = key("idle_without_receipt_secs");
        assert_eq!(idle["redundant"], true, "{idle}");
        assert_eq!(shown["last_change"]["change"], "effective", "{shown}");
    }
    assert_eq!(status.get("ignored"), None, "{status}");
    assert_eq!(doctor["ignored"], false, "{doctor}");
    assert!(
        doctor["warnings"][0]
            .as_str()
            .unwrap()
            .contains("not ignored by .gitignore"),
        "{doctor}"
    );
    fs::write(repo.join(".gitignore"), "dagq.local.toml\n").unwrap();
    assert_eq!(ok(&db, &["doctor"])["local_config"]["ignored"], true);

    // Rejected: a key outside the allowlist, with why.
    layer(&repo, 24 * 60 * 60, "[repository]\npush = false\n");
    let rejected = ok(&db, &["status", "--role", "inbox"])["local_config"].clone();
    assert_eq!(rejected["state"], "rejected", "{rejected}");
    assert_eq!(
        rejected["reason"], "local table not allowed: repository",
        "{rejected}"
    );
    assert_eq!(rejected["keys"], json!([]), "{rejected}");
    assert_eq!(
        ok(&db, &["doctor"])["local_config"]["reason"],
        "local table not allowed: repository"
    );
}
