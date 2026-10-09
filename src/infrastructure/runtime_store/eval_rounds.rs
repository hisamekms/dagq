//! The eval's rounds as supervisors own them ([`EvalRounds`],
//! ADR-t1728-1 decision 9): the start of a round and its take-up are each
//! one write transaction over the queue's `agent_eval_*` events, so of two
//! supervisors on one queue only one starts a round, and a running round
//! changes hands only from an owner that is gone.

use super::run_log::queue_event;
use super::*;
use crate::application::EvalRounds;
use crate::domain::agent_eval::round::owner_gone;

/// The queue's events of the eval of round `?1` of the kinds `?2` (a JSON
/// array).
const OF_ROUND: &str = "kind IN (SELECT value FROM json_each(?2))
     AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
     AND json_extract(payload,'$.eval_id') = ?1";

fn kinds(kinds: &[EventKind]) -> Result<String> {
    Ok(serde_json::to_string(
        &kinds.iter().map(|kind| kind.as_str()).collect::<Vec<_>>(),
    )?)
}

fn any_of_round(tx: &Connection, eval_id: i64, of: &[EventKind]) -> Result<bool> {
    Ok(tx.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM run_events WHERE {OF_ROUND})"),
        params![eval_id, kinds(of)?],
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
        let _read = read_before(&self.conn, Closing::Queue(kind.as_str(), &payload))?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if any_of_round(
            &tx,
            eval_id,
            &[EventKind::AgentEvalStarted, EventKind::AgentEvalRefused],
        )? {
            return Ok(false);
        }
        if kind == EventKind::AgentEvalStarted {
            let another_runs: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_events s
                 WHERE s.kind = ?1 AND s.run_id IS NULL AND s.task_id IS NULL AND s.goal_id IS NULL
                 AND NOT EXISTS(SELECT 1 FROM run_events f
                     WHERE f.kind = ?2 AND f.run_id IS NULL
                     AND json_extract(f.payload,'$.eval_id') = json_extract(s.payload,'$.eval_id')))",
                params![
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
        let _read = read_before(&self.conn, Closing::Queue(kind.as_str(), &payload))?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if any_of_round(&tx, eval_id, &[EventKind::AgentEvalFinished])? {
            return Ok(false);
        }
        // The supervisor of the latest start or take-up; no row: not
        // started.
        let owner: Option<Option<String>> = tx
            .query_row(
                &format!(
                    "SELECT json_extract(payload,'$.supervisor') FROM run_events
                     WHERE {OF_ROUND} ORDER BY id DESC LIMIT 1"
                ),
                params![
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
            Some(owner) => tx
                .query_row(
                    "SELECT heartbeat_at, pid FROM supervisors WHERE token = ?1",
                    [owner],
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requested(queue: &SqliteQueue) -> i64 {
        crate::application::RunLog::record_queue_event(
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
        crate::application::RunLog::record_queue_event(
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
            crate::application::RunLog::record_queue_event(
                &queue,
                EventKind::AgentEvalFinished,
                json!({"eval_id": id}),
            )
            .unwrap();
            assert!(!queue.take_up_eval_round(id, "me", &alive).unwrap());
        }
    }
}
