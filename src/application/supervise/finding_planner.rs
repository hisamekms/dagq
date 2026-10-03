//! Planners the runtime opens for findings (ADR-0044 decision 19, carried
//! by ADR-0047): a finding marked for a proposal — by the observer's
//! `finding record --propose`, or by a person's `propose` answer to an ask
//! — gets a planner of the runtime's, within
//! `runtime_planners` of [`LoopSettings::limits`] (shared with the planners for
//! revises and drafts), the oldest mark first. The planner submits a
//! proposal that remedies it (linking the finding, which becomes
//! `proposed`), dismisses it, or asks the inbox a `planner_question`,
//! whose answer the supervisor types into its workspace (or hands to a new
//! planner when that one is gone). A planner that ends with the finding
//! undecided is followed by another, at most
//! [`crate::domain::MAX_FINDING_PLANNERS`] per mark. The end of a linked
//! proposal resolves the finding, or opens it again.

use super::*;
use crate::domain::EventKind;
use crate::{
    application::{
        FindingPlannerStart, PlannerAnswerRoute,
        planner::{PlannerView, open_draft_planner},
        prompt::{FindingPlannerMaterial, finding_planner_prompt},
    },
    domain::{
        Ask, FindingId, GoalId, ImprovementLimit, PlannerState,
        kpi::config::DEFAULT_MAX_IMPROVEMENT_PROPOSALS,
    },
};

/// No planner was opened: the improvements running reached their limit
/// (ADR-0051 decision 25), so the findings after it wait too.
#[derive(Debug)]
struct ImprovementsAtLimit(ImprovementLimit);

impl std::fmt::Display for ImprovementsAtLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} improvement proposals run (at most {}); it waits for one to end",
            self.0.running, self.0.limit
        )
    }
}

impl std::error::Error for ImprovementsAtLimit {}

