//! Cases use the real entries and schema, including failed statements in
//! separate transactions: a constraint error aborts a Postgres transaction.
use super::ContractStore;
use crate::application::NotLeader;
use crate::infrastructure::sql_exec::{SqlError, SqlRead, SqlWrite, sql_params};
use crate::infrastructure::sqlite::WriteIdentity;
use anyhow::{Context, Result, ensure};
use std::time::Duration;

const LEASE: Duration = Duration::from_secs(86_400);
const INSERT_TASK: &str = "INSERT INTO tasks(title, description, acceptance, verification_commands, status) VALUES ('contract', '', '', '[]', 'draft')";

/// Take, extend, release and take over again through the real leader port.
/// All time bounds come from the DB, without a process clock or sleeping.
pub fn leader_lifecycle<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    let initial = store.write_leader()?.0;
    ensure!(initial.lease_until_ms == 0, "case needs an unheld leader");
    let generation = store
        .take_write_leader("contract-a", LEASE, false)?
        .context("take leader")?;
    ensure!(generation == initial.generation + 1);
    let (before, now) = store.write_leader()?;
    ensure!(before.holder.as_deref() == Some("contract-a"));
    ensure!(before.acquired_at_ms.is_some_and(|at| at <= now));
    ensure!(before.lease_until_ms > now);
    ensure!(
        store
            .take_write_leader("contract-b", LEASE, false)?
            .is_none()
    );
    ensure!(!store.renew_write_leader("contract-a", generation - 1, LEASE)?);
    ensure!(!store.release_write_leader("contract-b", generation)?);
    ensure!(store.write_leader()?.0 == before);
    ensure!(store.renew_write_leader("contract-a", generation, LEASE * 2)?);
    let renewed = store.write_leader()?.0;
    ensure!(renewed.lease_until_ms > before.lease_until_ms);
    ensure!(renewed.acquired_at_ms == before.acquired_at_ms);
    ensure!(store.release_write_leader("contract-a", generation)?);
    ensure!(store.write_leader()?.0.lease_until_ms == 0);
    let next = store
        .take_write_leader("contract-b", Duration::ZERO, false)?
        .context("released takeover")?;
    ensure!(next == generation + 1);
    // A zero-length lease expires at the DB, so no wait is needed.
    let last = store
        .take_write_leader("contract-c", LEASE, false)?
        .context("expired takeover")?;
    ensure!(last == next + 1);
    ensure!(!store.release_write_leader("contract-b", next)?);
    ensure!(store.release_write_leader("contract-c", last)?);
    Ok(())
}

/// The singleton index rejects a second row even with another primary key.
pub fn leader_singleton<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    for singleton in [1_i64, 2, 0, -1] {
        let tx = store.write_entry("contract_singleton")?;
        unique_error(tx.execute(
            "INSERT INTO write_leader(singleton, generation, lease_until_ms) VALUES (?1, 0, 0)",
            sql_params![singleton],
        ))?;
        drop(tx);
    }
    let tx = store.read_entry("contract_count")?;
    ensure!(
        tx.query_row("SELECT count(*) FROM write_leader", &[], |r| r
            .get::<_, i64>(0))?
            == 1
    );
    Ok(())
}

