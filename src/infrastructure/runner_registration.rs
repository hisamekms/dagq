//! Shared SQL for hashed runner credentials. Every mutation uses the adapter's
//! fenced write entry, including replay revocation committed before refusing.
use super::sql_exec::{OptionalRow, SqlRead, SqlWrite, TransactionEntries, sql_params};
use crate::application::RunnerStore;
use crate::domain::{oidc::OidcIdentity, runner_registration::*};
use anyhow::Result;

fn runner(tx: &impl SqlRead, id: &str) -> Result<Option<Runner>> {
    tx.query_row(
        "SELECT owner_issuer, owner_subject, created_at, revoked_at FROM runners WHERE id = ?1",
        sql_params![id],
        |r| {
            Ok(Runner {
                id: id.into(),
                owner: OidcIdentity {
                    issuer: r.get(0)?,
                    subject: r.get(1)?,
                },
                created_at: r.get(2)?,
                revoked_at: r.get(3)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn credentials(
    tx: &impl SqlWrite,
    id: &str,
    refresh: &str,
    token: &str,
    now: i64,
) -> Result<Outcome> {
    let Some(refresh_expiry) = now.checked_add(REFRESH_SECONDS) else {
        anyhow::bail!("credential expiry overflow")
    };
    let Some(token_expiry) = now.checked_add(SHORT_SECONDS) else {
        anyhow::bail!("credential expiry overflow")
    };
    tx.execute(
        "INSERT INTO runner_refresh(hash, runner_id, expires_at) VALUES (?1, ?2, ?3)",
        sql_params![refresh, id, refresh_expiry],
    )?;
    tx.execute(
        "INSERT INTO runner_tokens(hash, runner_id, expires_at) VALUES (?1, ?2, ?3)",
        sql_params![token, id, token_expiry],
    )?;
    Ok(Outcome::Issued(id.into()))
}

fn revoke(tx: &impl SqlWrite, id: &str, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE runners SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
        sql_params![id, now],
    )?;
    Ok(())
}

fn change(tx: &impl SqlWrite, action: &Change, now: i64) -> Result<Outcome> {
    match action {
        Change::Code { owner, hash } => {
            if !super::members::read(tx, owner)?.is_some_and(|m| m.disabled_at.is_none()) {
                return Ok(Outcome::Unauthenticated);
            }
            let expiry = now
                .checked_add(SHORT_SECONDS)
                .ok_or_else(|| anyhow::anyhow!("credential expiry overflow"))?;
            tx.execute("INSERT INTO runner_codes(hash, owner_issuer, owner_subject, expires_at) VALUES (?1, ?2, ?3, ?4)", sql_params![hash, owner.issuer, owner.subject, expiry])?;
            Ok(Outcome::Code)
        }
        Change::Enroll {
            hash,
            runner_id,
            refresh,
            token,
        } => {
            let row = tx.query_row("SELECT owner_issuer, owner_subject, expires_at, used_at FROM runner_codes WHERE hash = ?1",
                sql_params![hash], |r| Ok((OidcIdentity { issuer: r.get(0)?, subject: r.get(1)? }, Credential { expires_at: r.get(2)?, used_at: r.get(3)? }))).optional()?;
            let Some((owner, code)) = row else {
                return Ok(Outcome::Unauthenticated);
            };
            if !enrollment_live(Some(&code), now)
                || !super::members::read(tx, &owner)?.is_some_and(|m| m.disabled_at.is_none())
            {
                return Ok(Outcome::Unauthenticated);
            }
            if tx.execute(
                "UPDATE runner_codes SET used_at = ?2 WHERE hash = ?1 AND used_at IS NULL",
                sql_params![hash, now],
            )? != 1
            {
                return Ok(Outcome::Unauthenticated);
            }
            tx.execute("INSERT INTO runners(id, owner_issuer, owner_subject, created_at) VALUES (?1, ?2, ?3, ?4)", sql_params![runner_id, owner.issuer, owner.subject, now])?;
            credentials(tx, runner_id, refresh, token, now)
        }
        Change::Refresh {
            hash,
            refresh,
            token,
        } => {
            let row = tx
                .query_row(
                    "SELECT runner_id, expires_at, used_at FROM runner_refresh WHERE hash = ?1",
                    sql_params![hash],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            Credential {
                                expires_at: r.get(1)?,
                                used_at: r.get(2)?,
                            },
                        ))
                    },
                )
                .optional()?;
            let Some((id, credential)) = row else {
                return Ok(Outcome::Unauthenticated);
            };
            let Some(runner) = runner(tx, &id)? else {
                return Ok(Outcome::Unauthenticated);
            };
            let owner = super::members::read(tx, &runner.owner)?;
            match refresh_decision(Some(&credential), &runner, owner.as_ref(), now) {
                RefreshDecision::Reuse => {
                    revoke(tx, &id, now)?;
                    Ok(Outcome::Unauthenticated)
                }
                RefreshDecision::Refuse => Ok(Outcome::Unauthenticated),
                RefreshDecision::Rotate => {
                    if tx.execute("UPDATE runner_refresh SET used_at = ?2 WHERE hash = ?1 AND used_at IS NULL", sql_params![hash, now])? != 1 {
                        revoke(tx, &id, now)?;
                        return Ok(Outcome::Unauthenticated);
                    }
                    credentials(tx, &id, refresh, token, now)
                }
            }
        }
        Change::Revoke { caller, runner_id } => {
            let Some(caller) =
                super::members::read(tx, caller)?.filter(|m| m.disabled_at.is_none())
            else {
                return Ok(Outcome::Unauthenticated);
            };
            let Some(runner) = runner(tx, runner_id)? else {
                return Ok(Outcome::Denied);
            };
            if !may_revoke(&caller, &runner) {
                return Ok(Outcome::Denied);
            }
            revoke(tx, runner_id, now)?;
            Ok(Outcome::Revoked)
        }
    }
}

fn authenticate(tx: &impl SqlRead, hash: &str, now: i64) -> Result<Option<Runner>> {
    let row = tx
        .query_row(
            "SELECT runner_id, expires_at FROM runner_tokens WHERE hash = ?1",
            sql_params![hash],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((id, expiry)) = row else {
        return Ok(None);
    };
    let Some(runner) = runner(tx, &id)? else {
        return Ok(None);
    };
    let owner = super::members::read(tx, &runner.owner)?;
    Ok(token_live(expiry, &runner, owner.as_ref(), now).then_some(runner))
}

macro_rules! runner_store {
    ($store:ty $(, $session:ident : $bound:path)?) => {
        impl $(<$session: $bound>)? RunnerStore for $store {
            fn change_runner(&self, action: &Change, now: i64) -> Result<Outcome> {
                let tx = self.write_entry("runner_change")?;
                let result = change(&tx, action, now)?;
                tx.commit()?;
                Ok(result)
            }
            fn runner_token(&self, hash: &str, now: i64) -> Result<Option<Runner>> {
                let tx = self.read_entry("runner_token")?;
                authenticate(&tx, hash, now)
            }
        }
    };
}
runner_store!(super::sqlite::SqliteQueue);
runner_store!(super::postgres::PostgresStore<S>, S: super::postgres::PostgresSession);
