//! Primary audit adapter. The handle borrows the use case's already-open
//! transaction: it never commits or opens a second connection. SQLite's
//! immediate writer lock serializes admission. Indexed columns select records;
//! the full JSON preserves the versioned canonical hash input.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::sql_exec::{OptionalRow, SqlDialect, SqlRead, SqlWrite, sql_params};
use crate::application::audit::{self, AuditAppend, Caller, ChainHead};
use crate::domain::audit::{
    AssuranceClaim, Attestation, AuditAction, AuditRecord, Authority, Change, Correlation, Outcome,
    OutcomeStatus, Principal, Resource, Sender, SenderInput, chain,
};

/// A port on SQLite's current write transaction. Postgres deliberately has
/// no audit writer at stage zero; use `append_in` to select that deployment.
struct SqliteAudit<'a, T: SqlWrite>(pub &'a T);
impl<T: SqlWrite> AuditAppend for SqliteAudit<'_, T> {
    fn head(&mut self) -> Result<Option<ChainHead>> {
        Ok(self
            .0
            .query_row(
                "SELECT seq, hash FROM audit_records ORDER BY seq DESC LIMIT 1",
                &[],
                |row| {
                    Ok(ChainHead {
                        seq: row.get(0)?,
                        hash: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }
    fn by_key(&mut self, sender_id: &str, sender_seq: u64) -> Result<Option<AuditRecord>> {
        let seq = i64::try_from(sender_seq)?;
        let text: Option<String> = self
            .0
            .query_row(
                "SELECT record FROM audit_records WHERE sender_id=?1 AND sender_seq=?2",
                sql_params![sender_id, seq],
                |row| row.get(0),
            )
            .optional()?;
        text.map(|text| serde_json::from_str(&text).map_err(Into::into))
            .transpose()
    }
    fn insert(&mut self, record: &AuditRecord) -> Result<()> {
        self.0.execute(
            "INSERT INTO audit_records (seq,sender_id,sender_seq,hash,prev_hash,received_at,record) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            sql_params![i64::try_from(record.seq)?, record.sender_id, i64::try_from(record.sender_seq)?, record.hash, record.prev_hash, record.received_at, serde_json::to_string(record)?],
        )?;
        Ok(())
    }
}

/// Trusted control-side call, never deserialized from request params. Sender
/// and instance are selected by the component handling the authenticated call.
/// Postgres writes no audit at stage zero and returns `None` explicitly.
/// A retry returns the original record, including its admission timestamp.
pub fn append_in(
    tx: &impl SqlWrite,
    input: SenderInput,
    caller: Caller,
    received_at: String,
) -> Result<Option<AuditRecord>> {
    if tx.dialect() != SqlDialect::Sqlite {
        return Ok(None);
    }
    anyhow::ensure!(
        tx.in_write_transaction(),
        "audit append requires the state write transaction"
    );
    ensure_started(tx)?;
    let key = (
        format!("{}:{}", caller.sender, caller.sender_instance),
        input.sender_seq,
    );
    let mut store = SqliteAudit(tx);
    audit::append(&mut store, input, caller, received_at)?;
    store.by_key(&key.0, key.1)
}

/// The queue service's repository rebind, called by the store before it
/// commits the binding. The sender identity is this control-side component's,
/// never a request value. Its durable counter is selected under the same
/// writer lock as the state change and the chain append. Postgres records
/// nothing at stage zero.
pub(super) fn record_repository_binding(
    tx: &impl SqlWrite,
    actor: &crate::domain::ActorContext,
    before: Option<&str>,
    after: &str,
    at: String,
) -> Result<()> {
    if tx.dialect() != SqlDialect::Sqlite {
        return Ok(());
    }
    let sender_seq: u64 = tx.query_row(
        "SELECT COALESCE(MAX(sender_seq),0)+1 FROM audit_records WHERE sender_id=?1",
        sql_params!["queue_service:control"],
        |row| row.get(0),
    )?;
    let caller = Caller {
        sender: Sender::QueueService,
        sender_instance: "control".into(),
        principal: Principal {
            role: actor.role().as_str().to_owned(),
            id: actor.actor_id().to_owned(),
        },
        on_behalf_of: Vec::new(),
        auth: "control_side".into(),
        assurance: AssuranceClaim::Chained(Attestation::Enforced),
        evidence: Default::default(),
        clock_source: "host".into(),
    };
    append_in(
        tx,
        audit::repository_binding_change(sender_seq, before, after, at.clone()),
        caller,
        at,
    )?;
    Ok(())
}

/// Append facts of a control-side use case using the authenticated connection's
/// actor. The caller owns the transaction and no request may supply a principal.
pub(super) fn control(tx: &impl SqlWrite, mut input: SenderInput) -> Result<()> {
    if tx.dialect() != SqlDialect::Sqlite {
        return Ok(());
    }
    let principal = tx.query_row("SELECT dagq_actor_role(), dagq_actor_id()", &[], |r| {
        Ok(Principal {
            role: r.get(0)?,
            id: r.get(1)?,
        })
    })?;
    input.sender_seq = tx.query_row("SELECT COALESCE(MAX(sender_seq),0)+1 FROM audit_records WHERE sender_id='queue_service:decisions'", &[], |r| r.get(0))?;
    let at = crate::application::timestamp(std::time::SystemTime::now());
    input.at = at.clone();
    append_in(
        tx,
        input,
        Caller {
            sender: Sender::QueueService,
            sender_instance: "decisions".into(),
            principal,
            on_behalf_of: Vec::new(),
            auth: "control_side".into(),
            assurance: AssuranceClaim::Chained(Attestation::Enforced),
            evidence: Default::default(),
            clock_source: "host".into(),
        },
        at,
    )?;
    Ok(())
}

pub(super) use crate::application::audit::count_refusal_failure;
pub use crate::application::audit::lost_refusals;

fn startup_input(at: String, config: &[u8]) -> SenderInput {
    SenderInput {
        sender_seq: 1,
        at,
        action: AuditAction::ServiceLifecycle,
        resource: Resource {
            kind: "audit".into(),
            id: "primary".into(),
        },
        authority: Authority::default(),
        outcome: Outcome {
            status: OutcomeStatus::Succeeded,
            reason: None,
        },
        change: Some(Change {
            after: Some(json!({
                "operation": "audit_started", "dagq_version": crate::VERSION,
                "config_hash": super::queue_service::sha256_hex(config),
            })),
            ..Change::default()
        }),
        correlation: Correlation::default(),
        answered_via: None,
        landing: None,
        external: None,
    }
}

/// Runs before the first fenced state write, inside that transaction. An
/// empty chain gets one start record, without reading or replaying events.
/// The hash covers the bound repository's dagq.toml bytes (empty if absent).
/// Old schemas remain usable by migration and compatibility inspection.
pub(super) fn ensure_started(tx: &impl SqlWrite) -> Result<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='audit_records')",
        &[],
        |row| row.get(0),
    )?;
    if !exists || SqliteAudit(tx).head()?.is_some() {
        return Ok(());
    }
    let binding: Option<String> = tx
        .query_row(
            "SELECT git_common_dir FROM queue_repository LIMIT 1",
            &[],
            |row| row.get(0),
        )
        .optional()?;
    let config = match binding.and_then(|p| {
        std::path::PathBuf::from(p)
            .parent()
            .map(|p| p.join("dagq.toml"))
    }) {
        Some(path) => match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("read audit startup config {}", path.display()));
            }
        },
        None => Vec::new(),
    };
    let at = crate::application::timestamp(std::time::SystemTime::now());
    let caller = Caller {
        sender: Sender::QueueService,
        sender_instance: "audit-start".into(),
        principal: Principal {
            role: "supervisor".into(),
            id: "queue-service".into(),
        },
        on_behalf_of: Vec::new(),
        auth: "control_side".into(),
        assurance: AssuranceClaim::Chained(Attestation::Enforced),
        evidence: Default::default(),
        clock_source: "host".into(),
    };
    audit::append(
        &mut SqliteAudit(tx),
        startup_input(at.clone(), &config),
        caller,
        at,
    )?;
    Ok(())
}

