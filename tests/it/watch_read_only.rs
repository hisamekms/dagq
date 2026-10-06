//! Live read-only polling, distinct from one-shot migrated snapshots.
use crate::common::{self, cli::*};
use dagq::{
    application::watch::{WatchOptions, WatchRecord},
    compose,
    domain::{EventId, EventKind, SessionRole},
    infrastructure::{adapters::SystemProcesses, sqlite::SqliteQueue},
};
use serde_json::json;
use std::time::Duration;

// Add an attention only after the first poll has found no events. This
// synchronizes a concurrent write without a timing-dependent sleep.
struct WriteAfterRead<'a>(&'a SqliteQueue, bool);
impl WatchRecord for WriteAfterRead<'_> {
    fn heartbeat(&mut self, _: i64) -> anyhow::Result<()> {
        if !self.1 {
            self.0.record_queue_event(
                EventKind::ThroughputReviewReported,
                json!({"mode": "daily"}),
            )?;
            self.1 = true;
        }
        Ok(())
    }
    fn end(&mut self, _: i64) -> anyhow::Result<()> {
        Ok(())
    }
}

#[test]
fn the_watch_connection_rejects_writes_and_sees_attention_added_after_a_poll() {
    let (_dir, db) = queue();
    let reader = compose::open_queue_watch(&db).unwrap();
    let writer = SqliteQueue::open(&db).unwrap();
    let error = reader
        .record_queue_event(
            EventKind::ThroughputReviewReported,
            json!({"mode": "daily"}),
        )
        .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<rusqlite::Error>()
            .unwrap()
            .sqlite_error_code(),
        Some(rusqlite::ErrorCode::ReadOnly)
    );
    for role in [None, Some(SessionRole::Inbox), Some(SessionRole::Planner)] {
        for timeout in [Some(common::STEP_LIMIT), None] {
            let options = WatchOptions {
                after: Some(reader.latest_event_id().unwrap()),
                // The planner never receives attention, so its unlimited
                // mode is covered by the CLI refusal test below.
                timeout: if role == Some(SessionRole::Planner) {
                    Some(Duration::ZERO)
                } else {
                    timeout
                },
                interval: Duration::from_millis(1),
                role,
            };
            let mut record = WriteAfterRead(&writer, false);
            let _waiting = common::within(common::STEP_LIMIT, "live read-only watch");
            let result = dagq::application::watch::watch(
                &reader,
                reader.generators().clock.as_ref(),
                &SystemProcesses,
                Some(&mut record),
                &options,
            )
            .unwrap();
            assert!(record.1);
            assert_eq!(
                result["events"].as_array().unwrap().len(),
                usize::from(role != Some(SessionRole::Planner))
            );
        }
    }
}

#[test]
fn cli_and_library_watch_refuse_old_schemas_for_every_role_and_wait_mode() {
    let (dir, db) = queue();
    let raw = rusqlite::Connection::open(&db).unwrap();
    let old = SqliteQueue::SCHEMA_VERSION - 1;
    raw.pragma_update(None, "user_version", old).unwrap();
    for role in [None, Some("inbox"), Some("planner")] {
        for mode in ["--timeout", "--until-attention"] {
            let mut args = vec!["watch", mode];
            if mode == "--timeout" {
                args.push("0");
            }
            if let Some(role) = role {
                args.extend(["--role", role]);
            }
            let error = refused(&db, &args);
            assert!(error.contains("dagq migrate"), "{args:?}: {error}");
            let _waiting = common::within(common::STEP_LIMIT, "old-schema library watch");
            let error = compose::watch(
                &db,
                &WatchOptions {
                    after: Some(EventId::new(0)),
                    timeout: (mode == "--timeout").then_some(Duration::ZERO),
                    interval: Duration::from_millis(1),
                    role: role.map(|r| {
                        if r == "inbox" {
                            SessionRole::Inbox
                        } else {
                            SessionRole::Planner
                        }
                    }),
                },
            )
            .unwrap_err();
            assert!(error.to_string().contains("dagq migrate"), "{error:#}");
        }
    }
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        old
    );
    assert!(!dir.path().join("inbox-watchers").exists());
    assert!(compose::open_queue_watch(&dir.path().join("missing.db")).is_err());
    assert!(!dir.path().join("missing.db").exists());
}

/// Guard both entry points: reverting either CLI open selection or the
/// library opener to a writable connection must make this test fail.
#[test]
fn cli_and_library_watch_use_read_only_connections_on_the_live_queue() {
    let (dir, db) = queue();
    let writer = SqliteQueue::open(&db).unwrap();
    let event = writer
        .record_queue_event(
            EventKind::ThroughputReviewReported,
            json!({"mode": "daily"}),
        )
        .unwrap();
    for role in [None, Some(SessionRole::Inbox), Some(SessionRole::Planner)] {
        for timeout in [Some(Duration::ZERO), None] {
            let options = WatchOptions {
                after: Some(EventId::new(0)),
                timeout,
                interval: Duration::from_millis(1),
                role,
            };
            let _waiting = common::within(common::STEP_LIMIT, "writable watch refusal");
            let error = compose::watch_in(&db, &writer, &options).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("watch requires a live read-only"),
                "{error:#}"
            );
        }
    }
    assert!(!dir.path().join("inbox-watchers").exists());
    {
        let _waiting = common::within(common::STEP_LIMIT, "current-schema library watch");
        let result = compose::watch(
            &db,
            &WatchOptions {
                after: Some(EventId::new(0)),
                timeout: None,
                interval: Duration::from_millis(1),
                role: None,
            },
        )
        .unwrap();
        assert_eq!(result["events"][0]["id"], event.as_i64());
    }
    // Current-schema CLI success crosses watch_in's read-only guard.
    for role in [None, Some("inbox"), Some("planner")] {
        for mode in ["--timeout", "--until-attention"] {
            // A planner never wakes. Its unlimited mode is bounded by a
            // deterministic read error below, after the read-only guard.
            if role == Some("planner") && mode == "--until-attention" {
                continue;
            }
            let mut args = vec!["watch", "--after", "0", mode];
            if mode == "--timeout" {
                args.push("0");
            }
            if let Some(role) = role {
                args.extend(["--role", role]);
            }
            let result = ok(&db, &args);
            assert_eq!(
                result["events"].as_array().unwrap().len(),
                usize::from(role != Some("planner"))
            );
        }
    }
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TABLE run_events")
        .unwrap();
    let error = refused(&db, &["watch", "--role", "planner", "--until-attention"]);
    assert!(error.contains("no such table: run_events"), "{error}");
}
