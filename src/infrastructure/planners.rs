//! Planner sessions (ADR-0041 decisions 1, 6, 12, 13): one `planners` row
//! per on-demand planner session, with its origin, the proposal the
//! runtime opened it for, its session's handle in `workspace_id` (the
//! background wrapper's of a planner of the runtime's, ADR-t1433-2; the
//! workspace UUID of a person's planner opened before ADR-t1394-1) and
//! what its session wrapper records (pids, heartbeat, the agent's exit).
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, Row, params};

use super::sqlite::{SqliteQueue, enum_col};
use crate::domain::{
    AskId, GoalTask, PlannerHandover, PlannerId, PlannerOrigin, PlannerRoute, PlannerSession,
    ProposalId, RunEvent, TaskStatus, event_kind,
};

impl SqliteQueue {
    /// Mark planner `id` of the runtime's as asked to exit because only a
    /// person's answer to `asks`, its `planner_question`s, is left
    /// (ADR-t1704-1 decision 1): `answer_wait_at` and `planner_answer_wait`
    /// with `payload`, in one write transaction that re-checks that the row
    /// is open and not marked yet and that no ask of `asks` was answered or
    /// closed since. `false`, with nothing written, otherwise: an answer
    /// recorded first goes to the live planner as its next turn, and one
    /// recorded after the mark goes to a new planner once this one's row is
    /// closed, never to both.
    pub fn planner_answer_wait(
        &self,
        id: PlannerId,
        asks: &[AskId],
        payload: &serde_json::Value,
    ) -> Result<bool> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        for ask in asks {
            let waiting: bool = tx
                .query_row(
                    "SELECT answered_at IS NULL AND closed_at IS NULL FROM asks WHERE id=?1",
                    [ask],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(false);
            if !waiting {
                return Ok(false);
            }
        }
        let marked = tx.execute(
            "UPDATE planners SET answer_wait_at=?2
             WHERE id=?1 AND closed_at IS NULL AND answer_wait_at IS NULL",
            params![id, self.generators.clock.now()],
        )? == 1;
        if !marked {
            return Ok(false);
        }
        super::sqlite::record_queue_event_in(
            &tx,
            crate::domain::EventKind::PlannerAnswerWait,
            payload,
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// What the planner that ended for the answer of `ask` alone
    /// ([`Self::planner_answer_wait`]) left for the next (ADR-t1704-1
    /// decision 3): its notes and the drafts it created or edited that are
    /// still drafts. `None` when no planner ended so for it.
    pub fn planner_handover(&self, ask: AskId) -> Result<Option<PlannerHandover>> {
        let planner: Option<PlannerId> = self
            .conn
            .query_row(
                &format!(
                    "SELECT json_extract(e.payload,'$.planner_id') FROM run_events e,
                     json_each(e.payload,'$.asks') j WHERE e.kind='{}' AND j.value=?1
                     ORDER BY e.id DESC LIMIT 1",
                    event_kind::PLANNER_ANSWER_WAIT
                ),
                [ask],
                |r| r.get(0),
            )
            .optional()?;
        let Some(planner) = planner else {
            return Ok(None);
        };
        let actor = format!("planner:{planner}");
        let notes = self
            .conn
            .prepare(&format!(
                "SELECT id, coalesce(json_extract(payload,'$.text'), '') FROM run_events
                 WHERE kind='{}' AND actor_id=?1 ORDER BY id",
                event_kind::OBSERVATION
            ))?
            .query_map([&actor], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let ids: Vec<crate::domain::TaskId> = self
            .conn
            .prepare(&format!(
                "SELECT DISTINCT e.task_id FROM run_events e JOIN tasks t ON t.id=e.task_id
                 WHERE e.kind IN ('{}','{}') AND e.actor_id=?1 AND t.status='draft'
                 ORDER BY e.task_id",
                event_kind::TASK_CREATED,
                event_kind::TASK_EDITED
            ))?
            .query_map([&actor], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut drafts = Vec::new();
        for id in ids {
            let task = super::sqlite::read_task(&self.conn, id)?;
            if task.status() != TaskStatus::Draft {
                continue;
            }
            drafts.push(GoalTask {
                id,
                title: task.title().to_owned(),
                status: task.status(),
                priority: task.priority(),
                priority_source: task.priority_source(),
                priority_by: task.priority_by(),
            });
        }
        Ok(Some(PlannerHandover {
            planner_id: planner,
            notes,
            drafts,
        }))
    }

    /// Record a new planner before its workspace exists; the caller opens
    /// the workspace and records it with [`Self::planner_workspace_created`].
    pub fn open_planner(
        &self,
        origin: PlannerOrigin,
        proposal: Option<ProposalId>,
    ) -> Result<PlannerSession> {
        self.conn.execute(
            "INSERT INTO planners(origin, proposal_id, created_at) VALUES (?1, ?2, ?3)",
            params![origin.as_str(), proposal, self.generators.clock.now()],
        )?;
        self.planner(PlannerId::new(self.conn.last_insert_rowid()))
    }

    /// The UUID of the workspace cmux opened for the planner.
    pub fn planner_workspace_created(&self, id: PlannerId, workspace_id: &str) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE planners SET workspace_id=?2 WHERE id=?1 AND workspace_id IS NULL",
                params![id, workspace_id],
            )? == 1,
            "planner {id} already has a workspace or does not exist"
        );
        Ok(())
    }

    /// Give the planner up: its workspace failed to open (`error`), or is
    /// gone. Closing a closed planner keeps the first close.
    /// Close the planner's row; a planner of the runtime's opened for a
    /// bundle of drafts records what became of each (ADR-t807-1) in the
    /// same transaction.
    pub fn close_planner(&self, id: PlannerId, error: Option<&str>) -> Result<PlannerSession> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let now = self.generators.clock.now();
        let closed = tx.execute(
            "UPDATE planners SET closed_at=?2, error=coalesce(error, ?3)
             WHERE id=?1 AND closed_at IS NULL",
            params![id, now, error],
        )?;
        if closed == 1 {
            super::draft_planners::settle_bundle(&tx, id, now)?;
            // A headless session that had started ends with the row
            // (ADR-t1394-2 decision 4).
            super::sessions::close_planner_spans(&tx, id.as_i64())?;
        }
        tx.commit()?;
        self.planner(id)
    }

