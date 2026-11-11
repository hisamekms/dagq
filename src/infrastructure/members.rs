//! Shared registry SQL. No JWT or token value is stored here. Registry
//! writes are ports only; authorization and commands belong to their callers.
use super::sql_exec::{OptionalRow, SqlRead, SqlWrite, TransactionEntries, sql_params};
use crate::application::queue_service::MemberStore;
use crate::domain::{
    EventKind,
    credential_preference::{Offer, Order, Preference, Update},
    oidc::OidcIdentity,
    queue_service::{Member, TeamRole},
};
use anyhow::Result;

pub(crate) fn read(tx: &impl SqlRead, identity: &OidcIdentity) -> Result<Option<Member>> {
    let row = tx.query_row(
        "SELECT role, on_call, display_name, created_at, disabled_at FROM members WHERE issuer = ?1 AND subject = ?2",
        sql_params![identity.issuer, identity.subject],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<i64>>(4)?)),
    ).optional()?;
    row.map(|(role, on_call, display_name, created_at, disabled_at)| {
        let role = match role.as_str() {
            "admin" => TeamRole::Admin,
            "member" => TeamRole::Member,
            _ => anyhow::bail!("invalid registry role"),
        };
        anyhow::ensure!(matches!(on_call, 0 | 1), "invalid on-call mark");
        Ok(Member {
            identity: identity.clone(),
            role,
            on_call: on_call == 1,
            display_name,
            created_at,
            disabled_at,
        })
    })
    .transpose()
}

pub(crate) fn add(tx: &impl SqlWrite, member: &Member) -> Result<()> {
    tx.execute("INSERT INTO members(issuer, subject, role, on_call, display_name, created_at, disabled_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        sql_params![member.identity.issuer, member.identity.subject,
        match member.role { TeamRole::Admin => "admin", TeamRole::Member => "member" },
        i64::from(member.on_call), member.display_name, member.created_at, member.disabled_at])?;
    Ok(())
}

pub(crate) fn disable(tx: &impl SqlWrite, identity: &OidcIdentity, at: i64) -> Result<bool> {
    Ok(tx.execute("UPDATE members SET disabled_at = ?3 WHERE issuer = ?1 AND subject = ?2 AND disabled_at IS NULL",
        sql_params![identity.issuer, identity.subject, at])? == 1)
}

/// A person's own credential row (`member_credentials`); `None` when they
/// have none, which reads as [`Preference::NONE`].
pub(crate) fn preference(tx: &impl SqlRead, identity: &OidcIdentity) -> Result<Option<Preference>> {
    let row = tx
        .query_row(
            "SELECT offer, prefer FROM member_credentials WHERE issuer = ?1 AND subject = ?2",
            sql_params![identity.issuer, identity.subject],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    row.map(|(offer, prefer)| {
        Ok(Preference {
            offer: Offer::parse(&offer).ok_or_else(|| anyhow::anyhow!("invalid offer"))?,
            prefer: prefer
                .map(|p| Order::parse(&p).ok_or_else(|| anyhow::anyhow!("invalid order")))
                .transpose()?,
        })
    })
    .transpose()
}

/// Apply `update` to a person's row (or to what no row reads as), in the
/// transaction that finds them an active member: the row now, or `None`
/// (and nothing written) when they are not one. A read writes no row.
/// Either way an active member's call leaves one queue event in the same
/// transaction, `credential_preference_read` or
/// `credential_preference_changed`, as the queue's actor (the caller's
/// `human:` principal) with the person's `issuer` and `subject` and the row
/// now: the audit of each authenticated call (ADR-t3985-1 decision 8).
/// The check and the write need no row lock: every write, a member's
/// disabling included, runs on the service's one write line.
pub(crate) fn update_preference(
    tx: &impl SqlWrite,
    identity: &OidcIdentity,
    update: Update,
    at: i64,
) -> Result<Option<Preference>> {
    if !read(tx, identity)?.is_some_and(|m| m.disabled_at.is_none()) {
        return Ok(None);
    }
    let preference = Preference::of_row(self::preference(tx, identity)?).updated(update);
    let audit = |kind| {
        let mut payload = preference.to_json();
        payload["issuer"] = identity.issuer.clone().into();
        payload["subject"] = identity.subject.clone().into();
        super::store_writes::record_queue_event_in(tx, kind, &payload)
    };
    if update.is_read() {
        audit(EventKind::CredentialPreferenceRead)?;
        return Ok(Some(preference));
    }
    audit(EventKind::CredentialPreferenceChanged)?;
    tx.execute(
        "INSERT INTO member_credentials(issuer, subject, offer, prefer, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(issuer, subject) DO UPDATE SET offer = excluded.offer, prefer = excluded.prefer, updated_at = excluded.updated_at",
        sql_params![identity.issuer, identity.subject, preference.offer.as_str(),
            preference.prefer.map(Order::as_str), at],
    )?;
    Ok(Some(preference))
}

/// The audit of an authenticated person's call whose params did not read
/// (`bad_request`): `credential_preference_rejected` as the queue's actor
/// (the caller), with the person's `issuer` and `subject` and the result
/// alone, never a submitted value.
pub(crate) fn reject_preference(tx: &impl SqlWrite, identity: &OidcIdentity) -> Result<()> {
    super::store_writes::record_queue_event_in(
        tx,
        EventKind::CredentialPreferenceRejected,
        &serde_json::json!({"issuer": identity.issuer, "subject": identity.subject,
            "result": "bad_request"}),
    )
}

macro_rules! member_store {
    ($store:ty $(, $session:ident : $bound:path)?) => {
        impl $(<$session: $bound>)? MemberStore for $store {
            fn member(&self, identity: &OidcIdentity) -> Result<Option<Member>> {
                let tx = self.read_entry("member")?;
                read(&tx, identity)
            }
            fn add_member(&self, member: &Member) -> Result<()> {
                let tx = self.write_entry("add_member")?;
                add(&tx, member)?;
                tx.commit()?;
                Ok(())
            }
            fn disable_member(&self, identity: &OidcIdentity, at: i64) -> Result<bool> {
                let tx = self.write_entry("disable_member")?;
                let changed = disable(&tx, identity, at)?;
                tx.commit()?;
                Ok(changed)
            }
            fn credential_preference(&self, identity: &OidcIdentity) -> Result<Option<Preference>> {
                let tx = self.read_entry("credential_preference")?;
                preference(&tx, identity)
            }
            fn update_credential_preference(
                &self,
                identity: &OidcIdentity,
                update: Update,
                at: i64,
            ) -> Result<Option<Preference>> {
                let tx = self.write_entry("update_credential_preference")?;
                let changed = update_preference(&tx, identity, update, at)?;
                tx.commit()?;
                Ok(changed)
            }
            fn reject_credential_preference(&self, identity: &OidcIdentity) -> Result<()> {
                let tx = self.write_entry("reject_credential_preference")?;
                reject_preference(&tx, identity)?;
                tx.commit()?;
                Ok(())
            }
        }
    };
}
member_store!(super::sqlite::SqliteQueue);
member_store!(super::postgres::PostgresStore<S>, S: super::postgres::PostgresSession);
