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

/// The dialect's set of a field adds to an object and leaves any other document
/// as it is: a row whose body went to the port holds its key, a JSON string.
pub fn json_set_field<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    use serde_json::{Value, json};
    let store = open()?;
    let tx = store.read_entry("contract_json_set")?;
    let dialect = tx.dialect();
    let sql = format!(
        "SELECT {}",
        dialect.json_set_field("?1", "?2", &dialect.json_document("?3"))
    );
    for (document, expected) in [
        (json!({"a": 1}), json!({"a": 1, "b": [2]})),
        (json!({"b": 1}), json!({"b": [2]})),
        (json!("object-key"), json!("object-key")),
        (json!(7), json!(7)),
        (json!([1]), json!([1])),
    ] {
        let set = tx.query_row(
            &sql,
            sql_params![document.to_string(), "$.b".to_owned(), "[2]".to_owned()],
            |r| r.get::<_, String>(0),
        )?;
        ensure!(
            serde_json::from_str::<Value>(&set)? == expected,
            "{document} became {set}"
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

/// The backend capacity aggregate reads as an optional integer through the
/// execution port, including an empty set and the supervisor token filter.
pub fn backend_parallel<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    use crate::infrastructure::runtime_store::BACKEND_PARALLEL_SQL;

    let store = open()?;
    let tx = store.write_entry("contract_backend_parallel")?;
    let parallel = |token: Option<&str>| {
        tx.query_row(BACKEND_PARALLEL_SQL, sql_params![token], |r| {
            r.get::<_, Option<i64>>(0)
        })
    };
    ensure!(parallel(None)?.is_none());
    for (token, slots) in [("contract-backend-a", 3_i64), ("contract-backend-b", 5)] {
        tx.execute(
            "INSERT INTO supervisors(token, pid, parallel, binary_version, started_at, heartbeat_at) VALUES (?1, 1, ?2, 'contract', 0, 0)",
            sql_params![token, slots],
        )?;
    }
    ensure!(parallel(None)? == Some(8));
    ensure!(parallel(Some("contract-backend-a"))? == Some(3));
    ensure!(parallel(Some("contract-backend-missing"))?.is_none());
    Ok(())
}

/// Registry CRUD, uniqueness of the identity pair, and disabled rows survive
/// reopening on both real adapters. No authentication decisions live here.
pub fn members<
    S: ContractStore + crate::application::queue_service::MemberStore,
    F: Fn() -> Result<S>,
>(
    open: &F,
) -> Result<()> {
    use crate::domain::{
        oidc::OidcIdentity,
        queue_service::{Member, TeamRole},
    };
    let store = open()?;
    let identity = OidcIdentity {
        issuer: "https://contract.example".into(),
        subject: "person".into(),
    };
    let member = Member {
        identity: identity.clone(),
        role: TeamRole::Admin,
        on_call: true,
        display_name: Some("Registry Name".into()),
        created_at: 100,
        disabled_at: None,
    };
    ensure!(store.member(&identity)?.is_none());
    store.add_member(&member)?;
    ensure!(open()?.member(&identity)? == Some(member.clone()));
    ensure!(store.add_member(&member).is_err());
    let other = Member {
        identity: OidcIdentity {
            issuer: "https://other.example".into(),
            ..identity.clone()
        },
        role: TeamRole::Member,
        on_call: false,
        display_name: None,
        ..member.clone()
    };
    store.add_member(&other)?;
    ensure!(open()?.member(&other.identity)? == Some(other.clone()));
    ensure!(store.disable_member(&identity, 200)?);
    ensure!(!store.disable_member(&identity, 201)?);
    ensure!(
        open()?.member(&identity)?
            == Some(Member {
                disabled_at: Some(200),
                ..member
            })
    );
    ensure!(open()?.member(&other.identity)? == Some(other));
    let tx = store.write_entry("contract_members_cleanup")?;
    tx.execute(
        "DELETE FROM members WHERE subject = ?1",
        sql_params!["person"],
    )?;
    S::commit(tx)?;
    Ok(())
}

/// Code consumption, runner creation and rotation survive reopening together;
/// failed replacement writes roll back consumption. Replay commits revocation.
pub fn runner_credentials<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    use crate::domain::{
        oidc::OidcIdentity,
        queue_service::{Member, TeamRole},
        runner_registration::{Change, Outcome},
    };
    let store = open()?;
    let owner = OidcIdentity {
        issuer: "https://runner-contract.example".into(),
        subject: "runner-owner".into(),
    };
    store.add_member(&Member {
        identity: owner.clone(),
        role: TeamRole::Member,
        on_call: false,
        display_name: None,
        created_at: 0,
        disabled_at: None,
    })?;
    let code = Change::Code {
        owner: owner.clone(),
        hash: "code-hash".into(),
    };
    ensure!(store.change_runner(&code, 100)? == Outcome::Code);
    let enroll = Change::Enroll {
        hash: "code-hash".into(),
        runner_id: "contract-runner".into(),
        refresh: "refresh-hash".into(),
        token: "token-hash".into(),
    };
    ensure!(store.change_runner(&enroll, 101)? == Outcome::Issued("contract-runner".into()));
    ensure!(open()?.change_runner(&enroll, 102)? == Outcome::Unauthenticated);
    ensure!(open()?.runner_token("token-hash", 102)?.is_some());
    // Declaration replacement persists independently of registry approval.
    let declaration = crate::domain::runner_registration::Declaration {
        os: "linux".into(),
        arch: "aarch64".into(),
        container: true,
        credentials: vec!["team_api_key".into()],
    };
    let declare = |declaration| Change::Declare {
        runner_id: "contract-runner".into(),
        declaration,
    };
    ensure!(store.change_runner(&declare(declaration.clone()), 102)? == Outcome::Updated);
    let admin = OidcIdentity {
        subject: "runner-admin".into(),
        ..owner.clone()
    };
    store.add_member(&Member {
        identity: admin.clone(),
        role: TeamRole::Admin,
        on_call: false,
        display_name: None,
        created_at: 0,
        disabled_at: None,
    })?;
    let share = |caller, shared| Change::Share {
        caller,
        runner_id: "contract-runner".into(),
        shared,
    };
    ensure!(store.change_runner(&share(owner.clone(), true), 102)? == Outcome::Denied);
    ensure!(store.change_runner(&share(admin.clone(), true), 102)? == Outcome::Updated);
    let replacement = crate::domain::runner_registration::Declaration {
        container: false,
        ..declaration
    };
    ensure!(open()?.change_runner(&declare(replacement.clone()), 102)? == Outcome::Updated);
    let stored = open()?.runner_token("token-hash", 102)?.unwrap();
    ensure!(stored.declaration == Some(replacement));
    ensure!(stored.shared && stored.approved_by == Some(admin.clone()));
    ensure!(store.change_runner(&share(admin.clone(), true), 102)? == Outcome::Denied);
    if let Outcome::Listed(rows) = store.change_runner(
        &Change::List {
            caller: owner.clone(),
        },
        102,
    )? {
        ensure!(rows.len() == 1 && rows[0] == stored);
    } else {
        anyhow::bail!("owner listing refused");
    }
    ensure!(store.change_runner(&share(admin.clone(), false), 102)? == Outcome::Updated);
    ensure!(!open()?.runner_token("token-hash", 102)?.unwrap().shared);
    // Standby runner receives no refresh credential and never starts shared.
    let github = Change::Github {
        owner: owner.clone(),
        runner_id: "actions-runner".into(),
        token: "actions-token-hash".into(),
    };
    ensure!(store.change_runner(&github, 102)? == Outcome::Issued("actions-runner".into()));
    let actions = open()?.runner_token("actions-token-hash", 102)?.unwrap();
    ensure!(actions.owner == owner && !actions.shared && actions.approved_by.is_none());
    let tx = store.read_entry("actions_no_refresh")?;
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM runner_refresh WHERE runner_id = ?1",
        sql_params!["actions-runner"],
        |r| r.get(0),
    )?;
    ensure!(count == 0);
    drop(tx);
    if let Outcome::Listed(rows) = store.change_runner(
        &Change::List {
            caller: admin.clone(),
        },
        102,
    )? {
        ensure!(rows.len() == 2);
    } else {
        anyhow::bail!("admin listing refused");
    }
    if let Outcome::Listed(rows) = store.change_runner(
        &Change::List {
            caller: owner.clone(),
        },
        102,
    )? {
        ensure!(rows.len() == 2);
    } else {
        anyhow::bail!("owner listing refused");
    }
    ensure!(
        store.change_runner(
            &Change::Github {
                owner: OidcIdentity {
                    subject: "absent".into(),
                    ..owner.clone()
                },
                runner_id: "absent-runner".into(),
                token: "absent-token".into()
            },
            102
        )? == Outcome::Unauthenticated
    );
    ensure!(
        store.change_runner(
            &Change::Github {
                owner: admin.clone(),
                runner_id: "admin-runner".into(),
                token: "admin-token".into()
            },
            102
        )? == Outcome::Issued("admin-runner".into())
    );
    if let Outcome::Listed(rows) = store.change_runner(
        &Change::List {
            caller: owner.clone(),
        },
        102,
    )? {
        ensure!(rows.len() == 2 && rows.iter().all(|r| r.owner == owner));
    } else {
        anyhow::bail!("owner listing refused");
    }
    if let Outcome::Listed(rows) = store.change_runner(
        &Change::List {
            caller: admin.clone(),
        },
        102,
    )? {
        ensure!(rows.len() == 3);
    } else {
        anyhow::bail!("admin listing refused");
    }
    // Duplicate hash fails after consumption: transaction rollback leaves the
    // original refresh current and creates no replacement.
    let failed = Change::Refresh {
        hash: "refresh-hash".into(),
        refresh: "refresh-hash".into(),
        token: "unused-token".into(),
    };
    ensure!(store.change_runner(&failed, 102).is_err());
    ensure!(open()?.runner_token("unused-token", 103)?.is_none());
    let rotate = Change::Refresh {
        hash: "refresh-hash".into(),
        refresh: "next-refresh-hash".into(),
        token: "next-token-hash".into(),
    };
    ensure!(open()?.change_runner(&rotate, 103)? == Outcome::Issued("contract-runner".into()));
    ensure!(open()?.runner_token("next-token-hash", 104)?.is_some());
    ensure!(store.change_runner(&rotate, 104)? == Outcome::Unauthenticated);
    for token in ["token-hash", "next-token-hash"] {
        ensure!(open()?.runner_token(token, 105)?.is_none());
    }
    ensure!(
        store.change_runner(
            &Change::Refresh {
                hash: "next-refresh-hash".into(),
                refresh: "never".into(),
                token: "never".into()
            },
            105
        )? == Outcome::Unauthenticated
    );
    // A failed enroll must restore its code, as well as omit its runner row.
    store.change_runner(
        &Change::Code {
            owner: owner.clone(),
            hash: "second-code".into(),
        },
        200,
    )?;
    let second = |refresh: &str| Change::Enroll {
        hash: "second-code".into(),
        runner_id: "second-runner".into(),
        refresh: refresh.into(),
        token: "second-token".into(),
    };
    ensure!(store.change_runner(&second("refresh-hash"), 201).is_err());
    ensure!(
        open()?.change_runner(&second("second-refresh"), 202)?
            == Outcome::Issued("second-runner".into())
    );
    store.disable_member(&owner, 203)?;
    ensure!(
        store.change_runner(
            &Change::Github {
                owner: owner.clone(),
                runner_id: "disabled-runner".into(),
                token: "disabled-token".into()
            },
            203
        )? == Outcome::Unauthenticated
    );

    ensure!(open()?.runner_token("second-token", 203)?.is_none());
    ensure!(
        store.change_runner(
            &Change::Refresh {
                hash: "second-refresh".into(),
                refresh: "disabled-refresh".into(),
                token: "disabled-token".into()
            },
            203
        )? == Outcome::Unauthenticated
    );

    // A persisted revocation refuses the very next token lookup and refresh.
    store.change_runner(
        &Change::Code {
            owner: owner.clone(),
            hash: "third-code".into(),
        },
        200,
    )?;
    // Re-enable only within this store fixture; runtime mutations use ports.
    {
        let tx = store.write_entry("contract_owner_restore")?;
        tx.execute(
            "UPDATE members SET disabled_at = NULL WHERE issuer = ?1 AND subject = ?2",
            sql_params![owner.issuer, owner.subject],
        )?;
        S::commit(tx)?;
    }
    store.change_runner(
        &Change::Code {
            owner: owner.clone(),
            hash: "third-code".into(),
        },
        200,
    )?;
    ensure!(
        store.change_runner(
            &Change::Enroll {
                hash: "third-code".into(),
                runner_id: "third-runner".into(),
                refresh: "third-refresh".into(),
                token: "third-token".into()
            },
            201
        )? == Outcome::Issued("third-runner".into())
    );
    let other = OidcIdentity {
        subject: "other".into(),
        ..owner.clone()
    };
    store.add_member(&Member {
        identity: other.clone(),
        role: TeamRole::Member,
        on_call: false,
        display_name: None,
        created_at: 0,
        disabled_at: None,
    })?;
    ensure!(
        store.change_runner(
            &Change::Revoke {
                caller: other,
                runner_id: "third-runner".into()
            },
            202
        )? == Outcome::Denied
    );
    ensure!(open()?.runner_token("third-token", 202)?.is_some());
    ensure!(
        store.change_runner(
            &Change::Revoke {
                caller: owner.clone(),
                runner_id: "third-runner".into()
            },
            203
        )? == Outcome::Revoked
    );
    ensure!(open()?.runner_token("third-token", 203)?.is_none());
    ensure!(
        store.change_runner(
            &Change::Refresh {
                hash: "third-refresh".into(),
                refresh: "revoked-refresh".into(),
                token: "revoked-token".into()
            },
            203
        )? == Outcome::Unauthenticated
    );
    let tx = store.write_entry("contract_runner_cleanup")?;
    for table in ["runner_tokens", "runner_refresh", "runner_codes", "runners"] {
        tx.execute(&format!("DELETE FROM {table}"), sql_params![])?;
    }
    tx.execute(
        "DELETE FROM members WHERE issuer = ?1",
        sql_params![owner.issuer],
    )?;
    S::commit(tx)?;
    Ok(())
}
