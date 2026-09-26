//! Findings the runtime opens a planner for (ADR-0044 decision 19, carried
//! by ADR-0047): a finding the observer marked for a proposal
//! (`finding record --propose`), or one a person answered `propose` about,
//! gets a planner of the runtime's (`planners.finding_id`), which makes a
//! proposal of it, dismisses it or asks a person. The proposal's submission
//! links the finding to it (`proposed`), and the end of that proposal
//! resolves the finding or opens it again. Opening a planner is one write
//! transaction that re-checks the finding first, so two supervisors never
//! open one for the same finding.
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::json;

use super::{
    asks::{ask_row, read_ask},
    findings::{finding_event, finding_row, read_finding, record_in, set_status_in},
    sqlite::SqliteQueue,
};
use crate::application::{FindingPlannerStart, PlannerAnswerRoute};
use crate::domain::{
    Ask, AskId, AskKind, Finding, FindingAnswer, FindingId, FindingStatus, FindingTarget,
    MAX_FINDING_PLANNERS, NewFinding, PlannerId, PlannerOrigin, Proposal, ProposalId,
    ProposalStatus, Submission, TaskStatus, finding,
};

/// The findings waiting for a planner of the runtime's: `open`, marked for
/// a proposal, no planner of the runtime's open for it, no
/// `planner_question` about it nobody closed, and its planners since the
/// mark not used up.
const TARGETS: &str = "SELECT f.id FROM findings f
    WHERE f.status='open' AND f.propose_reason IS NOT NULL
    AND NOT EXISTS(SELECT 1 FROM planners p WHERE p.finding_id=f.id AND p.closed_at IS NULL)
    AND NOT EXISTS(SELECT 1 FROM asks a WHERE a.finding_id=f.id
        AND a.kind='planner_question' AND a.closed_at IS NULL)
    AND NOT EXISTS(SELECT 1 FROM run_events e WHERE e.kind='finding_planner_exhausted'
        AND json_extract(e.payload,'$.finding_id')=f.id
        AND json_extract(e.payload,'$.marked_at') IS f.propose_requested_at)";

impl SqliteQueue {
    /// Submit `submission` (see [`crate::application::TaskStore::submit`])
    /// and, in the same transaction, link to its proposal the findings it
    /// remedies ([`link_findings`]): the ones named, and that of the
    /// runtime's planner that submits.
    pub fn submit_linking(
        &mut self,
        submission: Submission,
        findings: &[FindingId],
    ) -> Result<Proposal> {
        let now = self.generators.clock.now();
        let workspace = submission.owner.workspace_id.clone();
        let by = match submission.owner.origin {
            PlannerOrigin::Runtime => "planner",
            PlannerOrigin::Person => "person",
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let proposal =
            super::proposals::submit(&tx, submission, &self.generators.clock.timestamp())?;
        link_findings(&tx, proposal.id(), workspace.as_deref(), findings, by, now)?;
        let proposal = super::proposals::read(&tx, proposal.id())?;
        tx.commit()?;
        Ok(proposal)
    }

    /// The findings waiting for a planner of the runtime's, the oldest
    /// mark first.
    pub fn planner_findings(&self) -> Result<Vec<Finding>> {
        let ids: Vec<FindingId> = self
            .conn
            .prepare(&format!("{TARGETS} ORDER BY f.propose_requested_at, f.id"))?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|id| read_finding(&self.conn, id))
            .collect()
    }

