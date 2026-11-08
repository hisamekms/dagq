//! Shared registry SQL. No JWT or token value is stored here. Registry
//! writes are ports only; authorization and commands belong to their callers.
use super::sql_exec::{OptionalRow, SqlRead, SqlWrite, TransactionEntries, sql_params};
use crate::application::queue_service::MemberStore;
use crate::domain::{
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
        }
    };
}
member_store!(super::sqlite::SqliteQueue);
member_store!(super::postgres::PostgresStore<S>, S: super::postgres::PostgresSession);
