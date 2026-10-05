//! Planning requests (ADR-t1394-1): a person's words the inbox records
//! (`plan_requests`), the planner of the runtime's the supervisor opens for
//! each `open` one (`planners.request_id`), the proposals it submits
//! (`plan_request_proposals`), and its `planner_question`s
//! (`asks.request_id`). Opening a planner is one write transaction that
//! re-checks the request first, so two supervisors never open one for the
//! same request. A request's events are the queue's.
use crate::domain::event_kind::EventKind;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::json;

use super::{
    asks::{ask_row, read_ask},
    sqlite::{SqliteQueue, enum_col, event_row, json_col, record_queue_event_in},
};
use crate::application::{PlanRequestStore, PlannerAnswerRoute, RequestPlannerStart};
use crate::domain::{
    Ask, AskId, EventId, PlannerId, PlannerOrigin, PlannerSession, ProposalId, RequestId, RunEvent,
    plan_request::{
        NewPlanRequest, NextRequestPlanner, PlanRequest, ProposalLink, RequestAnswerRoute,
        RequestStatus, check_decline, next_request_planner, proposal_link, request_answer_route,
        stale_decliner,
    },
};

/// The requests waiting for a planner of the runtime's: `open`, no planner
/// of the runtime's open for it, no `planner_question` about it nobody
/// closed, and no draft it names (`task:N`) taken by an open planner of
/// the runtime's for drafts, such as one its revisit time opened: the
/// request's planner waits for it to end (ADR-t1540-1).
const TARGETS: &str = "SELECT r.id FROM plan_requests r WHERE r.status='open'
    AND NOT EXISTS(SELECT 1 FROM planners p WHERE p.request_id=r.id AND p.closed_at IS NULL)
    AND NOT EXISTS(SELECT 1 FROM asks a WHERE a.request_id=r.id
        AND a.kind='planner_question' AND a.closed_at IS NULL)
    AND NOT EXISTS(SELECT 1 FROM json_each(r.refs) j JOIN planners p ON p.closed_at IS NULL
        AND p.origin='runtime' AND (p.draft_task_id=json_extract(j.value,'$.id')
             OR EXISTS(SELECT 1 FROM draft_bundle_members m WHERE m.planner_id=p.id
                 AND m.task_id=json_extract(j.value,'$.id')))
        WHERE json_extract(j.value,'$.kind')='task')";

impl SqliteQueue {
    /// Record `request` as `by_role` (`inbox`, `user`) with actor id
    /// `by_id`, `open`, with `request_recorded`.
    pub fn record_plan_request(
        &mut self,
        request: &NewPlanRequest,
        by_role: &str,
        by_id: &str,
    ) -> Result<PlanRequest> {
        request.validate()?;
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO plan_requests(text, note, refs, requested_by, requested_by_id, status,
               created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?6)",
            params![
                request.text,
                request.note,
                serde_json::to_string(&request.refs)?,
                by_role,
                by_id,
                now
            ],
        )?;
        let id = RequestId::new(tx.last_insert_rowid());
        record_queue_event_in(
            &tx,
            EventKind::RequestRecorded,
            &json!({
                "request_id": id,
                "refs": request.refs,
                "requested_by": by_role,
            }),
        )?;
        let recorded = read_request(&tx, id)?;
        tx.commit()?;
        Ok(recorded)
    }

    /// The requests, oldest first: the `open` ones, or every one with
    /// `all`.
    pub fn plan_requests(&self, all: bool) -> Result<Vec<PlanRequest>> {
        let ids: Vec<RequestId> = self
            .conn
            .prepare(if all {
                "SELECT id FROM plan_requests ORDER BY id"
            } else {
                "SELECT id FROM plan_requests WHERE status='open' ORDER BY id"
            })?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|id| read_request(&self.conn, id))
            .collect()
    }

    /// The planner of the runtime's open for `request`, if any: the one
    /// that may decline it.
    pub fn request_planner(&self, request: RequestId) -> Result<Option<PlannerId>> {
        open_planner_of(&self.conn, request).map(|planner| planner.map(|planner| planner.id))
    }

    /// Decline the `open` `request` with `reason` (ADR-t1394-1 decision 6):
    /// `declined`, with `request_declined` naming the planner that declined
    /// it and who ran the command.
    pub fn decline_request(
        &mut self,
        request: RequestId,
        reason: &str,
        by: &str,
        authorized: Option<PlannerId>,
    ) -> Result<PlanRequest> {
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_request(&tx, request)?;
        check_decline(&current, reason)?;
        let planner = open_planner_of(&tx, request)?.map(|planner| planner.id);
        // The decline was authorized for the planner open then; another
        // opened since is not the decliner's.
        if let Some(refused) = stale_decliner(request, planner, authorized) {
            anyhow::bail!(refused);
        }
        set_status(&tx, request, RequestStatus::Declined, reason, now)?;
        record_queue_event_in(
            &tx,
            EventKind::RequestDeclined,
            &json!({
                "request_id": request,
                "planner_id": planner,
                "reason": reason,
                "by": by,
            }),
        )?;
        let declined = read_request(&tx, request)?;
        tx.commit()?;
        Ok(declined)
    }
}

