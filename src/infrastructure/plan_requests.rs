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
    Ask, AskId, EventId, PlannerId, PlannerOrigin, PlannerSession, Priority, ProposalId, RequestId,
    RunEvent, Submission,
    plan_request::{
        NewPlanRequest, NextRequestPlanner, PlanRequest, ProposalLink, RequestAnswerRoute,
        RequestStatus, check_decline, next_request_planner, proposal_link, request_answer_route,
        stale_decliner,
    },
};

/// The requests waiting for a planner of the runtime's: `open`, no planner
/// of the runtime's open for it, no `planner_question` about it or about a
/// draft its planner left (ADR-t2015-1, as `request_of_draft` tells one)
/// nobody closed, and no draft it names (`task:N`) taken by an open planner of
/// the runtime's for drafts, such as one its revisit time opened: the
/// request's planner waits for it to end (ADR-t1540-1).
const TARGETS: &str = "SELECT r.id FROM plan_requests r WHERE r.status='open'
    AND NOT EXISTS(SELECT 1 FROM planners p WHERE p.request_id=r.id AND p.closed_at IS NULL)
    AND NOT EXISTS(SELECT 1 FROM asks a WHERE a.request_id=r.id
        AND a.kind='planner_question' AND a.closed_at IS NULL)
    AND NOT EXISTS(SELECT 1 FROM asks a JOIN tasks t ON t.id=a.task_id AND t.status='draft'
        JOIN run_events e ON e.task_id=t.id AND e.kind='task_created' AND e.actor_role='planner'
        JOIN planners p ON e.actor_id='planner:' || p.id AND p.origin='runtime' AND p.request_id=r.id
        WHERE a.kind='planner_question' AND a.closed_at IS NULL AND a.run_id IS NULL
        AND a.request_id IS NULL AND a.finding_id IS NULL
        AND NOT EXISTS(SELECT 1 FROM draft_origins o WHERE o.task_id=t.id)
        AND NOT EXISTS(SELECT 1 FROM draft_revisits v WHERE v.task_id=t.id)
        AND NOT EXISTS(SELECT 1 FROM draft_reopens d WHERE d.task_id=t.id
            AND json_extract(d.material,'$.proposal_id')=t.proposal_id)
        AND NOT EXISTS(SELECT 1 FROM proposals x WHERE x.id=t.proposal_id AND x.status!='canceled'))
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
               created_at, updated_at, priority) VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?6, ?7)",
            params![
                request.text,
                request.note,
                serde_json::to_string(&request.refs)?,
                by_role,
                by_id,
                now,
                request.priority.map(Priority::as_i64)
            ],
        )?;
        let id = RequestId::new(tx.last_insert_rowid());
        record_queue_event_in(
            &tx,
            EventKind::RequestRecorded,
            &json!({
                "request_id": id,
                "refs": request.refs,
                "priority": request.priority,
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
            // A planner of the request still open (asked to exit because
            // only a person's answer was left) is waited for: one planner
            // per request (ADR-t1704-1 decision 2).
            Some(ask) => {
                let ask = read_ask(&tx, ask)?;
                super::draft_planners::answer_request(&tx, &ask)? == Some(request)
                    && super::draft_planners::route_of(&tx, &ask)? == PlannerAnswerRoute::NewPlanner
                    && open_planner_of(&tx, request)?.is_none()
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
        "SELECT count(*) FROM planners WHERE request_id=?1 AND answer_wait_at IS NULL",
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
        priority: r
            .get::<_, Option<i64>>("priority")?
            .map(Priority::from_i64)
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })?,
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

/// The planner of the runtime's open for `request` that an answer may
/// still go to: not one asked to exit because only a person's answer was
/// left (ADR-t1704-1 decision 2), whose answer waits for a new planner
/// once its row is closed.
fn answering_planner_of(conn: &Connection, request: RequestId) -> Result<Option<PlannerSession>> {
    Ok(open_planner_of(conn, request)?.filter(|planner| planner.answer_wait_at.is_none()))
}

/// Where the answer of a `planner_question` about `request` goes
/// (ADR-t1394-1 decision 7): the planner of the runtime's open for it;
/// else a new planner while the request is still `open`; else closed by
/// the supervisor (it was proposed, declined or ran out).
pub(super) fn route_of(conn: &Connection, request: RequestId) -> Result<PlannerAnswerRoute> {
    if let Some(planner) = answering_planner_of(conn, request)? {
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

/// Where the answer of a `planner_question` about a draft a planner of
/// `request` added goes (ADR-t2015-1): the planner of the runtime's open
/// for the request; else a new planner for it carrying the answer, even
/// when the request is no longer `open` (proposed, declined or out of
/// planners), so the draft is decided.
pub(super) fn draft_route_of(conn: &Connection, request: RequestId) -> Result<PlannerAnswerRoute> {
    Ok(match answering_planner_of(conn, request)? {
        Some(planner) => PlannerAnswerRoute::Planner(Box::new(planner)),
        None => PlannerAnswerRoute::NewPlanner,
    })
}

/// Link the proposal a request's planner submits to its request, inside
/// the submit's transaction (ADR-t1394-1 decision 6): the request of the
/// planner of the runtime's that submits from `workspace`. Each proposal
/// is linked once; the first makes an `open` request `proposed`, with
/// `request_proposed`. A request already ended otherwise (declined, out of
/// planners) is left as it is. Whoever submits, the proposal is also linked
/// to the requests of the proposals its tasks and goals still name
/// (`previous`, [`previous_proposals`]), so a resubmission by another
/// planner stays the request's (ADR-t1971-1 decision 2), even when the
/// request has ended otherwise since: the link records where the plan came
/// from.
pub(super) fn link_requests(
    conn: &Connection,
    proposal: ProposalId,
    workspace: Option<&str>,
    previous: &[ProposalId],
    now: i64,
) -> Result<Vec<RequestId>> {
    let own: Vec<(PlannerId, RequestId)> = match workspace {
        Some(workspace) => conn
            .prepare(
                "SELECT id, request_id FROM planners WHERE workspace_id=?1 AND origin='runtime'
                 AND closed_at IS NULL AND request_id IS NOT NULL ORDER BY id",
            )?
            .query_map([workspace], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?,
        None => Vec::new(),
    };
    let mut linked = Vec::new();
    for (planner, request) in own {
        let link = proposal_link(read_request(conn, request)?.status);
        if link == ProposalLink::Skip {
            continue;
        }
        link_request(conn, request, proposal, link, Some(planner), now)?;
        linked.push(request);
    }
    for request in carried_requests(conn, previous)? {
        if linked.contains(&request) {
            continue;
        }
        let link = proposal_link(read_request(conn, request)?.status);
        link_request(conn, request, proposal, link, None, now)?;
        linked.push(request);
    }
    Ok(linked)
}

/// The proposals the tasks and goals of `submission` name before it is
/// submitted (a withdrawn or sent back proposal keeps them until then),
/// the draft tasks of its goals included. An accepted proposal is not one
/// carried on: new tasks for a goal it opened are another plan.
pub(super) fn previous_proposals(
    conn: &Connection,
    submission: &Submission,
) -> Result<Vec<ProposalId>> {
    let mut named: Vec<ProposalId> = Vec::new();
    for task in &submission.tasks {
        named.extend(
            conn.query_row("SELECT proposal_id FROM tasks WHERE id=?1", [task], |r| {
                r.get::<_, Option<ProposalId>>(0)
            })
            .optional()?
            .flatten(),
        );
    }
    for goal in &submission.goals {
        let ids: Vec<Option<ProposalId>> = conn
            .prepare(
                "SELECT proposal_id FROM goals WHERE id=?1
                 UNION SELECT proposal_id FROM tasks WHERE goal_id=?1 AND status='draft'",
            )?
            .query_map([goal], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        named.extend(ids.into_iter().flatten());
    }
    named.sort();
    named.dedup();
    let mut previous = Vec::new();
    for proposal in named {
        let accepted: bool = conn.query_row(
            "SELECT status='accepted' FROM proposals WHERE id=?1",
            [proposal],
            |r| r.get(0),
        )?;
        if !accepted {
            previous.push(proposal);
        }
    }
    Ok(previous)
}

/// The requests linked to a proposal of `previous`.
fn carried_requests(conn: &Connection, previous: &[ProposalId]) -> Result<Vec<RequestId>> {
    let mut links = Vec::new();
    for &proposal in previous {
        let requests: Vec<RequestId> = conn
            .prepare("SELECT request_id FROM plan_request_proposals WHERE proposal_id=?1")?
            .query_map([proposal], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        links.extend(requests.into_iter().map(|request| (proposal, request)));
    }
    Ok(crate::domain::plan_review::carried_requests(
        previous, &links,
    ))
}

/// Link `proposal` to `request` once; by `link`, an `open` request becomes
/// `proposed` with `request_proposed` (with the planner that submitted it
/// from the request, when one did).
fn link_request(
    conn: &Connection,
    request: RequestId,
    proposal: ProposalId,
    link: ProposalLink,
    planner: Option<PlannerId>,
    now: i64,
) -> Result<()> {
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
    Ok(())
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
