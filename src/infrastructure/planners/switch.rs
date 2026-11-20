use super::{StoreQueue, planner_row_port};
use crate::domain::{
    AskId, EventKind, PlannerCloseCode, PlannerId, PlannerSession, TaskId,
    actor_model::ActorLaunch,
    planner::{PLANNER_SWITCH_ATTEMPTS, PlannerSwitch},
};
use crate::infrastructure::{
    sessions::close_planner_spans,
    sql_dialect::JsonType::Integer,
    sql_exec::{
        CommitTransaction, OptionalRow, SqlDialect, SqlRead, SqlWrite, TransactionEntries,
        sql_params,
    },
    store_writes::record_queue_event_in,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

fn source_id(dialect: SqlDialect, column: &str) -> String {
    dialect.json_field(column, "$.source", Integer)
}

fn target_id(dialect: SqlDialect, column: &str) -> String {
    dialect.json_field(column, "$.planner_id", Integer)
}

/// A reservation holds the work until its recipient starts a turn. While
/// retrying, the newest reservation owns this lock even if its row closed.
pub(in crate::infrastructure) fn pending_switch(dialect: SqlDialect, planner: &str) -> String {
    pending_for(dialect, planner, false)
}

fn pending_for(dialect: SqlDialect, planner: &str, original: bool) -> String {
    let source = source_id(dialect, "reservation.payload");
    let other_source = source_id(dialect, "later.payload");
    let target = target_id(dialect, "reservation.payload");
    let turn_planner = target_id(dialect, "started.payload");
    let turn = dialect.json_field("started.payload", "$.turn", Integer);
    let selected = if original { &source } else { &target };
    format!(
        "EXISTS(SELECT 1 FROM run_events reservation
         WHERE reservation.kind='planner_switch_reserved' AND {selected}={planner}
         AND NOT EXISTS(SELECT 1 FROM run_events later
             WHERE later.kind IN ('planner_switch_reserved','planner_switch_stalled')
               AND later.id>reservation.id AND {other_source}={source})
         AND NOT EXISTS(SELECT 1 FROM run_events started WHERE started.kind='turn_started'
             AND +started.run_id IS NULL AND started.id>reservation.id
             AND ({turn_planner}={target} OR ({turn_planner}={source} AND {turn}>1))))"
    )
}

pub(in crate::infrastructure) fn counts_as_planner(dialect: SqlDialect) -> String {
    let source = source_id(dialect, "stalled.payload");
    let handoff = pending_for(dialect, "p.id", true);
    format!(
        "(coalesce(p.error,'') NOT IN ('provider_switched','switch_not_started')
            AND NOT {}
          OR p.error='provider_switched' AND ({handoff} OR EXISTS(SELECT 1 FROM run_events stalled
            WHERE stalled.kind='planner_switch_stalled' AND {source}=p.id)))",
        pending_switch(dialect, "p.id"),
    )
}