/// A retained generation refuses at the entry before business SQL can run;
/// the current identity commits successfully through that same entry.
pub fn stale_writer<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let leader = open()?;
    let first = leader
        .take_write_leader("contract-writer", LEASE, false)?
        .context("first takeover")?;
    let stale = open()?.with_identity(WriteIdentity {
        holder: Some("contract-writer".into()),
        generation: first,
    });
    ensure!(leader.release_write_leader("contract-writer", first)?);
    let next = leader
        .take_write_leader("contract-writer", LEASE, false)?
        .context("second takeover")?;
    // Even an expired lease is fenced by generation, not by its deadline.
    ensure!(leader.renew_write_leader("contract-writer", next, Duration::ZERO)?);
    let before = leader.write_leader()?.0;
    let read = leader.read_entry("contract_before")?;
    let count: i64 = read.query_row("SELECT count(*) FROM tasks", &[], |r| r.get(0))?;
    drop(read);
    let attempt = || -> Result<()> {
        let tx = stale.write_entry("contract_stale")?;
        tx.execute(INSERT_TASK, &[])?;
        S::commit(tx)
    };
    let error = attempt().err().context("stale write succeeded")?;
    ensure!(error.downcast_ref::<NotLeader>().is_some(), "{error:#}");
    ensure!(leader.write_leader()?.0 == before);
    let current = open()?.with_identity(WriteIdentity {
        holder: Some("contract-writer".into()),
        generation: next,
    });
    let tx = current.write_entry("contract_current")?;
    let id = tx.insert_id(INSERT_TASK, &[])?;
    S::commit(tx)?;
    let read = current.read_entry("contract_after")?;
    ensure!(
        read.query_row("SELECT count(*) FROM tasks", &[], |r| r.get::<_, i64>(0))? == count + 1
    );
    ensure!(
        read.query_row(
            "SELECT title FROM tasks WHERE id = ?1",
            sql_params![id],
            |r| r.get::<_, String>(0)
        )? == "contract"
    );
    drop(read);
    let tx = current.write_entry("contract_cleanup")?;
    tx.execute("DELETE FROM tasks WHERE id = ?1", sql_params![id])?;
    S::commit(tx)?;
    ensure!(leader.release_write_leader("contract-writer", next)?);
    Ok(())
}

/// Require a unique violation, not merely any failure (a malformed statement,
/// missing relation or aborted transaction must fail the case).
fn unique_error(result: Result<usize, SqlError>) -> Result<()> {
    let error = result.err().context("duplicate was accepted")?;
    if let SqlError::Driver { source, .. } = &error {
        if let Some(rusqlite::Error::SqliteFailure(code, _)) =
            source.downcast_ref::<rusqlite::Error>()
        {
            ensure!(
                [
                    rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
                    rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                ]
                .contains(&code.extended_code),
                "{error}"
            );
            return Ok(());
        }
        #[cfg(feature = "postgres")]
        if let Some(error) = source.downcast_ref::<postgres::Error>() {
            ensure!(error.code() == Some(&postgres::error::SqlState::UNIQUE_VIOLATION));
            return Ok(());
        }
    }
    anyhow::bail!("expected unique violation: {error}")
}

/// Each invariant of ADR-t3986-1 decision 5 rejects an otherwise valid second
/// row. Different tasks isolate the queue-wide integrating slot from the
/// per-task unfinished slot; different ids isolate every tested unique key.
pub fn unique_indexes<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    for (status, same_task) in [
        ("running", true),
        ("integrated", true),
        ("integrating", false),
    ] {
        let tx = store.write_entry("contract_runs")?;
        let a = tx.insert_id(INSERT_TASK, &[])?;
        let b = if same_task {
            a
        } else {
            tx.insert_id(INSERT_TASK, &[])?
        };
        let insert = "INSERT INTO task_runs(id, task_id, status, requested_provider, actual_provider, base_commit) VALUES (?1, ?2, ?3, 'codex', 'codex', 'base')";
        tx.execute(insert, sql_params!["contract-run-a", a, status])?;
        unique_error(tx.execute(insert, sql_params!["contract-run-b", b, status]))?;
        drop(tx);
    }
    let fixtures = [
        (
            "INSERT INTO proposals(id, status, owner_origin, submitted_at, created_at, updated_at) VALUES (1, 'draft', 'user', '', '', '')",
            "INSERT INTO plan_reviews(proposal_id, attempt, supervisor_token, started_at) VALUES (1, ?1, 'contract', 0)",
        ),
        (
            "",
            "INSERT INTO asks(kind, question, asked_by, reason_category) VALUES ('contract', 'contract', 'user', 'scope')",
        ),
        (
            "",
            "INSERT INTO findings(kind, target, summary, first_seen_at, last_seen_at, recorded_by, updated_at) VALUES ('contract', 'queue', 'contract', 0, 0, 'user', 0)",
        ),
    ];
    for (setup, insert) in fixtures {
        let tx = store.write_entry("contract_unique")?;
        if !setup.is_empty() {
            tx.execute(setup, &[])?;
        }
        let params = if insert.contains("?1") {
            sql_params![1_i64]
        } else {
            &[]
        };
        tx.execute(insert, params)?;
        let params = if insert.contains("?1") {
            sql_params![2_i64]
        } else {
            &[]
        };
        unique_error(tx.execute(insert, params))?;
        drop(tx);
    }
    Ok(())
}

