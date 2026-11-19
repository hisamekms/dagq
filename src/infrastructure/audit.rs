//! Primary audit adapter. The handle borrows the use case's already-open
//! transaction: it never commits or opens a second connection. SQLite's
//! immediate writer lock serializes admission. Indexed columns select records;
//! the full JSON preserves the versioned canonical hash input.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::sql_dialect::JsonType;
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
    let principal = connection_principal(tx)?;
    let evidence: Option<String> = tx.query_row("SELECT dagq_direct_human()", &[], |r| r.get(0))?;
    let mut assurance_evidence = std::collections::BTreeMap::new();
    if let Some(evidence) = evidence {
        assurance_evidence.insert("direct_human".into(), evidence);
        if input.action == AuditAction::AuthzPrivileged {
            input.answered_via = Some(crate::domain::audit::AnsweredVia::HumanAttested);
        }
    }
    admit_with(
        tx,
        (Sender::QueueService, "decisions"),
        principal,
        (Attestation::Enforced, assurance_evidence),
        input,
    )
}

/// The principal the connection writes as: the authenticated actor of the
/// request the queue service serves.
fn connection_principal(tx: &impl SqlWrite) -> Result<Principal> {
    Ok(
        tx.query_row("SELECT dagq_actor_role(), dagq_actor_id()", &[], |r| {
            Ok(Principal {
                role: r.get(0)?,
                id: r.get(1)?,
            })
        })?,
    )
}

/// Admit `input` from the control-side component `(sender, instance)` as
/// `principal`, at `attestation`, its counter the next of that component's
/// and its time now, in the caller's transaction.
fn admit_as(
    tx: &impl SqlWrite,
    sender: (Sender, &str),
    principal: Principal,
    attestation: Attestation,
    input: SenderInput,
) -> Result<()> {
    admit_with(
        tx,
        sender,
        principal,
        (attestation, Default::default()),
        input,
    )
}

/// [`admit_as`] with the attestation's `evidence`.
fn admit_with(
    tx: &impl SqlWrite,
    (sender, instance): (Sender, &str),
    principal: Principal,
    (attestation, evidence): (Attestation, std::collections::BTreeMap<String, String>),
    mut input: SenderInput,
) -> Result<()> {
    let sender_id = format!("{sender}:{instance}");
    input.sender_seq = tx.query_row(
        "SELECT COALESCE(MAX(sender_seq),0)+1 FROM audit_records WHERE sender_id=?1",
        sql_params![sender_id],
        |r| r.get(0),
    )?;
    let at = crate::application::timestamp(std::time::SystemTime::now());
    input.at = at.clone();
    append_in(
        tx,
        input,
        Caller {
            sender,
            sender_instance: instance.into(),
            principal,
            on_behalf_of: Vec::new(),
            auth: "control_side".into(),
            assurance: AssuranceClaim::Chained(attestation),
            evidence,
            clock_source: "host".into(),
        },
        at,
    )?;
    Ok(())
}

/// The principal of what the queue service does of itself (its start,
/// a migration it applies before it serves): no request's actor.
fn service_principal() -> Principal {
    Principal {
        role: "supervisor".into(),
        id: "queue-service".into(),
    }
}

/// An update's `update_installed`, in the transaction that records it:
/// the binary it put in place, sent by the updater as the authenticated
/// principal of the job. One that put only the plugin records nothing.
pub(super) fn update_installed(tx: &impl SqlWrite, payload: &Value) -> Result<()> {
    if tx.dialect() != SqlDialect::Sqlite {
        return Ok(());
    }
    let Some(change) = crate::domain::audit::administration::binary_change_of_update(payload)
    else {
        return Ok(());
    };
    let principal = connection_principal(tx)?;
    admit_as(
        tx,
        (Sender::Updater, "update"),
        principal,
        Attestation::Enforced,
        crate::domain::audit::administration::binary_installed(&change),
    )
}

