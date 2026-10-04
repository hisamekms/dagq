//! Planners the runtime opens for planning requests (ADR-t1394-1
//! decisions 4 to 7): a request the inbox recorded at a person's word gets
//! a planner of the runtime's, within `runtime_planners` of
//! [`LoopSettings::limits`] (shared with the planners for revises, drafts
//! and findings), the oldest request first. The planner reads the
//! request's words from the file they were handed over in
//! ([`crate::application::planner_handoff`]), and submits a proposal of it
//! (the request becomes `proposed`, linked to it), declines it with a
//! reason, or asks the inbox a `planner_question`, whose answer the
//! supervisor types into its workspace (or hands to a new planner when
//! that one is gone). A planner that ends with the request undecided is
//! followed by another, at most
//! [`crate::domain::plan_request::MAX_REQUEST_PLANNERS`]; then the request
//! is `exhausted` and the inbox decides.

use super::*;
use crate::domain::EventKind;
use crate::{
    application::{
        PlannerAnswerRoute, RequestPlannerStart,
        planner::{PlannerView, open_draft_planner, planner_dir},
        planner_handoff::hand_request_to_planner,
        prompt::{
            FittedPrompt, RequestPlannerMaterial, RequestRefMaterial, request_planner_prompt,
        },
    },
    domain::{
        Ask, FindingId, GoalId, PlannerSession, PlannerState, RequestId, TaskId,
        plan_request::{PlanRequest, RequestRef},
    },
};