impl PlanRequestStore for SqliteQueue {
    fn planner_requests(&self) -> Result<Vec<PlanRequest>> {
        let ids: Vec<RequestId> = self
            .conn
            .prepare(&format!("{TARGETS} ORDER BY r.id"))?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|id| read_request(&self.conn, id))
            .collect()
    }

    fn open_request_planner(
        &mut self,
        request: RequestId,
        answer: Option<AskId>,
    ) -> Result<RequestPlannerStart> {
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let eligible = match answer {
            None => tx.query_row(
                &format!("SELECT EXISTS({TARGETS} AND r.id=?1)"),
                [request],
                |r| r.get(0),
            )?,
            // The answer is routed again as it is now: answered, not closed
            // (the inbox may have closed it since the supervisor read it),
            // and still for a new planner of this request.
            Some(ask) => {
                let ask = read_ask(&tx, ask)?;
                ask.request_id == Some(request)
                    && super::draft_planners::route_of(&tx, &ask)? == PlannerAnswerRoute::NewPlanner
            }
        };
        if !eligible {
            return Ok(RequestPlannerStart::Skipped);
        }
        let current = read_request(&tx, request)?;
        // A person's answer is carried past the limit, as a draft's and a
        // finding's is: it is a person's decision.
        let attempt = match next_request_planner(current.planners, answer.is_some()) {
            NextRequestPlanner::Open { attempt } => attempt,
            NextRequestPlanner::Exhausted { attempts, reason } => {
                set_status(&tx, request, RequestStatus::Exhausted, &reason, now)?;
                record_queue_event_in(
                    &tx,
                    EventKind::RequestPlannerExhausted,
                    &json!({"request_id": request, "planners": attempts, "reason": reason}),
                )?;
                tx.commit()?;
                return Ok(RequestPlannerStart::Exhausted { attempts });
            }
        };
        tx.execute(
            "INSERT INTO planners(origin, request_id, created_at) VALUES (?1, ?2, ?3)",
            params![PlannerOrigin::Runtime.as_str(), request, now],
        )?;
        let planner = PlannerId::new(tx.last_insert_rowid());
        record_queue_event_in(
            &tx,
            EventKind::RequestPlannerOpened,
            &json!({
                "request_id": request,
                "planner_id": planner,
                "attempt": attempt,
                "ask_id": answer,
            }),
        )?;
        let current = read_request(&tx, request)?;
        tx.commit()?;
        Ok(RequestPlannerStart::Opened {
            planner: Box::new(self.planner(planner)?),
            request: Box::new(current),
            attempt,
        })
    }

    fn plan_request(&self, request: RequestId) -> Result<PlanRequest> {
        read_request(&self.conn, request)
    }

    fn request_asks(&self, request: RequestId) -> Result<Vec<Ask>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM asks WHERE request_id=?1 ORDER BY id")?
            .query_map([request], ask_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn event_by_id(&self, id: EventId) -> Result<Option<RunEvent>> {
        Ok(self
            .conn
            .query_row("SELECT * FROM run_events WHERE id=?1", [id], event_row)
            .optional()?)
    }
}