/// A fact of the administration a control-side command sends
/// ([`crate::domain::audit::administration::AdministrationFact`]): a
/// person's install, by the updater; `up` and `down`, by the queue
/// service, with the hash of the bound repository's `dagq.toml` as the
/// service reads it now.
///
/// A fact sent under the result's record `record` (the command's caller
/// sends it again under it while the service does not take it) keeps
/// that identity in its attestation's evidence (`record`), and one
/// admitted under it before is not admitted again.
pub(super) fn administration(
    tx: &impl SqlWrite,
    fact: &crate::domain::audit::administration::AdministrationFact,
    record: Option<&str>,
) -> Result<()> {
    use crate::domain::audit::administration::{self as admin, AdministrationFact};
    if tx.dialect() != SqlDialect::Sqlite {
        return Ok(());
    }
    let (sender, input) = match fact {
        AdministrationFact::Installed(change) => (
            (Sender::Updater, "install"),
            admin::binary_installed(change),
        ),
        AdministrationFact::Lifecycle { operation, error } => {
            let config = bound_settings(tx)?.map(|bytes| super::queue_service::sha256_hex(&bytes));
            let local = bound_local_hash(tx)?;
            (
                (Sender::QueueService, "lifecycle"),
                admin::service_lifecycle(*operation, *error, config.as_deref(), local.as_deref()),
            )
        }
    };
    let mut evidence = std::collections::BTreeMap::new();
    if let Some(record) = record {
        let admitted: bool = tx.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM audit_records WHERE sender_id=?1
                   AND {}=?2)",
                tx.dialect()
                    .json_field("record", "$.attestation.evidence.record", JsonType::Text)
            ),
            sql_params![format!("{}:{}", sender.0, sender.1), record],
            |row| row.get(0),
        )?;
        if admitted {
            return Ok(());
        }
        evidence.insert("record".to_owned(), record.to_owned());
    }
    let principal = connection_principal(tx)?;
    admit_with(
        tx,
        sender,
        principal,
        (Attestation::Enforced, evidence),
        input,
    )
}

/// A migration of the schema from `before` to `after`, in the migration's
/// own transaction once the audit's table is there.
pub(super) fn migrated(tx: &impl SqlWrite, before: i64, after: i64, applied: &[i64]) -> Result<()> {
    admit_as(
        tx,
        (Sender::QueueService, "migrate"),
        service_principal(),
        Attestation::Enforced,
        crate::domain::audit::administration::schema_migrated(before, after, applied),
    )
}

/// The instance of the supervisors' records of the settings they observe:
/// one for all of them, so that each compares with the last any wrote.
const SETTINGS_INSTANCE: &str = "settings";

/// The snapshot of the last record of the settings any supervisor
/// observed; `None` before the first (or with no audit to read).
pub(super) fn last_settings(
    tx: &impl SqlRead,
) -> Result<Option<crate::domain::audit::administration::SettingsSnapshot>> {
    if tx.dialect() != SqlDialect::Sqlite {
        return Ok(None);
    }
    let last: Option<String> = tx
        .query_row(
            "SELECT record FROM audit_records WHERE sender_id=?1 ORDER BY seq DESC LIMIT 1",
            sql_params![format!("{}:{SETTINGS_INSTANCE}", Sender::Supervisor)],
            |row| row.get(0),
        )
        .optional()?;
    Ok(last
        .map(|text| serde_json::from_str::<AuditRecord>(&text))
        .transpose()?
        .and_then(|record| {
            crate::domain::audit::administration::snapshot_of(record.change.as_ref())
        }))
}

/// The settings a supervisor observed at its pass, compared in the
/// transaction with the snapshot of the last record of the observation
/// ([`crate::domain::audit::administration::settings_changed`]); whether
/// it recorded a change. The record is `observed`: no command changed
/// them.
pub(super) fn observe_settings(
    tx: &impl SqlWrite,
    snapshot: &crate::domain::audit::administration::SettingsSnapshot,
) -> Result<bool> {
    use crate::domain::audit::administration as admin;
    if tx.dialect() != SqlDialect::Sqlite {
        return Ok(false);
    }
    let before = last_settings(tx)?;
    let Some(input) = admin::settings_changed(before.as_ref(), snapshot) else {
        return Ok(false);
    };
    let principal = connection_principal(tx)?;
    admit_as(
        tx,
        (Sender::Supervisor, SETTINGS_INSTANCE),
        principal,
        Attestation::Observed,
        input,
    )?;
    Ok(true)
}

/// The settings of the checkout `root` as the audit observes them
/// ([`crate::domain::audit::administration::SettingsSnapshot`]): its
/// `dagq.toml`, each file under its `.dagq/agents` (a missing one is not
/// in it) and the compiled authorization policy.
pub fn settings_snapshot(
    root: &std::path::Path,
) -> Result<crate::domain::audit::administration::SettingsSnapshot> {
    settings_snapshot_at(root, now_millis())
}