    /// Record a planner of the runtime's for `finding`
    /// (`finding_planner_opened`) after re-checking it in the same write
    /// transaction. With `answer`, the planner carries that answered
    /// `planner_question` about the finding, whose planner is gone. A
    /// finding that had [`MAX_FINDING_PLANNERS`] planners since its mark
    /// records `finding_planner_exhausted` (the inbox's attention) instead.
    pub fn open_finding_planner(
        &mut self,
        finding: FindingId,
        answer: Option<AskId>,
    ) -> Result<FindingPlannerStart> {
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let eligible = match answer {
            None => is_target(&tx, finding)?,
            Some(ask) => {
                let ask = read_ask(&tx, ask)?;
                ask.finding_id == Some(finding)
                    && route_of(&tx, finding)? == PlannerAnswerRoute::NewPlanner
            }
        };
        if !eligible {
            return Ok(FindingPlannerStart::Skipped);
        }
        let current = read_finding(&tx, finding)?;
        let opened = planners_since_mark(&tx, &current)?;
        // A person's answer is carried past the limit, as a draft's is.
        if opened >= MAX_FINDING_PLANNERS && answer.is_none() {
            finding_event(
                &tx,
                &current,
                "finding_planner_exhausted",
                json!({
                    "finding_id": finding,
                    "planners": opened,
                    "marked_at": current.propose_requested_at,
                    "reason": format!(
                        "{opened} planners of the runtime's ended without deciding the finding (at most {MAX_FINDING_PLANNERS})"
                    ),
                }),
            )?;
            tx.commit()?;
            return Ok(FindingPlannerStart::Exhausted { attempts: opened });
        }
        tx.execute(
            "INSERT INTO planners(origin, finding_id, created_at) VALUES (?1, ?2, ?3)",
            params![PlannerOrigin::Runtime.as_str(), finding, now],
        )?;
        let planner = PlannerId::new(tx.last_insert_rowid());
        let attempt = opened + 1;
        finding_event(
            &tx,
            &current,
            "finding_planner_opened",
            json!({
                "finding_id": finding,
                "planner_id": planner,
                "attempt": attempt,
                "ask_id": answer,
                "propose_reason": current.propose_reason,
            }),
        )?;
        tx.commit()?;
        Ok(FindingPlannerStart::Opened {
            planner: self.planner(planner)?,
            finding: Box::new(current),
            attempt,
        })
    }

    /// The asks about `finding`, oldest first: the observer's `blocked`
    /// ones, the asks a person answered `propose` or `dismiss` about it,
    /// and its planners' questions.
    pub fn finding_asks(&self, finding: FindingId) -> Result<Vec<Ask>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM asks WHERE finding_id=?1 ORDER BY id")?
            .query_map([finding], ask_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// End the `proposed` findings whose proposal ended
    /// ([`finding::settle`]): `resolved` once its tasks ended with one
    /// completed, `open` again (without its mark and proposal) when the
    /// proposal or all its tasks were canceled. Records
    /// `finding_status_changed` with the proposal.
    pub fn settle_findings(&mut self) -> Result<Vec<(FindingId, FindingStatus)>> {
        // Most passes have nothing to settle: no write lock for them.
        let any: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM findings WHERE status='proposed')",
            [],
            |r| r.get(0),
        )?;
        if !any {
            return Ok(Vec::new());
        }
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let proposed: Vec<Finding> = tx
            .prepare(
                "SELECT * FROM findings WHERE status='proposed' AND proposal_id IS NOT NULL
                 ORDER BY id",
            )?
            .query_map([], finding_row)?
            .collect::<rusqlite::Result<_>>()?;
        let mut settled = Vec::new();
        for current in proposed {
            let proposal = current.proposal_id.context("a proposed finding")?;
            let (status, tasks) = proposal_state(&tx, proposal)?;
            let Some((to, reason)) = finding::settle(status, &tasks) else {
                continue;
            };
            if to == FindingStatus::Open {
                tx.execute(
                    "UPDATE findings SET status='open', status_reason=?2, proposal_id=NULL,
                       propose_reason=NULL, propose_requested_at=NULL, updated_at=?3
                     WHERE id=?1",
                    params![current.id, reason, now],
                )?;
            } else {
                tx.execute(
                    "UPDATE findings SET status=?2, status_reason=?3, updated_at=?4 WHERE id=?1",
                    params![current.id, to.as_str(), reason, now],
                )?;
            }
            finding_event(
                &tx,
                &current,
                "finding_status_changed",
                json!({
                    "finding_id": current.id,
                    "from": current.status,
                    "to": to,
                    "reason": reason,
                    "proposal_id": proposal,
                    "by": "runtime",
                }),
            )?;
            settled.push((current.id, to));
        }
        tx.commit()?;
        Ok(settled)
    }

    /// Findings still `open` and marked whose planners since the mark were
    /// used up (`finding_planner_exhausted`), by ID.
    pub fn exhausted_findings(&self) -> Result<Vec<Finding>> {
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM findings f WHERE f.status='open' AND f.propose_reason IS NOT NULL
                 AND EXISTS(SELECT 1 FROM run_events e WHERE e.kind='finding_planner_exhausted'
                     AND json_extract(e.payload,'$.finding_id')=f.id
                     AND json_extract(e.payload,'$.marked_at') IS f.propose_requested_at)
                 ORDER BY f.id",
            )?
            .query_map([], finding_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Record an event on the finding's target (on nothing for the queue).
    pub fn record_finding_event(
        &mut self,
        finding: FindingId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        let current = read_finding(&self.conn, finding)?;
        finding_event(&self.conn, &current, kind, payload)
    }

    /// Whether the supervisor typed the answer of `ask` into `workspace`
    /// (`ask_delivered`).
    pub fn ask_delivered_to(&self, ask: AskId, workspace: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_events WHERE kind='ask_delivered'
             AND json_extract(payload,'$.ask_id')=?1
             AND json_extract(payload,'$.workspace_id')=?2)",
            params![ask, workspace],
            |r| r.get(0),
        )?)
    }

    /// Whether typing the answer of `ask` ever failed (`ask_delivery_failed`).
    pub fn ask_delivery_failed(&self, ask: AskId) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_events WHERE kind='ask_delivery_failed'
             AND json_extract(payload,'$.ask_id')=?1)",
            [ask],
            |r| r.get(0),
        )?)
    }
}

