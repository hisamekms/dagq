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
    let evidence: Option<String> = tx.query_row("SELECT dagq_direct_human()", &[], |r| r.get(0))?;
    let mut assurance_evidence = std::collections::BTreeMap::new();
    if let Some(evidence) = evidence {
        assurance_evidence.insert("direct_human".into(), evidence);
        if input.action == AuditAction::AuthzPrivileged {
            input.answered_via = Some(crate::domain::audit::AnsweredVia::HumanAttested);
        }
    }
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
            evidence: assurance_evidence,
            clock_source: "host".into(),
        },
        at,
    )?;
    Ok(())
}

pub(super) use crate::application::audit::count_refusal_failure;
pub use crate::application::audit::lost_refusals;

/// The landing's facts (F, ADR-t4840-2 decisions 7 and 8), written by the
/// store in the transaction of the run event that states them: a judgment
/// that lets the run land (`landing.judged` of a review's, `landing.approved`
/// of a person's or the inbox's, with the set of blob hashes, the diff hash
/// and the tree hash it is bound to and, as the change, the binding of the
/// run's judgment before it), the comparison of the merged result with it
/// (`landing.compared`, with what changed the run's branch since) and the
/// merge (`landing.merged`). The audit record is no copy of the event: the
/// use case states both facts at once, and a record that cannot be written
/// leaves the event unwritten too. Any other event writes none, nor does
/// Postgres at stage zero.
pub(super) fn record_landing(
    tx: &impl SqlWrite,
    event: crate::domain::EventId,
    task_id: crate::domain::TaskId,
    run_id: &crate::domain::RunId,
    kind: crate::domain::EventKind,
    payload: &Value,
) -> Result<()> {
    use crate::domain::EventKind as K;
    use crate::domain::landing_binding::{BINDING_KEY, binding_of};
    if tx.dialect() != SqlDialect::Sqlite {
        return Ok(());
    }
    let (role, id, requested_by): (Option<String>, Option<String>, Option<String>) = tx.query_row(
        "SELECT actor_role, actor_id, requested_by FROM run_events WHERE id=?1",
        sql_params![event.as_i64()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let actor = crate::domain::RunEvent {
        id: event,
        task_id: Some(task_id),
        goal_id: None,
        run_id: Some(run_id.clone()),
        kind: kind.as_str().into(),
        payload: payload.clone(),
        created_at: String::new(),
        actor: role
            .clone()
            .zip(id.clone())
            .map(|(role, id)| crate::domain::EventActor {
                role,
                id,
                requested_by: requested_by.clone(),
            }),
    };
    let (action, sender, capability, landing, change, outcome) = match kind {
        K::ReviewFinished | K::ReviseLimitJudged | K::IntegrationApproved
            if payload.get(BINDING_KEY).is_some() =>
        {
            let Some(path) = crate::domain::landing_binding::judgment_of(&actor) else {
                return Ok(());
            };
            let (action, sender, capability) = match path.reapproval() {
                crate::domain::landing_binding::Reapproval::Review => (
                    AuditAction::LandingJudged,
                    Sender::Supervisor,
                    crate::domain::authorization::Capability::Supervise,
                ),
                crate::domain::landing_binding::Reapproval::ApproveLanding => (
                    AuditAction::LandingApproved,
                    Sender::QueueService,
                    crate::domain::authorization::Capability::AskAnswer,
                ),
                crate::domain::landing_binding::Reapproval::Integrate => (
                    AuditAction::LandingApproved,
                    Sender::QueueService,
                    crate::domain::authorization::Capability::IntegrationRequest,
                ),
            };
            let facts = binding_of(payload);
            let before = previous_binding(tx, run_id, event)?;
            let change = Change {
                before: before.map(|facts| binding_change(&facts, None)),
                after: facts
                    .as_ref()
                    .map(|facts| binding_change(facts, Some(path.as_str()))),
                ..Change::default()
            };
            (
                action,
                sender,
                capability,
                facts,
                Some(change),
                Outcome {
                    status: OutcomeStatus::Succeeded,
                    reason: None,
                },
            )
        }
        K::IntegrationCompared => {
            let landed = payload
                .get("landed")
                .and_then(|value| serde_json::from_value(value.clone()).ok());
            let matched = payload["matched"] == true;
            let outcome = if matched {
                Outcome {
                    status: OutcomeStatus::Succeeded,
                    reason: None,
                }
            } else {
                Outcome {
                    status: OutcomeStatus::Denied,
                    reason: Some(format!(
                        "the landed files differ from the judgment: {}",
                        payload["differing"]
                    )),
                }
            };
            let change = Change {
                before: Some(
                    json!({"judgment": payload["judgment"], "path": payload["path"], "binding": payload["judged"], "head": payload["head_before"]}),
                ),
                after: Some(
                    json!({"binding": payload["landed"], "head": payload["head_after"], "rebased": payload["rebased"], "renumbered": payload["renumbered"], "conflict_resolved": payload["conflict_resolved"], "differing": payload["differing"], "reapproval": payload["reapproval"]}),
                ),
                ..Change::default()
            };
            (
                AuditAction::LandingCompared,
                Sender::Integrate,
                crate::domain::authorization::Capability::Land,
                landed,
                Some(change),
                outcome,
            )
        }
        // A rebase, a renumbering, a conflict's resolution (by the live
        // session, a resume or a skipped resume), a resume's fix or a revise
        // after the judgment: its own change, with each side's head and
        // binding (its tree hash and set).
        K::IntegrationRebased
        | K::MigrationRenumbered
        | K::ConflictResolved
        | K::ResumeFinished
        | K::ResumeSkipped
        | K::ReviseFinished
            if payload.get("binding_after").is_some_and(|b| !b.is_null()) =>
        {
            let after: Option<crate::domain::audit::LandingFacts> =
                serde_json::from_value(payload["binding_after"].clone()).ok();
            let change = Change {
                before: Some(
                    json!({"head": payload["head_before"], "binding": payload["binding_before"]}),
                ),
                after: Some(
                    json!({"head": payload["head_after"], "binding": payload["binding_after"], "main": payload["main"],
                    "rewrite": match kind {
                        K::IntegrationRebased => "rebase",
                        K::MigrationRenumbered => "renumber",
                        K::ConflictResolved => "conflict_resolution",
                        // A resume says what it rewrote.
                        K::ResumeFinished | K::ResumeSkipped => payload["rewrite"].as_str().unwrap_or("resume"),
                        _ => "revise",
                    }}),
                ),
                ..Change::default()
            };
            (
                AuditAction::LandingRewritten,
                Sender::Integrate,
                crate::domain::authorization::Capability::Land,
                after,
                Some(change),
                Outcome {
                    status: OutcomeStatus::Succeeded,
                    reason: None,
                },
            )
        }
        K::RunIntegrated if payload.get("merged").is_some_and(|m| !m.is_null()) => {
            let merged = &payload["merged"];
            let landed = merged
                .get("landed")
                .and_then(|value| serde_json::from_value(value.clone()).ok());
            let change = Change {
                before: Some(json!({"main": payload["main_before"]})),
                after: Some(json!({
                    "main": payload["commit"], "source_commit": payload["source_commit"],
                    "judgment": merged["judgment"], "path": merged["path"],
                    "rebased": merged["rebased"], "renumbered": merged["renumbered"],
                    "conflict_resolved": merged["conflict_resolved"],
                })),
                ..Change::default()
            };
            (
                AuditAction::LandingMerged,
                Sender::Integrate,
                crate::domain::authorization::Capability::Land,
                landed,
                Some(change),
                Outcome {
                    status: OutcomeStatus::Succeeded,
                    reason: None,
                },
            )
        }
        _ => return Ok(()),
    };
    let instance = "control";
    let sender_seq: u64 = tx.query_row(
        "SELECT COALESCE(MAX(sender_seq),0)+1 FROM audit_records WHERE sender_id=?1",
        sql_params![format!("{sender}:{instance}")],
        |row| row.get(0),
    )?;
    let principal = |role: Option<String>, id: Option<String>| Principal {
        role: role.unwrap_or_else(|| "unknown".into()),
        id: id.unwrap_or_else(|| "unknown".into()),
    };
    let mut on_behalf_of: Vec<Principal> = requested_by
        .map(|by| {
            let role = by.split(':').next().unwrap_or_default().to_owned();
            vec![principal(Some(role), Some(by))]
        })
        .unwrap_or_default();
    // A person's `land` answer the supervisor applies is the answerer's
    // approval: who answered, how it reached the queue and the ask are
    // kept, the supervisor being only who wrote it.
    let answered = (kind == K::IntegrationApproved)
        .then(|| payload["answered_by"].as_str())
        .flatten();
    if let Some(by) = answered {
        on_behalf_of.push(principal(Some(by.to_owned()), Some(by.to_owned())));
    }
    let answered_via = answered.and_then(|_| match payload["answer_authority"].as_str() {
        Some("user") => Some(crate::domain::audit::AnsweredVia::HumanAttested),
        Some("delegated") => Some(crate::domain::audit::AnsweredVia::RelayedByInbox),
        _ => None,
    });
    let mut evidence: std::collections::BTreeMap<String, String> =
        [("event".to_owned(), event.as_i64().to_string())].into();
    if let Some(ask_id) = payload["ask_id"]
        .as_i64()
        .filter(|_| kind == K::IntegrationApproved)
    {
        evidence.insert("ask".to_owned(), ask_id.to_string());
    }
    let at = crate::application::timestamp(std::time::SystemTime::now());
    let input = SenderInput {
        sender_seq,
        at: at.clone(),
        action,
        resource: Resource {
            kind: "run".into(),
            id: run_id.to_string(),
        },
        authority: Authority {
            capability: Some(capability.to_string()),
            ..Authority::default()
        },
        outcome,
        change,
        correlation: Correlation {
            task_id: Some(task_id.as_i64()),
            run_id: Some(run_id.to_string()),
            ..Correlation::default()
        },
        answered_via,
        landing,
        external: None,
    };
    let caller = Caller {
        sender,
        sender_instance: instance.into(),
        principal: principal(role, id),
        on_behalf_of,
        auth: "control_side".into(),
        assurance: AssuranceClaim::Chained(Attestation::Enforced),
        evidence,
        clock_source: "host".into(),
    };
    append_in(tx, input, caller, at)?;
    Ok(())
}

/// The binding of the run's latest judgment event before `event`.
fn previous_binding(
    tx: &impl SqlWrite,
    run_id: &crate::domain::RunId,
    event: crate::domain::EventId,
) -> Result<Option<crate::domain::audit::LandingFacts>> {
    use crate::domain::EventKind as K;
    let rows: Vec<(i64, String, String, Option<String>)> = tx.query_map(
        "SELECT id, kind, payload, actor_role FROM run_events WHERE run_id=?1 AND id<?2 AND kind IN (?3,?4,?5) ORDER BY id DESC",
        sql_params![run_id, event.as_i64(), K::ReviewFinished.as_str(), K::ReviseLimitJudged.as_str(), K::IntegrationApproved.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    Ok(rows.into_iter().find_map(|(id, kind, text, role)| {
        let payload: Value = serde_json::from_str(&text).ok()?;
        let judged = crate::domain::RunEvent {
            id: crate::domain::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: Some(run_id.clone()),
            kind: kind.as_str().into(),
            payload,
            created_at: String::new(),
            actor: role.map(|role| crate::domain::EventActor {
                role,
                id: String::new(),
                requested_by: None,
            }),
        };
        crate::domain::landing_binding::judgment_of(&judged)?;
        crate::domain::landing_binding::binding_of(&judged.payload)
    }))
}

/// A binding as a record's change shows it: the tree hash and the set
/// (and the judgment's path, for the new one).
fn binding_change(facts: &crate::domain::audit::LandingFacts, path: Option<&str>) -> Value {
    let mut value = json!({"tree_hash": facts.tree_hash, "approved_blobs": facts.approved_blobs});
    if let Some(path) = path {
        value["path"] = json!(path);
    }
    value
}

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

    /// A queue with one task's run [`RUN`] and the user as its actor.
    fn with_run() -> (tempfile::TempDir, SqliteQueue, crate::domain::RunId) {
        use crate::application::TaskStore;
        const RUN: &str = "11111111-1111-4111-8111-111111111111";
        let (dir, queue) = fixture();
        let mut queue = queue.with_actor(crate::domain::ActorContext::user());
        let task = queue
            .add(crate::domain::NewTask {
                title: "t".into(),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                priority: Default::default(),
                goal_id: None,
                context: String::new(),
                change: None,
                provider: None,
                worker_mode: None,
                wait_for_build: false,
                execution_class: Default::default(),
            })
            .unwrap()
            .id();
        queue
            .conn
            .execute_batch(&format!(
                "UPDATE tasks SET status='in_progress' WHERE id={task};
                 INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
                 VALUES ('{RUN}',{task},'awaiting_integration','claude','claude','{}');",
                "a".repeat(40),
            ))
            .unwrap();
        (dir, queue, crate::domain::RunId::new(RUN).unwrap())
    }

    fn landing_records(queue: &SqliteQueue) -> Vec<AuditRecord> {
        queue
            .conn
            .prepare("SELECT record FROM audit_records ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| serde_json::from_str::<AuditRecord>(&r.unwrap()).unwrap())
            .filter(|r| r.action.as_str().starts_with("landing."))
            .collect()
    }

    fn binding(blob: &str, tree: &str) -> Value {
        json!({"approved_blobs": {"src/a.rs": {"blob": blob}, "old.rs": "removed"}, "diff_hash": "d", "tree_hash": tree})
    }

    /// Each rewrite of the run's branch after a judgment is audited as its
    /// own change, named by what rewrote it, with each side's head and
    /// binding: the landing's rebase and renumbering, the live session's
    /// conflict resolution, a revise, and a resume's, with a session or
    /// skipped, by the rewrite it names. One with no binding after it
    /// writes none.
    #[test]
    fn each_rewrite_of_the_branch_is_audited_with_what_rewrote_it() {
        use crate::domain::EventKind as K;
        let (_dir, queue, run) = with_run();
        let record = |kind, rewrite: Option<&str>| {
            let mut payload = json!({"head_before": "h1", "head_after": "h2",
                "binding_before": binding("1", "t1"), "binding_after": binding("2", "t2")});
            if let Some(rewrite) = rewrite {
                payload["rewrite"] = json!(rewrite);
            }
            queue.record_runtime_event(&run, kind, payload).unwrap();
        };
        record(K::IntegrationRebased, None);
        record(K::MigrationRenumbered, None);
        record(K::ConflictResolved, None);
        record(K::ReviseFinished, None);
        record(K::ResumeFinished, Some("conflict_resolution"));
        record(K::ResumeSkipped, Some("conflict_resolution"));
        record(K::ResumeFinished, Some("resume"));
        record(K::ResumeSkipped, Some("resume"));
        queue
            .record_runtime_event(&run, K::ResumeFinished, json!({"outcome": "resolved"}))
            .unwrap();
        let records = landing_records(&queue);
        let rewrites: Vec<&str> = records
            .iter()
            .map(|r| {
                assert_eq!(r.action, AuditAction::LandingRewritten);
                let change = r.change.as_ref().unwrap();
                assert_eq!(change.before.as_ref().unwrap()["head"], "h1");
                assert_eq!(
                    change.before.as_ref().unwrap()["binding"]["tree_hash"],
                    "t1"
                );
                assert_eq!(change.after.as_ref().unwrap()["head"], "h2");
                assert_eq!(change.after.as_ref().unwrap()["binding"]["tree_hash"], "t2");
                change.after.as_ref().unwrap()["rewrite"].as_str().unwrap()
            })
            .collect();
        assert_eq!(
            rewrites,
            [
                "rebase",
                "renumber",
                "conflict_resolution",
                "revise",
                "conflict_resolution",
                "conflict_resolution",
                "resume",
                "resume"
            ]
        );
    }

    /// Each judgment that lets a run land is audited with the set it was
    /// bound to and, as its change, the binding of the judgment before it;
    /// the comparison is audited with both sides and what rewrote the
    /// branch (a rebase), denied when they differ; a verdict that does not
    /// let the run land, or a judgment with no binding, writes none.
    #[test]
    fn a_landings_judgments_and_comparisons_are_audited_with_the_sets_they_hold() {
        use crate::domain::EventKind as K;
        let (_dir, queue, run) = with_run();
        let record = |kind, payload| queue.record_runtime_event(&run, kind, payload).unwrap();
        record(
            K::ReviewFinished,
            json!({"verdict": "revise", "binding": binding("1", "t1")}),
        );
        record(K::ReviewFinished, json!({"verdict": "pass"}));
        assert!(landing_records(&queue).is_empty());
        record(
            K::ReviewFinished,
            json!({"verdict": "pass", "binding": binding("1", "t1")}),
        );
        record(
            K::IntegrationCompared,
            json!({
                "judgment": 3, "path": "review_pass", "matched": false, "judged": binding("1", "t1"),
                "landed": binding("2", "t2"), "head_before": "h1", "head_after": "h2", "rebased": true,
                "renumbered": false, "conflict_resolved": false, "differing": ["src/a.rs"], "reapproval": "review",
            }),
        );
        record(
            K::IntegrationApproved,
            json!({"ask_id": 4, "binding": binding("2", "t2")}),
        );
        let records = landing_records(&queue);
        let actions: Vec<&str> = records.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(
            actions,
            ["landing.judged", "landing.compared", "landing.approved"]
        );
        let (judged, compared, approved) = (&records[0], &records[1], &records[2]);
        assert_eq!(judged.sender_id, "supervisor:control");
        assert_eq!(
            judged.landing.as_ref().unwrap().tree_hash.as_deref(),
            Some("t1")
        );
        assert_eq!(
            serde_json::to_value(&judged.landing.as_ref().unwrap().approved_blobs).unwrap(),
            binding("1", "t1")["approved_blobs"]
        );
        assert_eq!(judged.change.as_ref().unwrap().before, None);
        assert_eq!(judged.correlation.run_id.as_deref(), Some(run.as_str()));
        assert_eq!(compared.outcome.status, OutcomeStatus::Denied);
        let change = compared.change.as_ref().unwrap();
        assert_eq!(
            change.before.as_ref().unwrap()["binding"]["tree_hash"],
            "t1"
        );
        assert_eq!(change.after.as_ref().unwrap()["binding"]["tree_hash"], "t2");
        assert_eq!(change.after.as_ref().unwrap()["rebased"], true);
        assert_eq!(approved.sender_id, "queue_service:control");
        assert_eq!(approved.principal.role, "user");
        assert_eq!(approved.authority.capability.as_deref(), Some("ask.answer"));
        let before = approved.change.as_ref().unwrap().before.as_ref().unwrap();
        assert_eq!(before["tree_hash"], "t1");
        assert_eq!(
            before["approved_blobs"],
            binding("1", "t1")["approved_blobs"]
        );
    }

    /// A judgment whose audit record cannot be written is not recorded
    /// either: both are one transaction.
    #[test]
    fn a_judgment_whose_audit_fails_is_not_recorded() {
        let (_dir, queue, run) = with_run();
        queue.conn.execute_batch("CREATE TRIGGER landing_audit_failure AFTER INSERT ON audit_records WHEN NEW.sender_id='supervisor:control' BEGIN SELECT RAISE(ABORT, 'audit failed'); END;").unwrap();
        let error = queue
            .record_runtime_event(
                &run,
                crate::domain::EventKind::ReviewFinished,
                json!({"verdict": "pass", "binding": binding("1", "t1")}),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("audit failed"), "{error:#}");
        let reviews: i64 = queue
            .conn
            .query_row(
                "SELECT count(*) FROM run_events WHERE kind='review_finished'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reviews, 0);
    }
}