/// The wall clock the audit's records of the settings read the local
/// layer's expiry at; a unit test gives its thread a fixed instant instead
/// (`tests::layer_clock_at`).
fn now_millis() -> i64 {
    #[cfg(test)]
    if let Some(now) = TEST_LAYER_NOW.with(std::cell::Cell::get) {
        return now;
    }
    crate::application::unix_millis(std::time::SystemTime::now())
}

#[cfg(test)]
thread_local! {
    /// The instant (Unix milliseconds) a unit test reads the local layer's
    /// expiry at on its thread, instead of the wall clock.
    static TEST_LAYER_NOW: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// [`settings_snapshot`] with the local layer read at `now` (Unix
/// milliseconds).
fn settings_snapshot_at(
    root: &std::path::Path,
    now: i64,
) -> Result<crate::domain::audit::administration::SettingsSnapshot> {
    use crate::domain::audit::administration::{AGENTS_DIR, POLICY_KEY, SETTINGS_FILE};
    use crate::domain::replan_snapshot::sha256;
    fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                files(&path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut snapshot = crate::domain::audit::administration::SettingsSnapshot::new();
    let mut paths = vec![root.join(SETTINGS_FILE)];
    files(&root.join(AGENTS_DIR), &mut paths)?;
    for path in paths {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let relative = path.strip_prefix(root).unwrap_or(&path);
        snapshot.insert(relative.to_string_lossy().into_owned(), sha256(&bytes));
    }
    snapshot.insert(POLICY_KEY.into(), audit::policy_version());
    if let Some(hash) = local_settings_hash(root, now) {
        snapshot.insert(
            crate::domain::audit::administration::LOCAL_SETTINGS_FILE.into(),
            hash,
        );
    }
    Ok(snapshot)
}

/// The hash of the main checkout `root`'s `dagq.local.toml` while it is
/// used over its `dagq.toml` now (ADR-t5108-1): the same read the
/// supervisor's settings use ([`super::run_env::inspect_local_config`]).
fn local_settings_hash(root: &std::path::Path, now: i64) -> Option<String> {
    super::run_env::inspect_local_config(root, now)
        .effective_hash()
        .map(str::to_owned)
}

/// [`local_settings_hash`] of the bound repository's main checkout,
/// resolved as [`bound_settings`] resolves it.
fn bound_local_hash(tx: &impl SqlRead) -> Result<Option<String>> {
    Ok(bound_checkout(tx)?.and_then(|checkout| local_settings_hash(&checkout, now_millis())))
}

/// The bytes of the `dagq.toml` of the bound repository's main checkout,
/// as the queue service resolves it without Git
/// ([`super::adapters::main_checkout_from_files`]): `None` with no binding
/// or no main checkout (a bare repository, a separate Git directory),
/// which reads no other directory's in its place; empty with no file.
fn bound_settings(tx: &impl SqlRead) -> Result<Option<Vec<u8>>> {
    let Some(checkout) = bound_checkout(tx)? else {
        return Ok(None);
    };
    let path = checkout.join("dagq.toml");
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Some(Vec::new())),
        Err(e) => Err(e).with_context(|| format!("read the settings {}", path.display())),
    }
}