/// Verify one snapshot. `since` is an inclusive sequence; its immediate
/// predecessor anchors the range (not an independently trusted checkpoint).
/// Indexed metadata is checked against the hashed JSON too, so editing only
/// a selected column cannot silently alter verification. No anchor can detect
/// truncation of the tail without a separately retained checkpoint.
pub fn verify(tx: &impl SqlRead, since: Option<u64>) -> Result<Value> {
    if tx.dialect() != SqlDialect::Sqlite {
        bail!("audit is not recorded on Postgres at stage zero");
    }
    let since = since.unwrap_or(1).max(1);
    let rows = tx.query_map(
        "SELECT seq,sender_id,sender_seq,hash,prev_hash,received_at,record FROM audit_records WHERE seq>=?1 ORDER BY seq",
        sql_params![i64::try_from(since - 1)?], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?, r.get::<_, u64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?)),
    )?;
    let mut anchor = chain::ChainAnchor::genesis();
    if since > 1 {
        let prev = rows
            .iter()
            .find(|r| r.0 == since - 1)
            .context("audit range has no predecessor")?;
        anchor.next_seq = since;
        anchor.prev_hash = prev.3.clone();
        anchor.from_genesis = false;
    }
    let mut checked = 0;
    for (seq, sender, sender_seq, hash, prev_hash, at, text) in
        rows.into_iter().filter(|r| r.0 >= since)
    {
        let record: AuditRecord = match serde_json::from_str(&text) {
            Ok(record) => record,
            Err(_) => return Ok(json!({"audit_verified": false, "seq": seq, "kind": "record"})),
        };
        if record.seq != seq
            || record.sender_id != sender
            || record.sender_seq != sender_seq
            || record.hash != hash
            || record.prev_hash != prev_hash
            || record.received_at != at
        {
            return Ok(json!({"audit_verified": false, "seq": seq, "kind": "metadata"}));
        }
        match chain::verify(anchor, &[record]) {
            Ok(next) => anchor = next,
            Err(e) => {
                return Ok(
                    json!({"audit_verified": false, "seq": e.seq, "kind": format!("{:?}", e.kind)}),
                );
            }
        }
        checked += 1;
    }
    Ok(json!({"audit_verified": true, "checked": checked, "next_seq": anchor.next_seq}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::sqlite::SqliteQueue;

    fn caller(sender: Sender) -> Caller {
        Caller {
            sender,
            sender_instance: "test".into(),
            principal: Principal {
                role: "user".into(),
                id: "user".into(),
            },
            on_behalf_of: Vec::new(),
            auth: "socket_peer".into(),
            assurance: AssuranceClaim::Chained(Attestation::Enforced),
            evidence: Default::default(),
            clock_source: "host".into(),
        }
    }
    fn input(n: u64) -> SenderInput {
        SenderInput {
            sender_seq: n,
            ..startup_input("2026-11-13T00:00:00Z".into(), b"config")
        }
    }
    fn fixture() -> (tempfile::TempDir, SqliteQueue) {
        let dir = tempfile::tempdir().unwrap();
        let q = SqliteQueue::init(dir.path().join("queue.db")).unwrap();
        (dir, q)
    }

    struct BindingClock;
    impl crate::application::Clock for BindingClock {
        fn system_time(&self) -> std::time::SystemTime {
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)
        }
        fn monotonic(&self) -> std::time::Instant {
            panic!("repository rebinding uses only the fixed wall clock")
        }
    }

    #[derive(Default)]
    struct Processes(std::cell::RefCell<std::collections::HashSet<u32>>);
    impl crate::application::ProcessControl for Processes {
        fn alive(&self, pid: u32) -> bool {
            self.0.borrow().contains(&pid)
        }
        fn terminate(&self, pid: u32) -> Result<()> {
            self.0.borrow_mut().remove(&pid);
            Ok(())
        }
        fn interrupt(&self, pid: u32) -> Result<()> {
            self.terminate(pid)
        }
        fn kill(&self, pid: u32) -> Result<()> {
            self.terminate(pid)
        }
    }

    fn rebind_use_case(
        queue: &mut SqliteQueue,
        dir: &std::path::Path,
        binding: &str,
    ) -> Result<Value> {
        crate::application::rebind::rebind(
            crate::application::rebind::Rebind {
                queue,
                files: &crate::application::memory_files::MemoryFiles::default(),
                processes: &Processes::default(),
                clock: &BindingClock,
            },
            crate::application::rebind::RebindTarget {
                db: dir.join("queue.db"),
                common_dir: binding.into(),
                queue_dir: dir.to_owned(),
                log_dir: dir.join("logs"),
                repository_file: dir.join("repository"),
                repository_queue_dir: None,
            },
        )
    }

    #[test]
    fn rebind_use_case_commits_binding_and_audit_together_and_rolls_back_on_audit_failure() {
        let (dir, queue) = fixture();
        let generators = crate::application::Generators {
            clock: std::sync::Arc::new(BindingClock),
            ..queue.generators.clone()
        };
        let mut queue =
            queue
                .with_generators(generators)
                .with_actor(crate::domain::ActorContext::new(
                    crate::domain::ActorRole::User,
                    "person-a",
                ));
        let old = dir.path().join("old/.git").to_string_lossy().into_owned();
        let new = dir.path().join("new/.git").to_string_lossy().into_owned();
        let last = dir.path().join("last/.git").to_string_lossy().into_owned();
        queue
            .conn
            .execute(
                "INSERT INTO queue_repository (singleton,git_common_dir) VALUES (1,?1)",
                [&old],
            )
            .unwrap();
        queue.conn.execute_batch("CREATE TRIGGER audit_binding_failure AFTER INSERT ON audit_records WHEN NEW.sender_id='queue_service:control' BEGIN SELECT RAISE(ABORT, 'audit failed'); END;").unwrap();
        let error = rebind_use_case(&mut queue, dir.path(), &new).unwrap_err();
        assert!(format!("{error:#}").contains("audit failed"));
        assert_eq!(
            queue.repository_binding().unwrap().as_deref(),
            Some(old.as_str())
        );
        assert_eq!(
            queue
                .conn
                .query_row("SELECT count(*) FROM audit_records", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        queue
            .conn
            .execute_batch("DROP TRIGGER audit_binding_failure")
            .unwrap();
        for target in [&new, &last] {
            let answer = rebind_use_case(&mut queue, dir.path(), target).unwrap();
            assert_eq!(answer["outcome"], "rebound");
            assert_eq!(
                queue.repository_binding().unwrap().as_deref(),
                Some(target.as_str())
            );
        }
        let records: Vec<AuditRecord> = queue
            .conn
            .prepare("SELECT record FROM audit_records ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
            .collect();
        assert_eq!(
            records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        for (index, (before, after)) in [(&old, &new), (&new, &last)].into_iter().enumerate() {
            let record = &records[index + 1];
            assert_eq!(record.prev_hash, records[index].hash);
            assert_eq!(record.sender_id, "queue_service:control");
            assert_eq!(record.sender_seq, (index + 1) as u64);
            assert_eq!(
                record.principal,
                Principal {
                    role: "user".into(),
                    id: "person-a".into()
                }
            );
            assert_eq!(record.action, AuditAction::AuthzPrivileged);
            assert_eq!(record.authority.capability.as_deref(), Some("queue.admin"));
            assert_eq!(record.at, "2023-11-14T22:13:20.000Z");
            let change = record.change.as_ref().unwrap();
            assert_eq!(change.before, Some(json!({"git_common_dir": before})));
            assert_eq!(change.after, Some(json!({"git_common_dir": after})));
        }
        assert_eq!(
            verify(&queue.read_tx("rebind_verify").unwrap(), None).unwrap()["checked"],
            3
        );
    }

    #[test]
    fn sqlite_rejects_updates_and_deletes() {
        let (_dir, q) = fixture();
        q.write_tx("start_audit").unwrap().commit().unwrap();
        for sql in [
            "UPDATE audit_records SET hash='bad'",
            "DELETE FROM audit_records",
        ] {
            assert!(
                q.conn
                    .execute(sql, [])
                    .unwrap_err()
                    .to_string()
                    .contains("append only")
            );
        }
        assert_eq!(
            q.conn
                .query_row("SELECT count(*) FROM audit_records", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn state_and_audit_commit_or_rollback_together_and_retry_returns_original() {
        let (_dir, q) = fixture();
        q.conn
            .execute_batch("CREATE TABLE audit_test_state (value TEXT)")
            .unwrap();
        {
            let tx = q.write_tx("failed_audited_change").unwrap();
            tx.execute("INSERT INTO audit_test_state VALUES ('rollback')", [])
                .unwrap();
            append_in(&tx, input(1), caller(Sender::QueueService), "first".into()).unwrap();
            // A failed state write after admission rolls everything back.
            assert!(
                tx.execute("INSERT INTO absent_table VALUES (1)", [])
                    .is_err()
            );
        }
        assert_eq!(
            q.conn
                .query_row("SELECT count(*) FROM audit_test_state", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            q.conn
                .query_row("SELECT count(*) FROM audit_records", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        {
            let tx = q.write_tx("successful_audited_change").unwrap();
            tx.execute("INSERT INTO audit_test_state VALUES ('committed')", [])
                .unwrap();
            let a = append_in(&tx, input(1), caller(Sender::QueueService), "first".into())
                .unwrap()
                .unwrap();
            let b = append_in(&tx, input(2), caller(Sender::QueueService), "second".into())
                .unwrap()
                .unwrap();
            assert_eq!((a.seq, b.seq), (2, 3));
            assert_eq!(b.prev_hash, a.hash);
            let retry = append_in(&tx, input(1), caller(Sender::QueueService), "retry".into())
                .unwrap()
                .unwrap();
            assert_eq!(retry, a);
            tx.commit().unwrap();
        }
        assert_eq!(
            q.conn
                .query_row("SELECT value FROM audit_test_state", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "committed"
        );
        let tx = q.read_tx("verify").unwrap();
        assert_eq!(verify(&tx, None).unwrap()["checked"], 3);
    }

    #[test]
    fn audit_failure_rolls_back_state_too() {
        let (_dir, q) = fixture();
        q.write_tx("start").unwrap().commit().unwrap();
        q.conn.execute_batch("CREATE TABLE audit_test_state (value TEXT); CREATE TRIGGER audit_test_failure BEFORE INSERT ON audit_records BEGIN SELECT RAISE(ABORT, 'audit failed'); END;").unwrap();
        {
            let tx = q.write_tx("audit_failure").unwrap();
            tx.execute("INSERT INTO audit_test_state VALUES ('rollback')", [])
                .unwrap();
            assert!(append_in(&tx, input(1), caller(Sender::QueueService), "now".into()).is_err());
        }
        assert_eq!(
            q.conn
                .query_row("SELECT count(*) FROM audit_test_state", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            q.conn
                .query_row("SELECT count(*) FROM audit_records", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn start_is_once_only_without_backfilling_events() {
        let (dir, q) = fixture();
        std::fs::write(dir.path().join("dagq.toml"), b"config").unwrap();
        q.conn
            .execute(
                "INSERT INTO queue_repository (singleton,git_common_dir) VALUES (1,?1)",
                [dir.path().join(".git").to_str().unwrap()],
            )
            .unwrap();
        q.conn
            .execute(
                "INSERT INTO run_events (kind,payload) VALUES ('old_event','{}')",
                [],
            )
            .unwrap();
        let before: i64 = q
            .conn
            .query_row("SELECT count(*) FROM run_events", [], |r| r.get(0))
            .unwrap();
        for _ in 0..2 {
            q.write_tx("start").unwrap().commit().unwrap();
        }
        let text: String = q
            .conn
            .query_row("SELECT record FROM audit_records", [], |r| r.get(0))
            .unwrap();
        let record: AuditRecord = serde_json::from_str(&text).unwrap();
        let after = record.change.unwrap().after.unwrap();
        assert_eq!(after["operation"], "audit_started");
        assert_eq!(after["dagq_version"], crate::VERSION);
        assert_eq!(
            after["config_hash"],
            "b79606fb3afea5bd1609ed40b622142f1c98125abcfe89a76a661b0e8e343910"
        );
        assert_eq!(
            q.conn
                .query_row("SELECT count(*) FROM audit_records", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            q.conn
                .query_row("SELECT count(*) FROM run_events", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            before
        );
    }

    #[test]
    fn verify_reports_first_tamper_and_can_anchor_a_range() {
        let (_dir, q) = fixture();
        {
            let tx = q.write_tx("records").unwrap();
            for n in 1..=3 {
                append_in(&tx, input(n), caller(Sender::QueueService), "now".into()).unwrap();
            }
            tx.commit().unwrap();
        }
        assert_eq!(
            verify(&q.read_tx("verify").unwrap(), None).unwrap()["audit_verified"],
            true
        );
        assert_eq!(
            verify(&q.read_tx("range").unwrap(), Some(3)).unwrap()["checked"],
            2
        );
        q.conn
            .execute_batch("DROP TRIGGER audit_records_no_update")
            .unwrap();
        // Simulate a writer outside the service, bypassing the trigger.
        let text: String = q
            .conn
            .query_row("SELECT record FROM audit_records WHERE seq=2", [], |r| {
                r.get(0)
            })
            .unwrap();
        let mut record: AuditRecord = serde_json::from_str(&text).unwrap();
        record.principal.id = "tampered".into();
        q.conn
            .execute(
                "UPDATE audit_records SET record=?1 WHERE seq IN (2,3)",
                [serde_json::to_string(&record).unwrap()],
            )
            .unwrap();
        let result = verify(&q.read_tx("tamper").unwrap(), None).unwrap();
        assert_eq!(result["audit_verified"], false);
        assert_eq!(result["seq"], 2);
        assert_eq!(result["kind"], "Hash");
    }
}