impl<E: TransactionEntries> StoreQueue<E> {
    /// Include earlier reservations of each active handoff for the retry
    /// limit and to remember whether ownership ever moved.
    pub(in crate::infrastructure) fn planner_switch_events(
        &self,
    ) -> Result<Vec<crate::domain::RunEvent>> {
        let tx = self.read_entry("planner_switch_events")?;
        let dialect = tx.dialect();
        let source = source_id(dialect, "reservation.payload");
        let target = target_id(dialect, "reservation.payload");
        let event_source = source_id(dialect, "e.payload");
        let pending = pending_switch(dialect, "recipient.id");
        Ok(tx.query_map(
            &format!("SELECT e.* FROM run_events e
              WHERE e.kind IN ('planner_switch_reserved','planner_switch_completed','planner_switch_stalled')
              AND {event_source} IN (
                 SELECT {source} FROM run_events reservation
                 JOIN planners original ON original.id={source}
                 JOIN planners recipient ON recipient.id={target}
                 WHERE reservation.kind='planner_switch_reserved'
                 AND (original.closed_at IS NULL OR recipient.closed_at IS NULL OR {pending})
              ) ORDER BY e.id"),
            sql_params![],
            super::super::store_rows::event_row,
        )?)
    }

    pub(in crate::infrastructure) fn planner_has_operations(
        &self,
        source: PlannerId,
    ) -> Result<bool> {
        operated(&self.read_entry("planner_has_operations")?, source)
    }

    pub(in crate::infrastructure) fn reserve_planner_switch(
        &self,
        source: PlannerId,
        previous: Option<PlannerId>,
        launch: &ActorLaunch,
    ) -> Result<Option<PlannerSession>> {
        let tx = self.write_entry("reserve_planner_switch")?;
        let original = row(&tx, source)?;
        let attempts = reservations(&tx, source)?;
        let last = attempts.first();
        if last.map(|last| last.planner_id) != previous
            || attempts.len() >= PLANNER_SWITCH_ATTEMPTS
            || operated(&tx, source)?
        {
            return Ok(None);
        }
        if let Some(last) = last {
            let prior = row(&tx, last.planner_id)?;
            if prior.error.as_deref() != Some(PlannerCloseCode::SwitchNotStarted.as_str())
                || !pending(&tx, last.planner_id)?
            {
                return Ok(None);
            }
        } else if original.closed_at.is_some() {
            return Ok(None);
        }
        let now = self.generators.clock.now();
        let id = PlannerId::new(tx.insert_id(
            "INSERT INTO planners(origin, proposal_id, draft_task_id, finding_id, request_id, created_at, route)
             SELECT origin, proposal_id, draft_task_id, finding_id, request_id, ?2, 'headless'
             FROM planners WHERE id=?1",
            sql_params![source, now],
        )?);
        // Copy the work's bundle and attempt numbers. The original open
        // events and consumed revisit remain the only records of an attempt.
        tx.execute(
            "INSERT INTO draft_bundles(planner_id, origin, key_kind, key_value, created_at)
             SELECT ?2, origin, key_kind, key_value, ?3 FROM draft_bundles WHERE planner_id=?1",
            sql_params![source, id, now],
        )?;
        tx.execute(
            "INSERT INTO draft_bundle_members(planner_id, task_id, attempt)
             SELECT ?2, task_id, attempt FROM draft_bundle_members WHERE planner_id=?1",
            sql_params![source, id],
        )?;
        let drafts: Vec<TaskId> = tx.query_map(
            "SELECT task_id FROM draft_bundle_members WHERE planner_id=?1 ORDER BY task_id",
            sql_params![id],
            |r| r.get(0),
        )?;
        let reservation = PlannerSwitch {
            source,
            planner_id: id,
            launch: launch.clone(),
            request_id: original.request_id,
            finding_id: original.finding_id,
            proposal_id: original.proposal_id,
            drafts,
        };
        record_queue_event_in(
            &tx,
            EventKind::PlannerSwitchReserved,
            &serde_json::to_value(reservation)?,
        )?;
        tx.commit()?;
        Ok(Some(self.planner(id)?))
    }

    /// Closes a recipient that never started. A stalled handoff releases
    /// its work and counts the original attempt once, including when its
    /// ownership had already moved to a recipient.
    pub(in crate::infrastructure) fn retire_planner_switch(
        &self,
        switch: &PlannerSwitch,
        workspace: Option<&str>,
        stalled: Option<&[AskId]>,
    ) -> Result<bool> {
        let tx = self.write_entry("retire_planner_switch")?;
        let id = switch.planner_id;
        if !pending(&tx, id)? {
            return Ok(false);
        }
        let target = row(&tx, id)?;
        if target.workspace_id.as_deref() != workspace {
            return Ok(false);
        }
        let why = target
            .error
            .as_deref()
            .filter(|error| *error != PlannerCloseCode::SwitchNotStarted.as_str())
            .map(str::to_owned)
            .or_else(|| {
                target
                    .exit_code
                    .map(|code| format!("wrapper exited with {code} before turn_started"))
            })
            .unwrap_or_else(|| {
                if target.workspace_id.is_none() {
                    "no wrapper handle was recorded".into()
                } else {
                    "wrapper ended before turn_started".into()
                }
            });
        let now = self.generators.clock.now();
        if target.error.as_deref() != Some(PlannerCloseCode::SwitchNotStarted.as_str()) {
            close_switch_row(&tx, &target, now, PlannerCloseCode::SwitchNotStarted, &why)?;
        }
        if let Some(asks) = stalled {
            let mut payload = serde_json::to_value(switch)?;
            payload["asks"] = json!(asks);
            payload["ask_id"] = json!(asks.first());
            payload["reason"] = json!(why);
            record_queue_event_in(&tx, EventKind::PlannerSwitchStalled, &payload)?;
        }
        tx.commit()?;
        Ok(true)
    }
}

/// Called inside the transaction that publishes the handle. The wrapper
/// waits for that handle, so its first operation sees the new revise owner.
pub(super) fn complete(tx: &impl SqlWrite, id: PlannerId, workspace: &str, now: i64) -> Result<()> {
    let Some(switch) = reservation_for(tx, id)? else {
        return Ok(());
    };
    ensure!(
        pending(tx, id)?,
        "planner {id} no longer holds its switch reservation"
    );
    let source = row(tx, switch.source)?;
    if let Some(proposal) = switch.proposal_id {
        let owner: Option<PlannerId> = tx.query_row(
            "SELECT revise_planner_id FROM proposals WHERE id=?1 AND status='revising'",
            sql_params![proposal],
            |r| r.get(0),
        )?;
        let owner = owner.context("the revise has no recipient")?;
        ensure!(
            owner == source.id
                || reservation_for(tx, owner)?.is_some_and(|prior| prior.source == source.id),
            "proposal {proposal} no longer has its switch's revise recipient"
        );
        super::super::plan_reviews::record_revise_sent(
            tx,
            proposal,
            id,
            workspace,
            Some(&switch.launch),
            &[],
            now,
        )?;
    }
    if source.closed_at.is_none() {
        ensure!(
            !operated(tx, source.id)?,
            "planner {} already acted",
            source.id
        );
        close_switch_row(
            tx,
            &source,
            now,
            PlannerCloseCode::ProviderSwitched,
            "the initial turn moved to another provider",
        )?;
    }
    record_queue_event_in(
        tx,
        EventKind::PlannerSwitchCompleted,
        &json!({
            "source": source.id,
            "planner_id": id,
        }),
    )
}

fn close_switch_row(
    tx: &impl SqlWrite,
    planner: &PlannerSession,
    now: i64,
    code: PlannerCloseCode,
    reason: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE planners SET closed_at=coalesce(closed_at,?2), error=?3 WHERE id=?1",
        sql_params![planner.id, now, code.as_str()],
    )?;
    close_planner_spans(tx, planner.id.as_i64())?;
    record_queue_event_in(
        tx,
        EventKind::PlannerClosed,
        &json!({
            "planner_id": planner.id,
            "origin": planner.origin,
            "workspace_id": planner.workspace_id,
            "proposal_id": planner.proposal_id,
            "draft_task_id": planner.draft_task_id,
            "finding_id": planner.finding_id,
            "request_id": planner.request_id,
            "code": code.as_str(),
            "reason": reason,
            "exit_code": planner.exit_code,
            "exited_at": planner.exited_at,
            "workspace_closed": false,
        }),
    )
}

