//! The eval's rounds as supervisors own them ([`EvalRounds`],
//! ADR-t1728-1 decision 9): the start of a round and its take-up are each
//! one write transaction over the queue's `agent_eval_*` events, so of two
//! supervisors on one queue only one starts a round, and a running round
//! changes hands only from an owner that is gone. A production set of an
//! agent's cases is taken the same way (`agent_cases_taken`).
//!
//! These events have no run, task or goal and the write lock is held while
//! they are read, so every read names `events_by_kind` (why:
//! [`crate::infrastructure::kpi_push`]).

use super::run_log::queue_event;
use super::*;
use crate::application::EvalRounds;
use crate::domain::agent_eval::production::{self, Take};
use crate::domain::agent_eval::round::owner_gone;
use crate::infrastructure::sql_dialect::{JsonType, SqlDialect};
use crate::infrastructure::sql_exec::SqlRead;

/// The queue's events of the eval of round `?1` of the kinds `?2` (a JSON
/// array).
fn of_round(dialect: SqlDialect) -> String {
    format!(
        "kind IN (SELECT value FROM json_each(?2))
     AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
     AND {} = ?1",
        dialect.json_field("payload", "$.eval_id", JsonType::Integer)
    )
}

/// The supervisor's token a start, a take-up or a take names.
fn supervisor(dialect: SqlDialect) -> String {
    dialect.json_field("payload", "$.supervisor", JsonType::Text)
}

fn kinds(kinds: &[EventKind]) -> Result<String> {
    Ok(serde_json::to_string(
        &kinds.iter().map(|kind| kind.as_str()).collect::<Vec<_>>(),
    )?)
}

/// Whether an event matching `of` ([`of_round`] or [`of_set`]) is there.
fn any_of_sql(of: &str) -> String {
    format!("SELECT EXISTS(SELECT 1 FROM run_events INDEXED BY events_by_kind WHERE {of})")
}

/// The supervisor the latest event matching `of` ([`of_round`] or
/// [`of_set`]) names.
fn latest_owner_sql(dialect: SqlDialect, of: &str) -> String {
    format!(
        "SELECT {} FROM run_events INDEXED BY events_by_kind
         WHERE {of} ORDER BY id DESC LIMIT 1",
        supervisor(dialect)
    )
}

/// Whether an event of the round `?1` of the kinds `?2` is there.
pub(super) fn any_of_round_sql(dialect: SqlDialect) -> String {
    any_of_sql(&of_round(dialect))
}

/// The supervisor of the latest event of the round `?1` of the kinds `?2`.
pub(super) fn round_owner_sql(dialect: SqlDialect) -> String {
    latest_owner_sql(dialect, &of_round(dialect))
}

/// Whether a round started (kind `?1`) and has not finished (kind `?2`).
pub(super) fn another_round_runs_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT EXISTS(SELECT 1 FROM run_events s INDEXED BY events_by_kind
         WHERE s.kind = ?1 AND s.run_id IS NULL AND s.task_id IS NULL AND s.goal_id IS NULL
         AND NOT EXISTS(SELECT 1 FROM run_events f INDEXED BY events_by_kind
             WHERE f.kind = ?2 AND f.run_id IS NULL
             AND {} = {}))",
        dialect.json_field("f.payload", "$.eval_id", JsonType::Integer),
        dialect.json_field("s.payload", "$.eval_id", JsonType::Integer),
    )
}

fn any_of_round(tx: &impl SqlRead, eval_id: i64, of: &[EventKind]) -> Result<bool> {
    Ok(tx.query_row(
        &any_of_round_sql(tx.dialect()),
        sql_params![eval_id, kinds(of)?],
        |r| r.get(0),
    )?)
}