fn is_target(conn: &Connection, finding: FindingId) -> Result<bool> {
    Ok(conn.query_row(
        &format!("SELECT EXISTS({TARGETS} AND f.id=?1)"),
        [finding],
        |r| r.get(0),
    )?)
}

/// The planners of the runtime's opened for the finding since its mark.
fn planners_since_mark(conn: &Connection, finding: &Finding) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM planners WHERE finding_id=?1 AND created_at>=?2",
        params![finding.id, finding.propose_requested_at.unwrap_or(0)],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count)?)
}

/// The proposal's status and the statuses of the tasks it holds.
fn proposal_state(
    conn: &Connection,
    proposal: ProposalId,
) -> Result<(ProposalStatus, Vec<TaskStatus>)> {
    let record = super::proposals::read(conn, proposal)?;
    let tasks = record
        .task_ids()
        .iter()
        .map(|&id| Ok(super::sqlite::read_task(conn, id)?.status()))
        .collect::<Result<Vec<_>>>()?;
    Ok((record.status(), tasks))
}

/// Where the answer of a `planner_question` about `finding` goes: the
/// planner of the runtime's not closed opened for the finding; else a new
/// planner while the finding still waits for one (`open`, marked, its
/// planners not used up); else closed by the supervisor (the finding
/// moved on). A finding no planner of the runtime's was ever opened for
/// leaves the answer to a person.
pub(super) fn route_of(conn: &Connection, finding: FindingId) -> Result<PlannerAnswerRoute> {
    let planner: Option<PlannerId> = conn
        .query_row(
            "SELECT id FROM planners WHERE origin='runtime' AND closed_at IS NULL
             AND finding_id=?1 ORDER BY id DESC LIMIT 1",
            [finding],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = planner {
        return Ok(PlannerAnswerRoute::Planner(conn.query_row(
            "SELECT * FROM planners WHERE id=?1",
            [id],
            super::planners::planner_row,
        )?));
    }
    // A finding the runtime never opened a planner for is not the
    // runtime's to carry: a person's planner asked about it.
    let runtime_planned: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM planners WHERE origin='runtime' AND finding_id=?1)",
        [finding],
        |r| r.get(0),
    )?;
    if !runtime_planned {
        return Ok(PlannerAnswerRoute::Person);
    }
    let current = read_finding(conn, finding)?;
    let exhausted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_events WHERE kind='finding_planner_exhausted'
         AND json_extract(payload,'$.finding_id')=?1
         AND json_extract(payload,'$.marked_at') IS ?2)",
        params![finding, current.propose_requested_at],
        |r| r.get(0),
    )?;
    Ok(
        if current.status == FindingStatus::Open && current.propose_reason.is_some() && !exhausted {
            PlannerAnswerRoute::NewPlanner
        } else {
            PlannerAnswerRoute::Close
        },
    )
}