    pub fn planner(&self, id: PlannerId) -> Result<PlannerSession> {
        self.conn
            .query_row("SELECT * FROM planners WHERE id=?1", [id], planner_row)
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("planner {id} does not exist"))
    }

    /// The planners not closed, oldest first; with `all`, every planner.
    pub fn planners(&self, all: bool) -> Result<Vec<PlannerSession>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM planners WHERE ?1 OR closed_at IS NULL ORDER BY id")?
            .query_map([all], planner_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Record the route the planner's agent runs on (ADR-t1394-2), before
    /// its session starts; interactive is left as the column's NULL.
    pub fn set_planner_route(&self, id: PlannerId, route: PlannerRoute) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE planners SET route=?2 WHERE id=?1 AND wrapper_pid IS NULL",
                params![
                    id,
                    (route != PlannerRoute::Interactive).then_some(route.as_str())
                ],
            )? == 1,
            "planner {id} has a session already or does not exist"
        );
        Ok(())
    }

    /// The turns of headless planner `id` (ADR-t1394-2 decision 2): the
    /// queue's `turn_*` events that name it, and its waits for a provider
    /// (`provider_waiting`, decision 5), oldest first.
    pub fn planner_turn_events(&self, id: PlannerId) -> Result<Vec<RunEvent>> {
        Ok(self
            .conn
            .prepare(&planner_turn_events_sql())?
            .query_map([id], super::sqlite::event_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The wrapper `pid` started for the planner (in the background, or in
    /// the workspace of a person's planner). A planner has
    /// one session: a second wrapper, or one for a closed planner, is refused.
    pub fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE planners SET wrapper_pid=?2, heartbeat_at=?3
                 WHERE id=?1 AND wrapper_pid IS NULL AND closed_at IS NULL",
                params![id, pid, self.generators.clock.now()],
            )? == 1,
            "planner {id} already has a session or is closed"
        );
        Ok(())
    }

    pub fn register_planner_agent(
        &self,
        id: PlannerId,
        wrapper_pid: u32,
        agent: u32,
    ) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE planners SET agent_pid=?3 WHERE id=?1 AND wrapper_pid=?2",
                params![id, wrapper_pid, agent],
            )? == 1,
            "wrapper {wrapper_pid} does not own planner {id}"
        );
        Ok(())
    }

    pub fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE planners SET heartbeat_at=?3
                 WHERE id=?1 AND wrapper_pid=?2 AND exited_at IS NULL",
                params![id, wrapper_pid, self.generators.clock.now()],
            )? == 1,
            "wrapper {wrapper_pid} does not own planner {id}"
        );
        Ok(())
    }

    /// The planner's agent exited with `exit_code`.
    pub fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE planners SET exit_code=?3, exited_at=?4
                 WHERE id=?1 AND wrapper_pid=?2 AND exited_at IS NULL",
                params![id, wrapper_pid, exit_code, self.generators.clock.now()],
            )? == 1,
            "wrapper {wrapper_pid} does not own planner {id}"
        );
        Ok(())
    }
}