impl SqliteQueue {
    /// [`EvalRounds::settle_eval_round`].
    pub fn settle_eval_round(
        &self,
        eval_id: i64,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool> {
        anyhow::ensure!(
            matches!(
                kind,
                EventKind::AgentEvalStarted | EventKind::AgentEvalRefused
            ),
            "{} does not settle a round",
            kind.as_str()
        );
        crate::domain::check_event_target(kind, None, None)?;
        let _read = self.read_before(Closing::Queue(kind, &payload))?;
        let tx = self.write_tx("settle_eval_round")?;
        if any_of_round(
            &tx,
            eval_id,
            &[EventKind::AgentEvalStarted, EventKind::AgentEvalRefused],
        )? {
            return Ok(false);
        }
        if kind == EventKind::AgentEvalStarted {
            let another_runs: bool = SqlRead::query_row(
                &tx,
                &another_round_runs_sql(SqlRead::dialect(&tx)),
                sql_params![
                    EventKind::AgentEvalStarted.as_str(),
                    EventKind::AgentEvalFinished.as_str()
                ],
                |r| r.get(0),
            )?;
            if another_runs {
                return Ok(false);
            }
        }
        queue_event(&tx, kind, &payload)?;
        tx.commit()?;
        Ok(true)
    }

    /// [`EvalRounds::take_up_eval_round`].
    pub fn take_up_eval_round(
        &self,
        eval_id: i64,
        token: &str,
        alive: &dyn Fn(u32) -> bool,
    ) -> Result<bool> {
        let kind = EventKind::AgentEvalTakenUp;
        crate::domain::check_event_target(kind, None, None)?;
        let now = self.generators.clock.now();
        let payload = json!({"eval_id": eval_id, "supervisor": token});
        let _read = self.read_before(Closing::Queue(kind, &payload))?;
        let tx = self.write_tx("take_up_eval_round")?;
        if any_of_round(&tx, eval_id, &[EventKind::AgentEvalFinished])? {
            return Ok(false);
        }
        // The supervisor of the latest start or take-up; no row: not
        // started.
        let owner: Option<Option<String>> = SqlRead::query_row(
            &tx,
            &round_owner_sql(SqlRead::dialect(&tx)),
            sql_params![
                eval_id,
                kinds(&[EventKind::AgentEvalStarted, EventKind::AgentEvalTakenUp])?
            ],
            |r| r.get(0),
        )
        .optional()?;
        let Some(owner) = owner else {
            return Ok(false);
        };
        let registration: Option<(Option<i64>, u32)> = match &owner {
            Some(owner) => SqlRead::query_row(
                &tx,
                "SELECT heartbeat_at, pid FROM supervisors WHERE token = ?1",
                sql_params![owner],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?,
            None => None,
        };
        let registration = registration
            .and_then(|(heartbeat, pid)| heartbeat.map(|heartbeat| (heartbeat, alive(pid))));
        if !owner_gone(owner.as_deref(), token, registration, now) {
            return Ok(false);
        }
        let mut payload = payload;
        payload["from"] = json!(owner);
        queue_event(&tx, kind, &payload)?;
        tx.commit()?;
        Ok(true)
    }
}

/// The queue's events of production set `?1` of the kinds `?2`.
fn of_set(dialect: SqlDialect) -> String {
    format!(
        "kind IN (SELECT value FROM json_each(?2))
     AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
     AND {} = ?1",
        dialect.json_field("payload", "$.set_id", JsonType::Integer)
    )
}

/// Whether an event of the production set `?1` of the kinds `?2` is there.
pub(super) fn any_of_set_sql(dialect: SqlDialect) -> String {
    any_of_sql(&of_set(dialect))
}

/// The supervisor of the latest event of the production set `?1` of the
/// kinds `?2`.
pub(super) fn set_owner_sql(dialect: SqlDialect) -> String {
    latest_owner_sql(dialect, &of_set(dialect))
}

impl SqliteQueue {
    /// [`EvalRounds::take_case_set`].
    pub fn take_case_set(
        &self,
        set_id: i64,
        token: &str,
        alive: &dyn Fn(u32) -> bool,
    ) -> Result<bool> {
        let kind = EventKind::AgentCasesTaken;
        crate::domain::check_event_target(kind, None, None)?;
        let now = self.generators.clock.now();
        let payload = json!({"set_id": set_id, "supervisor": token});
        let _read = self.read_before(Closing::Queue(kind, &payload))?;
        let tx = self.write_tx("take_case_set")?;
        let settled: bool = SqlRead::query_row(
            &tx,
            &any_of_set_sql(SqlRead::dialect(&tx)),
            sql_params![
                set_id,
                kinds(&[EventKind::AgentCasesBuilt, EventKind::AgentCasesRefused])?
            ],
            |r| r.get(0),
        )?;
        let owner: Option<Option<String>> = SqlRead::query_row(
            &tx,
            &set_owner_sql(SqlRead::dialect(&tx)),
            sql_params![set_id, kinds(&[kind])?],
            |r| r.get(0),
        )
        .optional()?;
        let owner = owner.flatten();
        let registration: Option<(Option<i64>, u32)> = match &owner {
            Some(owner) => SqlRead::query_row(
                &tx,
                "SELECT heartbeat_at, pid FROM supervisors WHERE token = ?1",
                sql_params![owner],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?,
            None => None,
        };
        let registration = registration
            .and_then(|(heartbeat, pid)| heartbeat.map(|heartbeat| (heartbeat, alive(pid))));
        let from = match production::take(settled, owner.as_deref(), token, registration, now) {
            Take::Settled | Take::Leave => return Ok(false),
            Take::Keep => return Ok(true),
            Take::Take { from } => from,
        };
        let mut payload = payload;
        payload["from"] = json!(from);
        queue_event(&tx, kind, &payload)?;
        tx.commit()?;
        Ok(true)
    }
}

impl EvalRounds for SqliteQueue {
    fn settle_eval_round(
        &self,
        eval_id: i64,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool> {
        SqliteQueue::settle_eval_round(self, eval_id, kind, payload)
    }
    fn take_up_eval_round(
        &self,
        eval_id: i64,
        token: &str,
        alive: &dyn Fn(u32) -> bool,
    ) -> Result<bool> {
        SqliteQueue::take_up_eval_round(self, eval_id, token, alive)
    }
    fn take_case_set(&self, set_id: i64, token: &str, alive: &dyn Fn(u32) -> bool) -> Result<bool> {
        SqliteQueue::take_case_set(self, set_id, token, alive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requested(queue: &SqliteQueue) -> i64 {
        crate::application::EventStore::record_queue_event(
            queue,
            EventKind::AgentEvalRequested,
            json!({"agent": "demo", "split": "dev"}),
        )
        .unwrap()
        .as_i64()
    }

    fn start(queue: &SqliteQueue, id: i64, owner: &str) -> bool {
        queue
            .settle_eval_round(
                id,
                EventKind::AgentEvalStarted,
                json!({"eval_id": id, "supervisor": owner}),
            )
            .unwrap()
    }

    /// Of two supervisors only one starts a waiting round, a refused round
    /// never starts, and no round starts while another runs.
    #[test]
    fn a_round_starts_once_and_only_while_no_other_runs() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let (first, second, refused) = (requested(&queue), requested(&queue), requested(&queue));
        assert!(start(&queue, first, "a"));
        assert!(!start(&queue, first, "b"), "started already");
        assert!(!start(&queue, second, "b"), "another round runs");
        assert!(
            queue
                .settle_eval_round(
                    refused,
                    EventKind::AgentEvalRefused,
                    json!({"eval_id": refused, "reason": "cost_unknown"}),
                )
                .unwrap()
        );
        crate::application::EventStore::record_queue_event(
            &queue,
            EventKind::AgentEvalFinished,
            json!({"eval_id": first}),
        )
        .unwrap();
        assert!(!start(&queue, refused, "b"), "refused already");
        assert!(start(&queue, second, "b"), "the first finished");
        assert!(
            !queue
                .settle_eval_round(second, EventKind::AgentEvalRunStarted, json!({}))
                .is_ok_and(|done| done),
            "only a start or a refusal settles a round"
        );
    }

    /// A running round is taken up from a gone owner only, once; a live
    /// owner (a fresh heartbeat, or a stale one whose process lives) keeps
    /// it, and a finished round is taken up by nobody.
    #[test]
    fn a_round_is_taken_up_only_from_a_gone_owner_and_once() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        queue
            .conn
            .execute(
                "INSERT INTO supervisors(token, pid, parallel, heartbeat_at)
                 VALUES ('live', 1, 1, unixepoch()), ('asleep', 2, 1, unixepoch() - 600),
                        ('stale', 3, 1, unixepoch() - 600), ('me', 9, 1, unixepoch())",
                [],
            )
            .unwrap();
        let alive = |pid: u32| pid == 2;
        for (owner, taken) in [
            ("live", false),
            ("asleep", false),
            ("stale", true),
            ("gone", true),
        ] {
            let id = requested(&queue);
            assert!(start(&queue, id, owner), "{owner}");
            assert_eq!(
                queue.take_up_eval_round(id, "me", &alive).unwrap(),
                taken,
                "{owner}"
            );
            if taken {
                // Now "me", alive, owns it: another supervisor cannot take it.
                assert!(!queue.take_up_eval_round(id, "other", &alive).unwrap());
            }
            crate::application::EventStore::record_queue_event(
                &queue,
                EventKind::AgentEvalFinished,
                json!({"eval_id": id}),
            )
            .unwrap();
            assert!(!queue.take_up_eval_round(id, "me", &alive).unwrap());
        }
    }
}