impl Supervisor<'_> {
    /// Deliver the answer of a `planner_question` about `request`: typed
    /// into the live planner opened for it once it stopped after asking,
    /// handed to a new planner when that one is gone (within the limit),
    /// or closed when the request moved on (proposed, declined, out of
    /// planners).
    pub(super) fn deliver_request_answer(
        &mut self,
        views: &[PlannerView],
        runtime_open: &mut usize,
        ask: &Ask,
        request: RequestId,
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
                // Typed into an interactive planner, the next turn of a
                // headless one (ADR-t1394-2).
                let what = format!("answer of ask {}", ask.id);
                match self.send_to_planner(view, &workspace, Input::Text(&text), &what) {
                    Ok(_) => {
                        self.queue.ask_delivered(ask.id, &workspace)?;
                        info!(ask_id = %ask.id, "answer of ask {} sent to planner {} of request {request} in workspace {workspace}", ask.id, planner.id);
                    }
                    Err(error) => {
                        warn!(ask_id = %ask.id, error = %format_args!("{error:#}"), "answer of ask {} could not be sent to planner {} in workspace {workspace}: {error:#}; it is left to the inbox", ask.id, planner.id);
                        self.queue.record_queue_event(
                            EventKind::AskDeliveryFailed,
                            json!({
                                "ask_id": ask.id,
                                "workspace_id": workspace,
                                "planner_id": planner.id,
                                "request_id": request,
                                "error": format!("{error:#}"),
                            }),
                        )?;
                    }
                }
            }
            PlannerAnswerRoute::NewPlanner
                if *runtime_open < self.limits.runtime_planners.value =>
            {
                if let Some(workspace) = self.start_request_planner(request, Some(ask))? {
                    *runtime_open += 1;
                    self.queue.ask_delivered(ask.id, &workspace)?;
                }
            }
            // At the limit: the answer waits for a planner to end.
            PlannerAnswerRoute::NewPlanner => {}
            PlannerAnswerRoute::Close => {
                self.queue.close_planner_answer(
                    ask.id,
                    "its request moved on and no planner of the runtime's works on it",
                )?;
                info!(ask_id = %ask.id, "ask {} closed: request {request} moved on", ask.id);
            }
            PlannerAnswerRoute::Person => {}
        }
        Ok(())
    }

    /// Open a planner for each request waiting for one, the oldest first,
    /// while the runtime's planners are below the limit.
    pub(super) fn open_request_planners(&mut self, runtime_open: &mut usize) -> Result<()> {
        for request in self.queue.planner_requests()? {
            if *runtime_open >= self.limits.runtime_planners.value {
                break;
            }
            if self.start_request_planner(request.id, None)?.is_some() {
                *runtime_open += 1;
            }
        }
        Ok(())
    }

    /// Record a planner for `request` (carrying `answer`), hand it the
    /// request, write its prompt and open its workspace; the workspace's
    /// UUID, or `None` when the request was not taken (another supervisor
    /// took it, it moved on, or its planners are used up).
    fn start_request_planner(
        &mut self,
        request: RequestId,
        answer: Option<&Ask>,
    ) -> Result<Option<String>> {
        let (planner, current, attempt) = match self
            .queue
            .open_request_planner(request, answer.map(|ask| ask.id))?
        {
            RequestPlannerStart::Skipped => return Ok(None),
            RequestPlannerStart::Exhausted { attempts } => {
                warn!(
                    "request {request}: {attempts} planners of the runtime's ended without deciding it; the inbox is told"
                );
                return Ok(None);
            }
            RequestPlannerStart::Opened {
                planner,
                request,
                attempt,
            } => (planner, request, attempt),
        };
        let prompt = match self.request_planner_material(&planner, &current, attempt, answer) {
            Ok(prompt) => prompt,
            Err(error) => {
                self.queue
                    .close_planner(planner.id, Some(&format!("{error:#}")))?;
                return Err(error);
            }
        };
        let id = planner.id;
        let opened = open_draft_planner(&self.planner_launch(), *planner, ("request", &prompt))?;
        let workspace = opened.planner.workspace_id.clone().unwrap_or_default();
        info!("request {request}: opened planner {id} {attempt} in workspace {workspace}");
        Ok(Some(workspace))
    }

    /// The initial prompt of `planner` for `request`, from the queue as it
    /// is now, after handing it the request's words.
    fn request_planner_material(
        &mut self,
        planner: &PlannerSession,
        request: &PlanRequest,
        attempt: usize,
        answer: Option<&Ask>,
    ) -> Result<FittedPrompt> {
        let dir = planner_dir(&self.layout.planners_dir, planner.id);
        let handed = hand_request_to_planner(
            &*self.files,
            &dir,
            &format!("request-{}", request.id),
            &request.text,
        )?;
        let mut refs = Vec::new();
        let mut goal_ids: Vec<GoalId> = Vec::new();
        for reference in &request.refs {
            let read = self.request_ref_material(reference, &mut goal_ids);
            refs.push(read.unwrap_or_else(|error| RequestRefMaterial::Unreadable {
                reference: reference.clone(),
                error: format!("{error:#}"),
            }));
        }
        let mut goals = Vec::new();
        for goal in goal_ids {
            // A goal the request names but the queue does not have is
            // left out; its reference says so.
            if let Ok(detail) = self.queue.show_goal(goal) {
                goals.push((detail.goal, detail.closed, detail.tasks));
            }
        }
        let asks = self.queue.request_asks(request.id)?;
        request_planner_prompt(&RequestPlannerMaterial {
            db: &self.layout.db,
            request,
            handed: &handed.sentence,
            attempt,
            refs: &refs,
            goals: &goals,
            asks: &asks,
            answer,
        })
    }

    /// What `reference` holds, adding the goal it leads to to `goals`.
    fn request_ref_material(
        &mut self,
        reference: &RequestRef,
        goals: &mut Vec<GoalId>,
    ) -> Result<RequestRefMaterial> {
        let mut lead_to = |goal: Option<GoalId>| {
            if let Some(goal) = goal
                && !goals.contains(&goal)
            {
                goals.push(goal);
            }
        };
        Ok(match reference {
            RequestRef::Ask(id) => {
                let ask = self.queue.read_ask(*id)?;
                lead_to(self.goal_led_to(None, ask.task_id, ask.finding_id)?);
                RequestRefMaterial::Ask(ask)
            }
            RequestRef::Task(id) => {
                let detail = self.queue.show(*id)?;
                lead_to(detail.task.goal_id());
                let receipt = detail
                    .events
                    .iter()
                    .rev()
                    .find(|event| event.kind == event_kind::INTEGRATION_RECEIPT)
                    .map(|event| event.payload["receipt"].clone());
                RequestRefMaterial::Task {
                    task: Box::new(detail.task),
                    receipt,
                }
            }
            RequestRef::Run(run) => {
                let events = self.queue.run_events(run)?;
                anyhow::ensure!(!events.is_empty(), "run {run} has no events in this queue");
                let task = events.iter().find_map(|event| event.task_id);
                lead_to(self.goal_led_to(None, task, None)?);
                let receipt = events
                    .iter()
                    .rev()
                    .find(|event| event.kind == event_kind::INTEGRATION_RECEIPT)
                    .map(|event| event.payload["receipt"].clone());
                RequestRefMaterial::Run {
                    run: run.clone(),
                    task,
                    receipt,
                }
            }
            RequestRef::Event(id) => {
                let event = self
                    .queue
                    .event_by_id(*id)?
                    .with_context(|| format!("event {id} does not exist"))?;
                lead_to(self.goal_led_to(event.goal_id, event.task_id, None)?);
                RequestRefMaterial::Event(event)
            }
            RequestRef::Finding(id) => {
                let view = self.queue.finding_view(*id)?;
                lead_to(self.goal_led_to(view.finding.goal_id, view.finding.task_id, None)?);
                RequestRefMaterial::Finding(Box::new(view))
            }
            RequestRef::Goal(id) => {
                lead_to(Some(*id));
                RequestRefMaterial::Goal(*id)
            }
        })
    }

    /// The goal what a reference names leads to (ADR-t1394-1 decision 5):
    /// its own `goal`, else its task's, else the goal its finding leads to
    /// (the finding's goal, or its task's).
    fn goal_led_to(
        &mut self,
        goal: Option<GoalId>,
        task: Option<TaskId>,
        finding: Option<FindingId>,
    ) -> Result<Option<GoalId>> {
        if goal.is_some() {
            return Ok(goal);
        }
        if let Some(task) = task
            && let Some(goal) = self.queue.show(task)?.task.goal_id()
        {
            return Ok(Some(goal));
        }
        match finding {
            Some(finding) => {
                let view = self.queue.finding_view(finding)?;
                self.goal_led_to(view.finding.goal_id, view.finding.task_id, None)
            }
            None => Ok(None),
        }
    }
}