/// The `turn_*` and `provider_waiting` events of planner `?1`, oldest
/// first, by their kind: `+run_id` keeps `events_by_run` from walking every
/// event that has no run.
pub(super) fn planner_turn_events_sql() -> String {
    format!(
        "SELECT * FROM run_events WHERE +run_id IS NULL
           AND kind IN ('{}','{}','{}','{}','{}')
           AND json_extract(payload, '$.planner_id')=?1 ORDER BY id",
        event_kind::TURN_REQUESTED,
        event_kind::TURN_STARTED,
        event_kind::TURN_FINISHED,
        event_kind::TURN_SESSION_IDENTIFIED,
        event_kind::PROVIDER_WAITING,
    )
}

pub(super) fn planner_row(r: &Row<'_>) -> rusqlite::Result<PlannerSession> {
    Ok(PlannerSession {
        id: r.get("id")?,
        origin: enum_col(r, "origin")?,
        proposal_id: r.get("proposal_id")?,
        draft_task_id: r.get("draft_task_id")?,
        finding_id: r.get("finding_id")?,
        request_id: r.get("request_id")?,
        workspace_id: r.get("workspace_id")?,
        wrapper_pid: r.get("wrapper_pid")?,
        agent_pid: r.get("agent_pid")?,
        heartbeat_at: r.get("heartbeat_at")?,
        exit_code: r.get("exit_code")?,
        exited_at: r.get("exited_at")?,
        closed_at: r.get("closed_at")?,
        error: r.get("error")?,
        created_at: r.get("created_at")?,
        route: match r.get::<_, Option<String>>("route")? {
            Some(route) => route.parse().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
            None => PlannerRoute::Interactive,
        },
        answer_wait_at: r.get("answer_wait_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::PlannerState;

    /// ADR-t1704-1 decisions 1 to 3: a planner is marked as ended for a
    /// person's answer once, and only while its questions are not answered
    /// or closed; what it noted and the drafts it created or edited are
    /// handed to the planner that carries the answer.
    #[test]
    fn a_planner_is_marked_for_an_unanswered_question_once_and_hands_over_its_notes_and_drafts() {
        use crate::application::TaskStore;
        use crate::domain::{
            AskKind, AskReason, NewAsk, NewNote, NewTask, NoteTarget,
            actor::{ActorContext, ActorRole},
        };
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let planner = queue.open_planner(PlannerOrigin::Runtime, None).unwrap();
        let mut as_planner = SqliteQueue::open(dir.path().join("q.db"))
            .unwrap()
            .with_actor(ActorContext::instance(ActorRole::Planner, planner.id));
        let draft = as_planner
            .add(
                serde_json::from_value::<NewTask>(serde_json::json!({
                    "title": "left", "description": "", "acceptance": "",
                    "verification_commands": [], "dependencies": [], "context": "",
                }))
                .unwrap(),
            )
            .unwrap();
        as_planner
            .add_note(NewNote {
                target: NoteTarget::Task(draft.id()),
                text: "decided: split".into(),
                kind: None,
                by: "planner".into(),
            })
            .unwrap();
        let question = |queue: &mut SqliteQueue| {
            queue
                .ask(NewAsk {
                    recommendation: None,
                    confidence: None,
                    topics: Vec::new(),
                    kind: AskKind::PlannerQuestion,
                    task_id: Some(draft.id()),
                    run_id: None,
                    question: "split?".into(),
                    options: vec!["adopt".into(), "cancel".into()],
                    asked_by: "planner".into(),
                    reason_category: AskReason::Scope,
                    finding_id: None,
                    request_id: None,
                })
                .unwrap()
                .ask
        };
        // An answer recorded first: the planner is not marked.
        let answered = question(&mut queue);
        queue.answer(answered.id, "adopt").unwrap();
        let payload = serde_json::json!({"planner_id": planner.id});
        assert!(
            !queue
                .planner_answer_wait(planner.id, &[answered.id], &payload)
                .unwrap()
        );
        assert_eq!(queue.planner(planner.id).unwrap().answer_wait_at, None);
        assert!(queue.planner_handover(answered.id).unwrap().is_none());
        // One not answered: marked once.
        let open = question(&mut queue);
        let payload = serde_json::json!({"planner_id": planner.id, "asks": [open.id]});
        assert!(
            queue
                .planner_answer_wait(planner.id, &[open.id], &payload)
                .unwrap()
        );
        assert!(
            !queue
                .planner_answer_wait(planner.id, &[open.id], &payload)
                .unwrap()
        );
        assert!(queue.planner(planner.id).unwrap().answer_wait_at.is_some());
        let handover = queue.planner_handover(open.id).unwrap().unwrap();
        assert_eq!(handover.planner_id, planner.id);
        assert_eq!(
            handover
                .notes
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>(),
            ["decided: split"]
        );
        assert_eq!(
            handover.drafts.iter().map(|t| t.id).collect::<Vec<_>>(),
            [draft.id()]
        );
    }

    #[test]
    fn planners_are_recorded_one_row_each_with_their_session() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let person = queue.open_planner(PlannerOrigin::Person, None).unwrap();
        let other = queue.open_planner(PlannerOrigin::Person, None).unwrap();
        assert_ne!(person.id, other.id);
        assert_eq!(person.workspace_id, None);
        queue.planner_workspace_created(person.id, "W1").unwrap();
        queue.planner_workspace_created(other.id, "W2").unwrap();
        // A workspace is recorded once, and never shared.
        assert!(queue.planner_workspace_created(person.id, "W3").is_err());
        queue.register_planner_wrapper(person.id, 10).unwrap();
        assert!(queue.register_planner_wrapper(person.id, 12).is_err());
        queue.register_planner_agent(person.id, 10, 11).unwrap();
        assert!(queue.register_planner_agent(person.id, 99, 11).is_err());
        queue.heartbeat_planner(person.id, 10).unwrap();
        assert!(queue.heartbeat_planner(person.id, 99).is_err());
        let live = queue.planner(person.id).unwrap();
        assert_eq!(
            (
                live.workspace_id.as_deref(),
                live.wrapper_pid,
                live.agent_pid
            ),
            (Some("W1"), Some(10), Some(11))
        );
        assert!(live.heartbeat_at.is_some());
        queue.planner_exited(person.id, 10, 0).unwrap();
        assert!(queue.planner_exited(person.id, 10, 0).is_err());
        assert!(queue.heartbeat_planner(person.id, 10).is_err());
        let exited = queue.planner(person.id).unwrap();
        assert_eq!(exited.exit_code, Some(0));
        assert_eq!(queue.planners(false).unwrap().len(), 2);

        let failed = queue.open_planner(PlannerOrigin::Runtime, None).unwrap();
        let closed = queue
            .close_planner(failed.id, Some("cmux refused"))
            .unwrap();
        assert_eq!(closed.error.as_deref(), Some("cmux refused"));
        let first = closed.closed_at;
        assert!(first.is_some());
        let again = queue.close_planner(failed.id, Some("other")).unwrap();
        assert_eq!(
            (again.closed_at, again.error.as_deref()),
            (first, Some("cmux refused"))
        );
        assert!(queue.register_planner_wrapper(failed.id, 20).is_err());
        assert_eq!(queue.planners(false).unwrap().len(), 2);
        assert_eq!(queue.planners(true).unwrap().len(), 3);
        assert!(queue.planner(PlannerId::new(99)).is_err());
        let probe = crate::domain::PlannerProbe {
            now: 0,
            workspace_listed: true,
            wrapper_alive: true,
            idle: None,
        };
        assert_eq!(again.state(&probe), PlannerState::Closed);
    }
}
