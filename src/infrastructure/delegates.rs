//! Shared SQL of the owners' delegates (ADR-t3989-1 decision 3), the
//! `delegates` table, on either store's port. Who may change them and what
//! a change does are the domain's ([`crate::domain::delegate`]); here the
//! registry is read and the row and its event are written in the caller's
//! one write transaction.
use super::sql_exec::{CommitTransaction, SqlRead, SqlWrite, TransactionEntries, sql_params};
use super::store_queue::StoreQueue;
use crate::application::commands::delegates::DelegateStore;
use crate::domain::delegate::{self, DelegateChange, DelegatesChanged};
use crate::domain::event_kind::EventKind;
use crate::domain::goal_owner::Owner;
use crate::domain::oidc::OidcIdentity;
use anyhow::Result;
use serde_json::{Value, json};

/// The delegates `owner` has, sorted by issuer and subject here rather
/// than by the database's collation.
pub(crate) fn read(tx: &impl SqlRead, owner: &Owner) -> Result<Vec<OidcIdentity>> {
    let owner = owner.identity();
    let mut delegates = tx.query_map(
        "SELECT delegate_issuer, delegate_subject FROM delegates \
         WHERE owner_issuer = ?1 AND owner_subject = ?2",
        sql_params![owner.issuer, owner.subject],
        |row| {
            Ok(OidcIdentity {
                issuer: row.get(0)?,
                subject: row.get(1)?,
            })
        },
    )?;
    delegates.sort_by(delegate::order);
    Ok(delegates)
}

/// Make `change` to `owner`'s delegates in the caller's write transaction
/// at `now` (Unix seconds), its event a queue event. The
/// registry's row of the person is read here and held to
/// [`delegate::check_delegate`]: a refusal returns the
/// [`delegate::DelegateRefusal`] with nothing written. A change that changes
/// something writes its row and its `delegates_changed` event, whose actor
/// is the transaction's; one that changes nothing writes neither.
pub(crate) fn change(
    tx: &impl SqlWrite,
    owner: &Owner,
    change: &DelegateChange,
    now: i64,
) -> Result<DelegatesChanged> {
    change_recorded(tx, owner, change, now, |tx, payload| {
        super::store_writes::record_queue_event_in(tx, EventKind::DelegatesChanged, payload)
    })
}

/// [`change`], its event written by `record`: a failure there fails the
/// change, whose caller lets the transaction go with the row uncommitted.
pub(crate) fn change_recorded<T: SqlWrite>(
    tx: &T,
    owner: &Owner,
    change: &DelegateChange,
    now: i64,
    record: impl FnOnce(&T, &Value) -> Result<()>,
) -> Result<DelegatesChanged> {
    let member = super::members::read(tx, change.delegate())?;
    delegate::check_delegate(owner, change, member.as_ref())?;
    let changed = delegate::change_delegates(owner, &read(tx, owner)?, change);
    if !changed.changed {
        return Ok(changed);
    }
    let (o, d) = (owner.identity(), change.delegate());
    match change {
        DelegateChange::Add(_) => tx.execute(
            "INSERT INTO delegates(owner_issuer, owner_subject, delegate_issuer, \
             delegate_subject, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            sql_params![o.issuer, o.subject, d.issuer, d.subject, now],
        )?,
        DelegateChange::Remove(_) => tx.execute(
            "DELETE FROM delegates WHERE owner_issuer = ?1 AND owner_subject = ?2 \
             AND delegate_issuer = ?3 AND delegate_subject = ?4",
            sql_params![o.issuer, o.subject, d.issuer, d.subject],
        )?,
    };
    record(
        tx,
        &json!({"owner": changed.owner, "change": changed.change,
            "delegate": changed.delegate, "before": changed.before, "after": changed.after}),
    )?;
    Ok(changed)
}

impl<E: TransactionEntries> DelegateStore for StoreQueue<E> {
    fn delegates(&self, owner: &Owner) -> Result<Vec<OidcIdentity>> {
        let tx = self.read_entry("delegates")?;
        read(&tx, owner)
    }

    fn change_delegate(
        &mut self,
        owner: &Owner,
        delegate: &DelegateChange,
    ) -> Result<DelegatesChanged> {
        let tx = self.write_entry("change_delegate")?;
        let changed = change(&tx, owner, delegate, self.generators.clock.now())?;
        tx.commit()?;
        Ok(changed)
    }
}