/// The request `id` with its proposals and the number of its planners; a
/// missing one is an error.
pub(super) fn read_request(conn: &Connection, id: RequestId) -> Result<PlanRequest> {
    let mut request = conn
        .query_row("SELECT * FROM plan_requests WHERE id=?1", [id], request_row)
        .optional()?
        .with_context(|| format!("request {id} does not exist"))?;
    request.proposals = conn
        .prepare(
            "SELECT proposal_id FROM plan_request_proposals WHERE request_id=?1
             ORDER BY created_at, proposal_id",
        )?
        .query_map([id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let planners: i64 = conn.query_row(
        "SELECT count(*) FROM planners WHERE request_id=?1",
        [id],
        |r| r.get(0),
    )?;
    request.planners = usize::try_from(planners)?;
    Ok(request)
}

fn request_row(r: &Row<'_>) -> rusqlite::Result<PlanRequest> {
    Ok(PlanRequest {
        id: r.get("id")?,
        text: r.get("text")?,
        note: r.get("note")?,
        refs: json_col(r, "refs")?,
        requested_by: r.get("requested_by")?,
        requested_by_id: r.get("requested_by_id")?,
        status: enum_col(r, "status")?,
        status_reason: r.get("status_reason")?,
        proposals: Vec::new(),
        planners: 0,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

fn set_status(
    conn: &Connection,
    request: RequestId,
    to: RequestStatus,
    reason: &str,
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE plan_requests SET status=?2, status_reason=?3, updated_at=?4 WHERE id=?1",
        params![request, to.as_str(), reason, now],
    )?;
    Ok(())
}

fn open_planner_of(conn: &Connection, request: RequestId) -> Result<Option<PlannerSession>> {
    Ok(conn
        .query_row(
            "SELECT * FROM planners WHERE origin='runtime' AND closed_at IS NULL
             AND request_id=?1 ORDER BY id DESC LIMIT 1",
            [request],
            super::planners::planner_row,
        )
        .optional()?)
}

/// Where the answer of a `planner_question` about `request` goes
/// (ADR-t1394-1 decision 7): the planner of the runtime's open for it;
/// else a new planner while the request is still `open`; else closed by
/// the supervisor (it was proposed, declined or ran out).
pub(super) fn route_of(conn: &Connection, request: RequestId) -> Result<PlannerAnswerRoute> {
    if let Some(planner) = open_planner_of(conn, request)? {
        return Ok(PlannerAnswerRoute::Planner(Box::new(planner)));
    }
    // No planner open for it: the request is read only then.
    Ok(
        match request_answer_route(false, read_request(conn, request)?.status) {
            RequestAnswerRoute::NewPlanner => PlannerAnswerRoute::NewPlanner,
            RequestAnswerRoute::OwnPlanner | RequestAnswerRoute::Close => PlannerAnswerRoute::Close,
        },
    )
}

/// Link the proposal a request's planner submits to its request, inside
/// the submit's transaction (ADR-t1394-1 decision 6): the request of the
/// planner of the runtime's that submits from `workspace`. Each proposal
/// is linked once; the first makes an `open` request `proposed`, with
/// `request_proposed`. A request already ended otherwise (declined, out of
/// planners) is left as it is.
pub(super) fn link_requests(
    conn: &Connection,
    proposal: ProposalId,
    workspace: Option<&str>,
    now: i64,
) -> Result<Vec<RequestId>> {
    let Some(workspace) = workspace else {
        return Ok(Vec::new());
    };
    let own: Vec<(PlannerId, RequestId)> = conn
        .prepare(
            "SELECT id, request_id FROM planners WHERE workspace_id=?1 AND origin='runtime'
             AND closed_at IS NULL AND request_id IS NOT NULL ORDER BY id",
        )?
        .query_map([workspace], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut linked = Vec::new();
    for (planner, request) in own {
        let link = proposal_link(read_request(conn, request)?.status);
        if link == ProposalLink::Skip {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO plan_request_proposals(request_id, proposal_id, created_at)
             VALUES (?1, ?2, ?3)",
            params![request, proposal, now],
        )?;
        if link == ProposalLink::Propose {
            set_status(
                conn,
                request,
                RequestStatus::Proposed,
                &format!("proposal {proposal} was submitted from it"),
                now,
            )?;
            record_queue_event_in(
                conn,
                EventKind::RequestProposed,
                &json!({
                    "request_id": request,
                    "proposal_id": proposal,
                    "planner_id": planner,
                }),
            )?;
        }
        linked.push(request);
    }
    Ok(linked)
}

/// What the request commands record a refusal on: the queue itself.
impl crate::application::commands::DenialLog for SqliteQueue {
    fn record_denial(&self, payload: serde_json::Value) -> Result<()> {
        crate::application::RunLog::record_queue_event(
            self,
            EventKind::AuthorizationDenied,
            payload,
        )
        .map(drop)
    }
}

impl crate::application::commands::requests::RequestStore for SqliteQueue {
    fn record_request(
        &mut self,
        request: &NewPlanRequest,
        by_role: &str,
        by_id: &str,
    ) -> Result<PlanRequest> {
        self.record_plan_request(request, by_role, by_id)
    }

    fn request_planner(&self, request: RequestId) -> Result<Option<PlannerId>> {
        read_request(&self.conn, request)?;
        SqliteQueue::request_planner(self, request)
    }

    fn decline_request(
        &mut self,
        request: RequestId,
        reason: &str,
        by: &str,
        planner: Option<PlannerId>,
    ) -> Result<PlanRequest> {
        SqliteQueue::decline_request(self, request, reason, by, planner)
    }
}