/// The bound repository's main checkout as [`bound_settings`] reads it.
fn bound_checkout(tx: &impl SqlRead) -> Result<Option<std::path::PathBuf>> {
    let binding: Option<String> = tx
        .query_row(
            "SELECT git_common_dir FROM queue_repository LIMIT 1",
            &[],
            |row| row.get(0),
        )
        .optional()?;
    Ok(binding.and_then(|binding| {
        super::adapters::main_checkout_from_files(std::path::Path::new(&binding))
    }))
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
    use crate::domain::landing_binding::{BINDING_KEY, binding_of, facts_of};
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
                facts.as_ref().map(|facts| facts.audit_summary()),
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
                .and_then(facts_of)
                .map(|facts| facts.audit_summary());
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
                        "the landing could not carry over the approved change: {}",
                        payload["differing"]
                    )),
                }
            };
            let change = Change {
                before: Some(
                    json!({"judgment": payload["judgment"], "path": payload["path"], "binding": payload["judged"], "head": payload["head_before"]}),
                ),
                after: Some(
                    json!({"binding": payload["landed"], "head": payload["head_after"], "rebased": payload["rebased"], "renumbered": payload["renumbered"], "conflict_resolved": payload["conflict_resolved"], "main": payload["main"], "own_change": payload["own_change"], "differing": payload["differing"], "reapproval": payload["reapproval"]}),
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
            let after = facts_of(&payload["binding_after"]).map(|facts| facts.audit_summary());
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
                .and_then(facts_of)
                .map(|facts| facts.audit_summary());
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
) -> Result<Option<crate::domain::landing_binding::LandingFacts>> {
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
fn binding_change(
    facts: &crate::domain::landing_binding::LandingFacts,
    path: Option<&str>,
) -> Value {
    let mut value = json!(facts);
    if let Some(path) = path {
        value["path"] = json!(path);
    }
    value
}

fn startup_input(at: String, config: &[u8], local: Option<&str>) -> SenderInput {
    let mut after = json!({
        "operation": "audit_started", "dagq_version": crate::VERSION,
        "config_hash": super::queue_service::sha256_hex(config),
    });
    if let Some(local) = local {
        after["local_config_hash"] = json!(local);
    }
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
            after: Some(after),
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
/// The hash covers the dagq.toml bytes of the bound repository's main checkout
/// (empty if absent, or with no main checkout to read it from), and
/// `local_config_hash` the dagq.local.toml used over it, if any.
/// Old schemas remain usable by migration and compatibility inspection.
pub(super) fn ensure_started(tx: &impl SqlWrite) -> Result<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='audit_records')",
        &[],
        |row| row.get(0),
    )?;
    // Only emptiness matters here. Decoding a damaged head would prevent
    // the verifier from writing its mismatch attention in this transaction.
    if !exists
        || tx.query_row("SELECT EXISTS(SELECT 1 FROM audit_records)", &[], |row| {
            row.get::<_, bool>(0)
        })?
    {
        return Ok(());
    }
    let config = bound_settings(tx)?.unwrap_or_default();
    let local = bound_local_hash(tx)?;
    let at = crate::application::timestamp(std::time::SystemTime::now());
    let caller = Caller {
        sender: Sender::QueueService,
        sender_instance: "audit-start".into(),
        principal: service_principal(),
        on_behalf_of: Vec::new(),
        auth: "control_side".into(),
        assurance: AssuranceClaim::Chained(Attestation::Enforced),
        evidence: Default::default(),
        clock_source: "host".into(),
    };
    audit::append(
        &mut SqliteAudit(tx),
        startup_input(at.clone(), &config, local.as_deref()),
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
    // Decode malformed metadata as a verification mismatch, not a failed
    // verification job. SQL/I/O failures still propagate as errors.
    let raw = tx.query_map(
        "SELECT seq,sender_id,sender_seq,hash,prev_hash,received_at,record FROM audit_records WHERE ?1=1 OR seq>=?1-1 ORDER BY seq",
        sql_params![i64::try_from(since)?], |r| {
            let seq = r.get::<_, i64>(0)?;
            let decoded: super::sql_exec::SqlResult<_> = (|| {
                Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?, r.get::<_, u64>(2)?,
                    r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?))
            })();
            Ok((seq, decoded))
        },
    )?;
    let mut rows = Vec::with_capacity(raw.len());
    for (seq, decoded) in raw {
        match decoded {
            Ok(row) if seq > 0 => rows.push(row),
            _ => return Ok(json!({"audit_verified": false, "seq": seq, "kind": "metadata"})),
        }
    }
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
            ..startup_input("2026-11-13T00:00:00Z".into(), b"config", None)
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
        std::fs::create_dir(dir.path().join(".git")).unwrap();
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

    /// Metadata a writer around the CLI made unreadable as the record's
    /// type (a negative sender counter or sequence, a hash that is not
    /// text) is the verification's mismatch of that record's metadata, not
    /// a failed verification: the sequence it names is the row's as stored.
    #[test]
    fn unreadable_metadata_is_a_mismatch_of_its_record() {
        for (corruption, seq) in [("sender_seq=-1", 3), ("seq=-1", -1), ("hash=X'00'", 3)] {
            let (_dir, q) = fixture();
            {
                let tx = q.write_tx("records").unwrap();
                for n in 1..=3 {
                    append_in(&tx, input(n), caller(Sender::QueueService), "now".into()).unwrap();
                }
                tx.commit().unwrap();
            }
            q.conn
                .execute_batch(&format!(
                    "DROP TRIGGER audit_records_no_update; UPDATE audit_records SET {corruption} WHERE seq=3;"
                ))
                .unwrap();
            assert_eq!(
                verify(&q.read_tx("malformed").unwrap(), None).unwrap(),
                json!({"audit_verified": false, "seq": seq, "kind": "metadata"}),
                "{corruption}"
            );
        }
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

    fn records(q: &SqliteQueue) -> Vec<AuditRecord> {
        q.conn
            .prepare("SELECT record FROM audit_records ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
            .collect()
    }

    fn of(q: &SqliteQueue, action: AuditAction) -> Vec<AuditRecord> {
        records(q)
            .into_iter()
            .filter(|r| r.action == action)
            .collect()
    }

    #[test]
    fn an_updates_install_records_its_versions_and_digests_with_it_once() {
        use crate::domain::EventKind;
        let (_dir, q) = fixture();
        let q = q.with_actor(crate::domain::ActorContext::new(
            crate::domain::ActorRole::Updater,
            "job-1",
        ));
        q.record_queue_event(
            EventKind::UpdateInstalled,
            json!({"pid": 9, "version": "0.2.0",
                   "digest": "d-new", "replaced_binary": {"version": "0.1.0", "digest": "d-old"}, "commit": "abc",
                   "e2e": {"status": "passed"}}),
        )
        .unwrap();
        q.record_queue_event(
            EventKind::UpdateInstalled,
            json!({"pid": 9, "version": "0.2.0", "plugin_only": true}),
        )
        .unwrap();
        q.record_queue_event(EventKind::UpdateStarted, json!({"pid": 9}))
            .unwrap();
        let installed = of(&q, AuditAction::BinaryInstalled);
        assert_eq!(installed.len(), 1);
        let record = &installed[0];
        assert_eq!(record.sender_id, "updater:update");
        assert_eq!(record.principal.role, "updater");
        assert_eq!(record.attestation.level, Attestation::Enforced);
        let change = record.change.as_ref().unwrap();
        assert_eq!(
            change.before,
            Some(json!({"version": "0.1.0", "digest": "d-old"}))
        );
        let after = change.after.as_ref().unwrap();
        assert_eq!(after["version_after"], "0.2.0");
        assert_eq!(after["digest_after"], "d-new");
        assert_eq!(after["of"], "abc");
        assert_eq!(after["verification"]["status"], "passed");
        // The update's record and the audit's commit together.
        q.conn.execute_batch("CREATE TRIGGER audit_fail BEFORE INSERT ON audit_records BEGIN SELECT RAISE(ABORT, 'audit failed'); END;").unwrap();
        let before: i64 = q
            .conn
            .query_row(
                "SELECT count(*) FROM run_events WHERE kind='update_installed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            q.record_queue_event(EventKind::UpdateInstalled, json!({"version": "0.3.0"}))
                .is_err()
        );
        assert_eq!(
            q.conn
                .query_row(
                    "SELECT count(*) FROM run_events WHERE kind='update_installed'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            before
        );
    }

    #[test]
    fn up_down_and_an_install_each_record_once_as_the_principal() {
        use crate::domain::audit::administration::{
            AdministrationFact, BinaryChange, LifecycleOperation,
        };
        let (dir, q) = fixture();
        std::fs::write(dir.path().join("dagq.toml"), b"config").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        q.conn
            .execute(
                "INSERT INTO queue_repository (singleton,git_common_dir) VALUES (1,?1)",
                [dir.path().join(".git").to_str().unwrap()],
            )
            .unwrap();
        let q = q.with_actor(crate::domain::ActorContext::new(
            crate::domain::ActorRole::User,
            "person-a",
        ));
        q.audit_administration(&AdministrationFact::Lifecycle {
            operation: LifecycleOperation::Up,
            error: None,
        })
        .unwrap();
        q.audit_administration(&AdministrationFact::Lifecycle {
            operation: LifecycleOperation::Down,
            error: Some(crate::domain::audit::administration::LifecycleFailure::OperationFailed),
        })
        .unwrap();
        q.audit_administration(&AdministrationFact::Installed(Box::new(BinaryChange {
            source: "install".into(),
            version_after: Some("0.2.0".into()),
            digest_after: Some("d".into()),
            ..BinaryChange::default()
        })))
        .unwrap();
        let lifecycle = of(&q, AuditAction::ServiceLifecycle);
        // The audit's start and then `up` and `down`.
        assert_eq!(lifecycle.len(), 3);
        for (record, operation) in lifecycle[1..].iter().zip(["up", "down"]) {
            assert_eq!(record.principal.id, "person-a");
            assert_eq!(record.sender_id, "queue_service:lifecycle");
            let after = record.change.as_ref().unwrap().after.as_ref().unwrap();
            assert_eq!(after["operation"], operation);
            assert_eq!(
                after["config_hash"],
                "b79606fb3afea5bd1609ed40b622142f1c98125abcfe89a76a661b0e8e343910"
            );
        }
        assert_eq!(lifecycle[1].outcome.status, OutcomeStatus::Succeeded);
        assert_eq!(lifecycle[2].outcome.status, OutcomeStatus::Failed);
        assert_eq!(
            lifecycle[2].outcome.reason.as_deref(),
            Some("operation_failed")
        );
        let installed = of(&q, AuditAction::BinaryInstalled);
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].sender_id, "updater:install");
        assert_eq!(installed[0].principal.id, "person-a");
    }

    /// A binding with no main checkout (a separate Git directory) hashes no
    /// other directory's `dagq.toml`: the record says it has none.
    #[test]
    fn a_binding_with_no_main_checkout_records_no_settings_hash() {
        use crate::domain::audit::administration::{AdministrationFact, LifecycleOperation};
        let (dir, q) = fixture();
        // The parent of the separate Git directory has a dagq.toml that is
        // no checkout's.
        std::fs::write(dir.path().join("dagq.toml"), b"config").unwrap();
        std::fs::create_dir(dir.path().join("repo.git")).unwrap();
        q.conn
            .execute(
                "INSERT INTO queue_repository (singleton,git_common_dir) VALUES (1,?1)",
                [dir.path().join("repo.git").to_str().unwrap()],
            )
            .unwrap();
        q.audit_administration(&AdministrationFact::Lifecycle {
            operation: LifecycleOperation::Up,
            error: None,
        })
        .unwrap();
        let lifecycle = of(&q, AuditAction::ServiceLifecycle);
        let up = lifecycle
            .iter()
            .find(|r| r.sender_id == "queue_service:lifecycle")
            .unwrap();
        assert_eq!(
            up.change.as_ref().unwrap().after.as_ref().unwrap()["config_hash"],
            Value::Null
        );
    }

    #[test]
    fn the_settings_are_recorded_observed_when_they_change_only() {
        let (_dir, q) = fixture();
        let snapshot = |hash: &str| -> crate::domain::audit::administration::SettingsSnapshot {
            [("dagq.toml".to_owned(), hash.to_owned())].into()
        };
        assert!(q.observe_settings(&snapshot("a")).unwrap());
        assert!(!q.observe_settings(&snapshot("a")).unwrap());
        assert!(q.observe_settings(&snapshot("b")).unwrap());
        assert!(!q.observe_settings(&snapshot("b")).unwrap());
        let changed = of(&q, AuditAction::ConfigChanged);
        assert_eq!(changed.len(), 2);
        let last = &changed[1];
        // Supervisor A sent `a` above; B observed `b` and recorded it; the
        // settings go back to `a`, which A sends again: compared with B's
        // record, the change back is recorded.
        assert!(q.observe_settings(&snapshot("a")).unwrap());
        let back = of(&q, AuditAction::ConfigChanged);
        assert_eq!(back.len(), 3);
        assert_eq!(
            back[2].change.as_ref().unwrap().before,
            Some(json!({"hashes": {"dagq.toml": "b"}}))
        );
        assert_eq!(last.attestation.level, Attestation::Observed);
        assert_eq!(last.sender_id, "supervisor:settings");
        let change = last.change.as_ref().unwrap();
        assert_eq!(change.before, Some(json!({"hashes": {"dagq.toml": "a"}})));
        assert_eq!(
            change.after,
            Some(json!({"hashes": {"dagq.toml": "b"}, "paths": ["dagq.toml"]}))
        );
    }

    #[test]
    fn the_settings_snapshot_hashes_the_settings_agents_and_policy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dagq.toml"), b"config").unwrap();
        std::fs::create_dir_all(dir.path().join(".dagq/agents/security")).unwrap();
        std::fs::write(dir.path().join(".dagq/agents/security/a.md"), b"agent").unwrap();
        let snapshot = settings_snapshot(dir.path()).unwrap();
        assert_eq!(
            snapshot.keys().collect::<Vec<_>>(),
            [
                ".dagq/agents/security/a.md",
                "authorization_policy",
                "dagq.toml"
            ]
        );
        assert_eq!(
            snapshot["dagq.toml"],
            "b79606fb3afea5bd1609ed40b622142f1c98125abcfe89a76a661b0e8e343910"
        );
        assert_eq!(snapshot["authorization_policy"], audit::policy_version());
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(settings_snapshot(empty.path()).unwrap().len(), 1);
    }

    /// While the main checkout's dagq.local.toml is used, the settings'
    /// snapshot holds its hash beside the dagq.toml's; expired, it does not.
    #[test]
    fn the_settings_snapshot_hashes_a_used_local_layer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "[core]\nbare = false\n").unwrap();
        std::fs::write(dir.path().join("dagq.toml"), "[disk]\nreserve = '5%'\n").unwrap();
        let local = "[local]\nexpires_at = '2027-01-16T08:00:00Z'\n[disk]\nreserve = '12%'\n";
        std::fs::write(dir.path().join("dagq.local.toml"), local).unwrap();
        let now = 1_800_000_000_000;
        let snapshot = settings_snapshot_at(dir.path(), now).unwrap();
        assert_eq!(
            snapshot["dagq.local.toml"],
            crate::domain::replan_snapshot::sha256(local.as_bytes())
        );
        assert!(snapshot.contains_key("dagq.toml"));
        let expired = settings_snapshot_at(dir.path(), now + 86_400_000).unwrap();
        assert!(!expired.contains_key("dagq.local.toml"));
    }

    /// While it lives, the local layer's expiry is read at `now` (Unix
    /// milliseconds) on this thread, as the audit's paths read it.
    pub(super) fn layer_clock_at(now: i64) -> impl Drop {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                TEST_LAYER_NOW.with(|at| at.set(None));
            }
        }
        TEST_LAYER_NOW.with(|at| at.set(Some(now)));
        Reset
    }

    /// The audit's start and a lifecycle record of the bound repository take
    /// the hash of its main checkout's dagq.local.toml while the layer is
    /// used, through the entry each production write goes by: a queue's
    /// first write transaction (the start) and `audit_administration` (`up`).
    /// The expiry is read at a fixed instant given as a value; past it, the
    /// next record has none. The expected hash is computed with another
    /// SHA-256 implementation.
    #[test]
    fn the_start_and_lifecycle_records_hash_a_used_local_layer() {
        use crate::domain::audit::administration::{AdministrationFact, LifecycleOperation};
        let now = 1_800_000_000_000;
        let _clock = layer_clock_at(now);
        let bind = |q: &SqliteQueue, repo: &std::path::Path| {
            q.conn
                .execute(
                    "INSERT INTO queue_repository (singleton,git_common_dir) VALUES (1,?1)",
                    [repo.join(".git").to_str().unwrap()],
                )
                .unwrap();
        };
        let start_of = |q: &SqliteQueue| {
            of(q, AuditAction::ServiceLifecycle)
                .into_iter()
                .find(|r| r.sender_id == "queue_service:audit-start")
                .unwrap()
                .change
                .unwrap()
                .after
                .unwrap()
        };
        let ups = |q: &SqliteQueue| {
            of(q, AuditAction::ServiceLifecycle)
                .into_iter()
                .filter(|r| r.sender_id == "queue_service:lifecycle")
                .map(|r| r.change.unwrap().after.unwrap())
                .collect::<Vec<_>>()
        };
        let up = |q: &SqliteQueue| {
            q.audit_administration(&AdministrationFact::Lifecycle {
                operation: LifecycleOperation::Up,
                error: None,
            })
            .unwrap();
        };
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        std::fs::write(repo.path().join(".git/config"), "[core]\nbare = false\n").unwrap();
        std::fs::write(repo.path().join("dagq.toml"), "[disk]\nreserve = '5%'\n").unwrap();
        // Without a layer, the start holds no local hash.
        let (_first_dir, first) = fixture();
        bind(&first, repo.path());
        first.write_tx("start").unwrap().commit().unwrap();
        let after = start_of(&first);
        assert!(after["config_hash"].is_string(), "{after}");
        assert_eq!(after.get("local_config_hash"), None, "{after}");
        // 2027-01-16T08:00:00Z, a day after `now`.
        let local = "[local]\nexpires_at = '2027-01-16T08:00:00Z'\n[disk]\nreserve = '12%'\n";
        std::fs::write(repo.path().join("dagq.local.toml"), local).unwrap();
        let expected = ring::digest::digest(&ring::digest::SHA256, local.as_bytes())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        // A queue whose chain starts now: its start holds the layer's hash.
        let (_second_dir, second) = fixture();
        bind(&second, repo.path());
        second.write_tx("start").unwrap().commit().unwrap();
        let after = start_of(&second);
        assert_eq!(after["local_config_hash"], expected.as_str(), "{after}");
        // `up` through the administration's entry holds it too.
        up(&first);
        let after = &ups(&first)[0];
        assert_eq!(after["operation"], "up");
        assert_eq!(after["local_config_hash"], expected.as_str(), "{after}");
        assert!(after["config_hash"].is_string(), "{after}");
        // Past its expiry, the next `up` holds none.
        drop(_clock);
        let _later = layer_clock_at(now + 86_400_000);
        up(&first);
        let after = &ups(&first)[1];
        assert_eq!(after.get("local_config_hash"), None, "{after}");
    }

    #[test]
    fn a_change_around_the_cli_opens_one_attention_until_the_chain_verifies() {
        use crate::domain::EventKind;
        let (_dir, q) = fixture();
        {
            let tx = q.write_tx("records").unwrap();
            for n in 1..=2 {
                append_in(&tx, input(n), caller(Sender::QueueService), "now".into()).unwrap();
            }
            tx.commit().unwrap();
        }
        let mismatches = |q: &SqliteQueue| -> i64 {
            q.conn
                .query_row(
                    "SELECT count(*) FROM run_events WHERE kind=?1",
                    [EventKind::SupervisorAuditMismatch.as_str()],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(q.check_audit_chain().unwrap()["audit_verified"], true);
        assert_eq!(mismatches(&q), 0);
        let text: String = q
            .conn
            .query_row("SELECT record FROM audit_records WHERE seq=2", [], |r| {
                r.get(0)
            })
            .unwrap();
        q.conn
            .execute_batch("DROP TRIGGER audit_records_no_update")
            .unwrap();
        q.conn
            .execute(
                "UPDATE audit_records SET record=?1 WHERE seq=2",
                [text.replace("\"user\"", "\"intruder\"")],
            )
            .unwrap();
        for _ in 0..2 {
            let found = q.check_audit_chain().unwrap();
            assert_eq!(found["audit_verified"], false);
            assert_eq!(found["seq"], 2);
        }
        assert_eq!(mismatches(&q), 1);
        q.conn
            .execute("UPDATE audit_records SET record=?1 WHERE seq=2", [&text])
            .unwrap();
        assert_eq!(q.check_audit_chain().unwrap()["audit_verified"], true);
        // A check that verified a sound snapshot before the change and
        // writes after another check found it: its older answer closes
        // nothing.
        let stale = q.verify_audit_chain().unwrap().unwrap();
        assert_eq!(stale.0["audit_verified"], true);
        q.conn
            .execute(
                "UPDATE audit_records SET record=?1 WHERE seq=2",
                [text.replace("\"user\"", "\"intruder\"")],
            )
            .unwrap();
        assert_eq!(q.check_audit_chain().unwrap()["audit_verified"], false);
        assert_eq!(mismatches(&q), 2);
        assert_eq!(q.apply_audit_check(stale).unwrap()["superseded"], true);
        assert_eq!(
            q.latest_queue_event(
                &crate::domain::audit::administration::CHECK_KINDS.map(EventKind::as_str)
            )
            .unwrap()
            .unwrap()
            .kind,
            EventKind::SupervisorAuditMismatch
        );
        q.conn
            .execute("UPDATE audit_records SET record=?1 WHERE seq=2", [text])
            .unwrap();
        assert_eq!(q.check_audit_chain().unwrap()["audit_verified"], true);
        assert_eq!(
            q.latest_queue_event(
                &crate::domain::audit::administration::CHECK_KINDS.map(EventKind::as_str)
            )
            .unwrap()
            .unwrap()
            .kind,
            EventKind::SupervisorAuditVerified
        );
    }
}