/// Link the findings of a submission to its proposal, inside the submit's
/// transaction (ADR-0044 decision 19): the ones named (`submit --finding`)
/// and the one of the planner of the runtime's that submits from
/// `workspace`. Each `open` one becomes `proposed` with the proposal
/// (`finding_status_changed`); one already `proposed` for this proposal (a
/// resubmission) stays. A named finding that cannot be linked fails the
/// submission; the planner's own is linked only while `open`.
pub(super) fn link_findings(
    conn: &Connection,
    proposal: ProposalId,
    workspace: Option<&str>,
    named: &[FindingId],
    by: &str,
    now: i64,
) -> Result<Vec<FindingId>> {
    let mut targets: Vec<(FindingId, bool)> = named.iter().map(|&id| (id, true)).collect();
    if let Some(workspace) = workspace {
        let own: Vec<FindingId> = conn
            .prepare(
                "SELECT finding_id FROM planners WHERE workspace_id=?1 AND origin='runtime'
                 AND closed_at IS NULL AND finding_id IS NOT NULL ORDER BY id",
            )?
            .query_map([workspace], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in own {
            if !targets.iter().any(|(named, _)| *named == id) {
                targets.push((id, false));
            }
        }
    }
    let mut linked = Vec::new();
    for (id, named) in targets {
        let current = read_finding(conn, id)?;
        let link = match finding::check_link(&current, proposal) {
            Ok(link) => link,
            Err(error) if named => return Err(error.into()),
            Err(_) => continue,
        };
        if !link {
            continue;
        }
        conn.execute(
            "UPDATE findings SET status='proposed', status_reason=NULL, proposal_id=?2,
               updated_at=?3 WHERE id=?1",
            params![id, proposal, now],
        )?;
        finding_event(
            conn,
            &current,
            "finding_status_changed",
            json!({
                "finding_id": id,
                "from": current.status,
                "to": FindingStatus::Proposed,
                "reason": format!("proposal {proposal} was submitted to remedy it"),
                "proposal_id": proposal,
                "by": by,
            }),
        )?;
        linked.push(id);
    }
    Ok(linked)
}

/// What a person's answer did to a finding.
pub(super) struct AppliedAnswer {
    pub finding: FindingId,
    /// `propose` or `dismiss`.
    pub action: &'static str,
}

/// Apply a person's `propose` or `dismiss` answer to the ask's finding,
/// inside the answer's transaction (ADR-0044 decision 19), when the ask
/// offered that option. `propose` marks the finding for a proposal; an ask
/// about no finding (a `stalled` one) records a finding of its own first,
/// on the ask's run (or task, or the queue) with the ask's `ask_opened` as
/// its evidence, and names it on the ask. `dismiss` dismisses the ask's
/// finding. Only a `blocked` or `stalled` ask's answer is applied: a
/// `planner_question`'s goes to its planner. `None`: the answer is not one
/// the runtime applies.
pub(super) fn apply_answer(
    conn: &Connection,
    ask: &Ask,
    text: &str,
    by: &str,
    now: i64,
) -> Result<Option<AppliedAnswer>> {
    if !matches!(ask.kind, AskKind::Blocked | AskKind::Stalled) {
        return Ok(None);
    }
    let Some(answer) = FindingAnswer::parse(text) else {
        return Ok(None);
    };
    let offered = |option: &str| ask.options.iter().any(|o| o.trim() == option);
    match answer {
        FindingAnswer::Propose(why) if offered(crate::domain::PROPOSE_OPTION) => {
            let reason =
                why.unwrap_or_else(|| format!("a person answered propose to ask {}", ask.id));
            let id = match ask.finding_id {
                Some(id) => id,
                None => {
                    let id = record_from_ask(conn, ask, &reason, by, now)?;
                    conn.execute(
                        "UPDATE asks SET finding_id=?2 WHERE id=?1",
                        params![ask.id, id],
                    )?;
                    id
                }
            };
            let current = read_finding(conn, id)?;
            if let Some(marked) = finding::mark_for_proposal(&current, &reason, now) {
                conn.execute(
                    "UPDATE findings SET status=?2, status_reason=NULL, proposal_id=NULL,
                       propose_reason=?3, propose_requested_at=?4, updated_at=?4 WHERE id=?1",
                    params![id, marked.status.as_str(), marked.propose_reason, now],
                )?;
                if current.status != marked.status {
                    finding_event(
                        conn,
                        &marked,
                        "finding_status_changed",
                        json!({
                            "finding_id": id,
                            "from": current.status,
                            "to": marked.status,
                            "reason": reason,
                            "ask_id": ask.id,
                            "by": by,
                        }),
                    )?;
                }
                finding_event(
                    conn,
                    &marked,
                    "finding_updated",
                    json!({
                        "finding_id": id,
                        "changed": ["propose_reason"],
                        "ask_id": ask.id,
                        "by": by,
                    }),
                )?;
            }
            Ok(Some(AppliedAnswer {
                finding: id,
                action: "propose",
            }))
        }
        FindingAnswer::Dismiss(why) if offered(crate::domain::DISMISS_OPTION) => {
            let Some(id) = ask.finding_id else {
                return Ok(None);
            };
            let reason =
                why.unwrap_or_else(|| format!("a person answered dismiss to ask {}", ask.id));
            if read_finding(conn, id)?.status != FindingStatus::Dismissed {
                set_status_in(conn, id, FindingStatus::Dismissed, &reason, by, now)?;
            }
            Ok(Some(AppliedAnswer {
                finding: id,
                action: "dismiss",
            }))
        }
        _ => Ok(None),
    }
}

/// Record the finding a `propose` answer to an ask about no finding asks
/// for: of the ask's kind, on its run (or task, or the queue), with the
/// ask's `ask_opened` as its evidence and its question as the summary (its
/// first line) and the detail.
fn record_from_ask(
    conn: &Connection,
    ask: &Ask,
    reason: &str,
    by: &str,
    now: i64,
) -> Result<FindingId> {
    let target = match (&ask.run_id, ask.task_id) {
        (Some(run), _) => FindingTarget::Run(run.clone()),
        (None, Some(task)) => FindingTarget::Task(task),
        (None, None) => FindingTarget::Queue,
    };
    let opened: Option<crate::domain::EventId> = conn
        .query_row(
            "SELECT id FROM run_events WHERE kind='ask_opened'
             AND json_extract(payload,'$.ask_id')=?1 ORDER BY id LIMIT 1",
            [ask.id],
            |r| r.get(0),
        )
        .optional()?;
    let summary: String = ask
        .question
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(&ask.question)
        .chars()
        .take(200)
        .collect();
    let recorded = record_in(
        conn,
        &NewFinding {
            kind: ask.kind.as_str().replace('-', "_"),
            target,
            subject: String::new(),
            summary,
            detail: Some(ask.question.clone()),
            impact: None,
            evidence: opened.into_iter().collect(),
            propose: Some(reason.to_owned()),
            by: by.to_owned(),
        },
        now,
    )?;
    Ok(recorded.finding.id)
}