fn operated(tx: &impl SqlRead, planner: PlannerId) -> Result<bool> {
    let prompt_planner = target_id(tx.dialect(), "prompt.payload");
    // The prompt record precedes issuance of the planner's credentials.
    // Use its event ID as the lower bound, tolerating a missing record.
    Ok(tx.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM run_events e WHERE e.actor_role='planner'
         AND e.actor_id=?1 AND e.id>=coalesce((SELECT prompt.id FROM run_events prompt
         WHERE prompt.kind='planner_prompt_written' AND {prompt_planner}=?2
         ORDER BY prompt.id LIMIT 1),0))"
        ),
        sql_params![format!("planner:{planner}"), planner],
        |r| r.get(0),
    )?)
}

pub(in crate::infrastructure) fn pending(tx: &impl SqlRead, planner: PlannerId) -> Result<bool> {
    Ok(tx.query_row(
        &format!("SELECT {}", pending_switch(tx.dialect(), "?1")),
        sql_params![planner],
        |r| r.get(0),
    )?)
}

pub(in crate::infrastructure) fn pending_source(
    tx: &impl SqlRead,
    planner: PlannerId,
) -> Result<bool> {
    Ok(tx.query_row(
        &format!("SELECT {}", pending_for(tx.dialect(), "?1", true)),
        sql_params![planner],
        |r| r.get(0),
    )?)
}

fn reservations(tx: &impl SqlRead, source: PlannerId) -> Result<Vec<PlannerSwitch>> {
    let field = source_id(tx.dialect(), "payload");
    let rows: Vec<Value> = tx.query_map(
        &format!("SELECT payload FROM run_events WHERE kind='planner_switch_reserved' AND {field}=?1 ORDER BY id DESC"),
        sql_params![source],
        |r| super::super::store_rows::json_col(r, "payload"),
    )?;
    rows.into_iter()
        .map(|payload| Ok(serde_json::from_value(payload)?))
        .collect()
}

fn reservation_for(tx: &impl SqlRead, planner: PlannerId) -> Result<Option<PlannerSwitch>> {
    let field = target_id(tx.dialect(), "payload");
    let payload: Option<Value> = tx
        .query_row(
            &format!(
                "SELECT payload FROM run_events WHERE kind='planner_switch_reserved' AND {field}=?1"
            ),
            sql_params![planner],
            |r| super::super::store_rows::json_col(r, "payload"),
        )
        .optional()?;
    payload
        .map(|payload| Ok(serde_json::from_value(payload)?))
        .transpose()
}

fn row(tx: &impl SqlRead, planner: PlannerId) -> Result<PlannerSession> {
    Ok(tx.query_row(
        "SELECT * FROM planners WHERE id=?1",
        sql_params![planner],
        planner_row_port,
    )?)
}

pub(in crate::infrastructure) fn owns_work(dialect: SqlDialect) -> String {
    format!(
        "(p.closed_at IS NULL OR {})",
        pending_switch(dialect, "p.id")
    )
}