impl Supervisor<'_> {
    /// Resolve the `proposed` findings whose proposal ended with a task
    /// completed, and open again those whose proposal came to nothing.
    pub(super) fn settle_findings(&mut self) -> Result<()> {
        for (finding, status) in self.queue.settle_findings()? {
            info!(
                "finding {finding}: its proposal ended; it is {}",
                status.as_str()
            );
        }
        Ok(())
    }

    /// Deliver the answer of a `planner_question` about `finding`: typed
    /// into the live planner opened for it once it stopped after asking,
    /// handed to a new planner when that one is gone (within the limit), or
    /// closed when the finding moved on.
    pub(super) fn deliver_finding_answer(
        &mut self,
        views: &[PlannerView],
        runtime_open: &mut usize,
        ask: &Ask,
        finding: FindingId,
    ) -> Result<()> {
        match self.queue.planner_answer_route(ask)? {
            PlannerAnswerRoute::Planner(planner) => {
                let Some(view) = views.iter().find(|view| view.planner.id == planner.id) else {
                    return Ok(());
                };
                // The planner that asked is typed to once it stopped after
                // asking; a question someone else opened waits only for it
                // to be idle.
                let asked_by_planner = ask.asked_by == SessionRole::Planner.as_str();
                if view.state != PlannerState::Idle
                    // One that waits for Claude gets it after its retry
                    // (ADR-t1394-2 decision 5).
                    || self.planner_at_wall(view).is_some()
                    || (asked_by_planner
                        && view.idle_since.is_none_or(|since| since < ask.created_at))
                {
                    return Ok(());
                }
                let Some(workspace) = view.planner.workspace_id.clone() else {
                    return Ok(());
                };
                // Claimed first, so two supervisors type it once (task 406).
                if self.queue.ask_delivery_failed(ask.id)?
                    || !self
                        .queue
                        .claim_planner_answer(ask.id, planner.id, &workspace)?
                {
                    return Ok(());
                }
                let text = format!(
                    "answer to ask {}: {}",
                    ask.id,
                    ask.answer.as_deref().unwrap_or_default()
                );
                self.stamp_planner_input(view);
                let what = format!("answer of ask {}", ask.id);
                match self.send_to_planner(view, &workspace, Input::Text(&text), &what) {
                    Ok(_) => {
                        self.queue.ask_delivered(ask.id, &workspace)?;
                        info!(ask_id = %ask.id, "answer of ask {} sent to planner {} of finding {finding} in workspace {workspace}", ask.id, planner.id);
                    }
                    Err(error) => {
                        warn!(ask_id = %ask.id, error = %format_args!("{error:#}"), "answer of ask {} could not be sent to planner {} in workspace {workspace}: {error:#}; it is left to the inbox", ask.id, planner.id);
                        self.queue.record_finding_event(
                            finding,
                            EventKind::AskDeliveryFailed,
                            json!({
                                "ask_id": ask.id,
                                "workspace_id": workspace,
                                "planner_id": planner.id,
                                "finding_id": finding,
                                "error": format!("{error:#}"),
                            }),
                        )?;
                    }
                }
            }
            PlannerAnswerRoute::NewPlanner
                if *runtime_open < self.limits.runtime_planners.value =>
            {
                if let Some(workspace) = self.start_finding_planner(finding, Some(ask), 0)? {
                    *runtime_open += 1;
                    self.queue.ask_delivered(ask.id, &workspace)?;
                }
            }
            // At the limit: the answer waits for a planner to end.
            PlannerAnswerRoute::NewPlanner => {}
            PlannerAnswerRoute::Close => {
                self.queue.close_planner_answer(
                    ask.id,
                    "its finding moved on and no planner of the runtime's works on it",
                )?;
                info!(ask_id = %ask.id, "ask {} closed: finding {finding} moved on", ask.id);
            }
            PlannerAnswerRoute::Person => {}
        }
        Ok(())
    }

    /// Open a planner for each finding waiting for one, the oldest mark
    /// first, while the runtime's planners are below the limit and the
    /// improvements running below theirs (ADR-0051 decision 25).
    pub(super) fn open_finding_planners(&mut self, runtime_open: &mut usize) -> Result<()> {
        let findings = self.queue.planner_findings()?;
        if findings.is_empty() {
            return Ok(());
        }
        let limit = self.max_improvement_proposals();
        for finding in findings {
            if *runtime_open >= self.limits.runtime_planners.value {
                break;
            }
            match self.start_finding_planner(finding.id, None, limit) {
                Ok(Some(_)) => *runtime_open += 1,
                Ok(None) => {}
                Err(error) if error.is::<ImprovementsAtLimit>() => {
                    tracing::debug!("finding {}: {error}", finding.id);
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// `[kpi]`'s `max_improvement_proposals`, read again each time; the
    /// default when the settings do not read.
    fn max_improvement_proposals(&self) -> usize {
        match (self.max_improvement_proposals)() {
            Ok(limit) => limit,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the [kpi] settings do not read: {error:#}; at most {DEFAULT_MAX_IMPROVEMENT_PROPOSALS} improvement proposals run");
                DEFAULT_MAX_IMPROVEMENT_PROPOSALS
            }
        }
    }

    /// Record a planner for `finding` (carrying `answer`), write its prompt
    /// and open its workspace; the workspace's UUID, or `None` when the
    /// finding was not taken (another supervisor took it, it moved on, or
    /// its planners are used up), or the error [`ImprovementsAtLimit`].
    /// `limit` is ignored when the planner carries `answer`.
    fn start_finding_planner(
        &mut self,
        finding: FindingId,
        answer: Option<&Ask>,
        limit: usize,
    ) -> Result<Option<String>> {
        let (planner, attempt) = match self.queue.open_finding_planner(
            finding,
            answer.map(|ask| ask.id),
            limit,
        )? {
            FindingPlannerStart::Skipped => return Ok(None),
            FindingPlannerStart::AtLimit(improvements) => {
                return Err(ImprovementsAtLimit(improvements).into());
            }
            FindingPlannerStart::Exhausted { attempts } => {
                warn!(
                    "finding {finding}: {attempts} planners of the runtime's ended without deciding it; the inbox is told"
                );
                return Ok(None);
            }
            FindingPlannerStart::Opened {
                planner, attempt, ..
            } => (planner, attempt),
        };
        let prompt = match self.finding_planner_material(finding, attempt, answer) {
            Ok(prompt) => prompt,
            Err(error) => {
                self.queue
                    .close_planner(planner.id, Some(&format!("{error:#}")))?;
                return Err(error);
            }
        };
        let id = planner.id;
        let opened = open_draft_planner(&self.planner_launch(), *planner, &prompt)?;
        let workspace = opened.planner.workspace_id.clone().unwrap_or_default();
        info!("finding {finding}: opened planner {id} {attempt} in workspace {workspace}");
        Ok(Some(workspace))
    }

    /// The initial prompt of the planner for `finding`, from the queue as
    /// it is now.
    fn finding_planner_material(
        &mut self,
        finding: FindingId,
        attempt: usize,
        answer: Option<&Ask>,
    ) -> Result<String> {
        let view = self.queue.finding_view(finding)?;
        let asks = self.queue.finding_asks(finding)?;
        let goal_id: Option<GoalId> = match (view.finding.goal_id, view.finding.task_id) {
            (Some(goal), _) => Some(goal),
            (None, Some(task)) => self.queue.show(task)?.task.goal_id(),
            (None, None) => None,
        };
        let goal = match goal_id {
            Some(id) => Some(self.queue.show_goal(id)?),
            None => None,
        };
        let siblings: Vec<_> = goal
            .iter()
            .flat_map(|detail| detail.tasks.iter())
            .cloned()
            .collect();
        finding_planner_prompt(&FindingPlannerMaterial {
            db: &self.layout.db,
            finding: &view,
            attempt,
            asks: &asks,
            goal: goal.as_ref().map(|detail| &detail.goal),
            goal_closed: goal.as_ref().is_some_and(|detail| detail.closed),
            siblings: &siblings,
            answer,
        })
    }
}