/// Read and write the base schema's identity, JSON and boolean columns, and
/// both a fraction and NULL in columns mapping SQLite REAL to float8.
pub fn base_types<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_types")?;
    let id = tx.insert_id(INSERT_TASK, &[])?;
    let next = tx.insert_id(INSERT_TASK, &[])?;
    ensure!(next > id);
    let json = serde_json::json!([{"日本語": [1, true, null]}]);
    tx.execute(
        "UPDATE tasks SET verification_commands = ?1, wait_for_build = ?2 WHERE id = ?3",
        sql_params![json, true, id],
    )?;
    let values = tx.query_row(
        "SELECT verification_commands, wait_for_build FROM tasks WHERE id = ?1",
        sql_params![id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
    )?;
    ensure!(serde_json::from_str::<serde_json::Value>(&values.0)? == json);
    ensure!(values.1);
    ensure!(!tx.query_row(
        "SELECT wait_for_build FROM tasks WHERE id = ?1",
        sql_params![next],
        |r| r.get::<_, bool>(0)
    )?);
    for (unix, load) in [(1_i64, Some(0.125_f64)), (2, None)] {
        let sample = tx.insert_id("INSERT INTO node_samples(node_id, unix, local_day, source, load1) VALUES ('contract', ?1, 1, 'contract', ?2)", sql_params![unix, load])?;
        ensure!(
            tx.query_row(
                "SELECT load1 FROM node_samples WHERE id = ?1",
                sql_params![sample],
                |r| r.get::<_, Option<f64>>(0)
            )? == load
        );
    }
    Ok(())
}

/// Connection actors survive the event row round trip. Nested request scopes
/// restore both an existing requester and NULL, even inside one transaction.
pub fn event_actors<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    use crate::domain::{
        EventActor,
        actor::{ActorContext, ActorRole},
    };
    use crate::infrastructure::event_actor::event_actor;

    let store = open()?;
    store.set_actor(ActorContext::instance(ActorRole::Supervisor, 42));
    let tx = store.read_entry("contract_actor_read")?;
    ensure!(
        tx.query_row("SELECT dagq_actor_id()", &[], |r| r.get::<_, String>(0))? == "supervisor:42"
    );
    store.set_actor(ActorContext::user());
    ensure!(tx.query_row("SELECT dagq_actor_id()", &[], |r| r.get::<_, String>(0))? == "user");
    drop(tx);
    let tx = store.write_entry("contract_actor_events")?;
    let record = |role: &str, id: &str, requester: Option<&str>| -> Result<()> {
        let event_id = tx.insert_id("INSERT INTO run_events(kind, payload, actor_role, actor_id, requested_by) VALUES ('observe_started', '{}', dagq_actor_role(), dagq_actor_id(), dagq_requested_by())", &[])?;
        let expected = EventActor {
            role: role.into(),
            id: id.into(),
            requested_by: requester.map(str::to_owned),
        };
        let restored = tx.query_row(
            "SELECT actor_role, actor_id, requested_by FROM run_events WHERE id = ?1",
            sql_params![event_id],
            |row| {
                let role: String = row.get("actor_role")?;
                let id: String = row.get("actor_id")?;
                let requested_by: Option<String> = row.get("requested_by")?;
                let actor = event_actor(row)?;
                assert_eq!(
                    actor,
                    Some(EventActor {
                        role,
                        id,
                        requested_by
                    })
                );
                Ok(actor)
            },
        )?;
        ensure!(restored == Some(expected));
        Ok(())
    };
    record("user", "user", None)?;
    store.set_actor(ActorContext::instance(ActorRole::Supervisor, 42));
    let outer = store.request(Some("outer-job".into()));
    ensure!(outer.is_none());
    record("supervisor", "supervisor:42", Some("outer-job"))?;
    let previous = store.request(Some("inner-job".into()));
    record("supervisor", "supervisor:42", Some("inner-job"))?;
    store.request(previous);
    record("supervisor", "supervisor:42", Some("outer-job"))?;
    store.request(outer);
    record("supervisor", "supervisor:42", None)?;
    drop(tx);
    // A new transaction must not inherit a transaction-local requester.
    let tx = store.read_entry("contract_actor_after")?;
    ensure!(
        tx.query_row("SELECT dagq_requested_by()", &[], |r| r
            .get::<_, Option<String>>(0))?
            .is_none()
    );
    Ok(())
}

/// An event's body written through the shared `dagq_event_body()` reads back
/// the same on both sides of the threshold. Compared as JSON: `jsonb` keeps the
/// document, not its spelling.
pub fn event_bodies<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    use crate::application::{EVENT_BODY_THRESHOLD, is_large_event_body};
    use serde_json::{Value, json};

    let store = open()?;
    let tx = store.write_entry("contract_event_bodies")?;
    let frame = json!({"message": "本文"}).to_string().len();
    for size in [
        EVENT_BODY_THRESHOLD - 1,
        EVENT_BODY_THRESHOLD,
        EVENT_BODY_THRESHOLD + 1,
    ] {
        let body = json!({"message": format!("本文{}", "x".repeat(size - frame))});
        let text = body.to_string();
        ensure!(text.len() == size);
        ensure!(is_large_event_body(&text) == (size > EVENT_BODY_THRESHOLD));
        let event_id = tx.insert_id(
            "INSERT INTO run_events(kind, payload, actor_role, actor_id, requested_by) VALUES ('observe_started', dagq_event_body(?1), dagq_actor_role(), dagq_actor_id(), dagq_requested_by())",
            sql_params![text],
        )?;
        let stored: String = tx.query_row(
            "SELECT payload FROM run_events WHERE id = ?1",
            sql_params![event_id],
            |row| row.get(0),
        )?;
        ensure!(
            serde_json::from_str::<Value>(&stored)? == body,
            "a body of {size} bytes changed in the row"
        );
    }
    Ok(())
}

/// NULL disables each typed filter; a value selects only matching rows.
/// JSON booleans are integer expressions in both dialects and need an
/// explicit comparison, including missing fields and JSON null.
pub fn typed_conditions<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    use crate::infrastructure::sql_dialect::JsonType;
    let store = open()?;
    let read = store.read_entry("contract_typed_conditions")?;
    let rows = "WITH rows(id, name, payload) AS (VALUES
        (CAST(4294967296 AS BIGINT), 'missing', '{}'),
        (CAST(4294967297 AS BIGINT), 'null', '{\"plugin_only\":null}'),
        (CAST(4294967298 AS BIGINT), 'false', '{\"plugin_only\":false}'),
        (CAST(4294967299 AS BIGINT), 'true', '{\"plugin_only\":true}'))";
    let filtered = format!(
        "{rows} SELECT name FROM rows
         WHERE (CAST(?1 AS BIGINT) IS NULL OR id=?1)
           AND (CAST(?2 AS TEXT) IS NULL OR name=?2) ORDER BY id"
    );
    for (id, name, expected) in [
        (None, None, vec!["missing", "null", "false", "true"]),
        (Some(4294967297_i64), None, vec!["null"]),
        (None, Some("false"), vec!["false"]),
        (Some(4294967297), Some("false"), vec![]),
    ] {
        let found = read.query_map(&filtered, sql_params![id, name], |r| r.get::<_, String>(0))?;
        ensure!(found == expected, "typed filters: {found:?}");
    }
    let boolean = read
        .dialect()
        .json_field("payload", "$.plugin_only", JsonType::Boolean);
    for (condition, expected) in [
        (
            format!("COALESCE({boolean}, 0) = 0"),
            vec!["missing", "null", "false"],
        ),
        (format!("{boolean} = 1"), vec!["true"]),
    ] {
        let found = read.query_map(
            &format!("{rows} SELECT name FROM rows WHERE {condition} ORDER BY id"),
            &[],
            |r| r.get::<_, String>(0),
        )?;
        ensure!(found == expected, "boolean condition: {found:?}");
    }
    Ok(())
}
