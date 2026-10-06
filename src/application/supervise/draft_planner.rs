//! Planners the runtime opens for drafts (ADR-0041 decision 16, which
//! replaced the follow-up triage job of ADR-0037): the drafts the runtime
//! or a job registered (a follow_up of a landed receipt, a goal's gap, a
//! reopened task) get a planner of the runtime's, one per bundle (ADR-t807-1:
//! the drafts waiting at once that came from the same run's receipt, goal
//! review or withdrawal), within `runtime_planners` of [`LoopSettings::limits`]
//! (shared with the planners opened for revises; a bundle counts one), the
//! bundle of the oldest draft first. The planner adopts each draft
//! (completes and submits it, so plan review checks it), drops it (cancels
//! it with a note) or asks the inbox a `planner_question` about it, whose
//! answer the supervisor types into its workspace (or hands to a new
//! planner when that one is gone). The drafts a planner leaves undecided
//! make the next bundle, at most [`crate::domain::MAX_DRAFT_PLANNERS`]
//! planners per draft.

use super::*;
use crate::domain::EventKind;
use crate::{
    application::{
        DraftPlannerStart, PlannerAnswerRoute,
        planner::{PlannerView, open_draft_planner},
        prompt::{DraftPlannerMaterial, FittedPrompt, RevisitHistory, draft_planner_prompt},
    },
    domain::{
        Ask, AskKind, BundleKey, DraftOrigin, DraftTarget, follow_up::bundles,
        planner::answer_waits,
    },
};

impl Supervisor<'_> {
    /// Deliver the answered `planner_question` asks: sent to the live
    /// planner that works on the ask's task once it stopped after asking (as
    /// its next turn; typed only into a person's planner opened before
    /// `dagq plan` was abolished), handed to a new planner when the draft's
    /// one is gone (within the limit, counted in `runtime_open`), or closed
    /// when the draft moved on. The sending is claimed first
    /// (`planner_answer_claimed`), so only one supervisor sends an answer. A
    /// sending that fails records `ask_delivery_failed` and leaves the
    /// answer to the inbox.
    pub(super) fn deliver_planner_answers(
        &mut self,
        views: &[PlannerView],
        runtime_open: &mut usize,
    ) -> Result<()> {
        for ask in self.queue.planner_answers()? {
            if let Some(request) = ask.request_id {
                self.deliver_request_answer(views, runtime_open, &ask, request)?;
                continue;
            }
            if let Some(finding) = ask.finding_id {
                self.deliver_finding_answer(views, runtime_open, &ask, finding)?;
                continue;
            }
            let Some(task) = ask.task_id else {
                continue;
            };
            match self.queue.planner_answer_route(&ask)? {
                PlannerAnswerRoute::Planner(planner) => {
                    let Some(view) = views.iter().find(|view| view.planner.id == planner.id) else {
                        continue;
                    };
                    // The planner that asked is typed to once it stopped
                    // after asking; a question someone else opened about
                    // its draft waits only for it to be idle.
                    // One that waits for Claude gets it after its retry
                    // (ADR-t1394-2 decision 5).
                    if answer_waits(
                        view.state,
                        self.planner_at_wall(view).is_some(),
                        ask.asked_by == SessionRole::Planner.as_str(),
                        view.idle_since,
                        ask.created_at,
                    ) {
                        continue;
                    }
                    let Some(workspace) = view.planner.workspace_id.clone() else {
                        continue;
                    };
                    // One transaction takes the typing first, so two
                    // supervisors (across a handoff) never both type it.
                    if self.delivery_failed(task, ask.id)?
                        || !self
                            .queue
                            .claim_planner_answer(ask.id, planner.id, &workspace)?
                    {
                        continue;
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
                            info!(task_id = %task, ask_id = %ask.id, "answer of ask {} sent to planner {} in workspace {workspace}", ask.id, planner.id);
                        }
                        Err(error) => {
                            warn!(task_id = %task, ask_id = %ask.id, error = %format_args!("{error:#}"), "answer of ask {} could not be sent to planner {} in workspace {workspace}: {error:#}; it is left to the inbox", ask.id, planner.id);
                            self.queue.record_task_event(
                                task,
                                EventKind::AskDeliveryFailed,
                                json!({
                                    "ask_id": ask.id,
                                    "workspace_id": workspace,
                                    "planner_id": planner.id,
                                    "error": format!("{error:#}"),
                                }),
                            )?;
                        }
                    }
                }
                PlannerAnswerRoute::NewPlanner
                    if *runtime_open < self.limits.runtime_planners.value =>
                {
                    // The drafts of its bundle that wait go with it.
                    let mut drafts = vec![task];
                    if let Some(origin) = self.queue.draft_origin(task)? {
                        let key = BundleKey::of(origin.0, &origin.1, task);
                        drafts.extend(
                            self.queue
                                .planner_drafts()?
                                .into_iter()
                                .filter(|target| {
                                    target.task.id() != task && target.bundle_key() == key
                                })
                                .map(|target| target.task.id()),
                        );
                    }
                    if let Some(workspace) = self.start_draft_planner(&drafts, Some(&ask))? {
                        *runtime_open += 1;
                        self.queue.ask_delivered(ask.id, &workspace)?;
                    }
                }
                // At the limit: the answer waits for a planner to end.
                PlannerAnswerRoute::NewPlanner => {}
                PlannerAnswerRoute::Close => {
                    self.queue.close_planner_answer(
                        ask.id,
                        "its draft moved on and no planner of the runtime's works on it",
                    )?;
                    info!(task_id = %task, ask_id = %ask.id, "ask {} closed: draft task {task} moved on", ask.id);
                }
                PlannerAnswerRoute::Person => {}
            }
        }
        Ok(())
    }

    fn delivery_failed(&mut self, task: TaskId, ask: AskId) -> Result<bool> {
        Ok(self.queue.show(task)?.events.iter().any(|event| {
            event.kind == event_kind::ASK_DELIVERY_FAILED
                && event.payload.get("ask_id").and_then(Value::as_i64) == Some(ask.as_i64())
        }))
    }

    /// Open a planner for each bundle of drafts waiting for one, the
    /// bundle of the oldest draft first, while the runtime's planners are
    /// below the limit.
    pub(super) fn open_draft_planners(&mut self, runtime_open: &mut usize) -> Result<()> {
        for bundle in bundles(self.queue.planner_drafts()?) {
            if *runtime_open >= self.limits.runtime_planners.value {
                break;
            }
            let drafts: Vec<TaskId> = bundle.iter().map(|target| target.task.id()).collect();
            if self.start_draft_planner(&drafts, None)?.is_some() {
                *runtime_open += 1;
            }
        }
        Ok(())
    }

    /// Record a planner for the bundle of `drafts` (carrying `answer`,
    /// about the first), write its prompt and open its workspace; the
    /// workspace's UUID, or `None` when no draft was taken (another
    /// supervisor took them, they moved on, or their planners are used up).
    fn start_draft_planner(
        &mut self,
        drafts: &[TaskId],
        answer: Option<&Ask>,
    ) -> Result<Option<String>> {
        let (planner, key, members, exhausted) = match self
            .queue
            .open_draft_planner(drafts, answer.map(|ask| ask.id))?
        {
            DraftPlannerStart::Skipped => return Ok(None),
            DraftPlannerStart::Exhausted { drafts } => {
                warn!(
                    "draft tasks {drafts:?}: planners of the runtime's ended without deciding them; the inbox is told"
                );
                return Ok(None);
            }
            DraftPlannerStart::Opened {
                planner,
                key,
                members,
                exhausted,
            } => (planner, key, members, exhausted),
        };
        if !exhausted.is_empty() {
            warn!(
                "draft tasks {exhausted:?}: planners of the runtime's ended without deciding them; the inbox is told"
            );
        }
        let prompt = match self.draft_planner_material(&key, &members, answer) {
            Ok(prompt) => prompt,
            Err(error) => {
                self.queue
                    .close_planner(planner.id, Some(&format!("{error:#}")))?;
                return Err(error);
            }
        };
        let id = planner.id;
        let opened = open_draft_planner(&self.planner_launch(), *planner, ("draft", &prompt))?;
        let workspace = opened.planner.workspace_id.clone().unwrap_or_default();
        let ids: Vec<TaskId> = members.iter().map(|(target, _)| target.task.id()).collect();
        info!(
            "draft tasks {ids:?} ({} {}): opened planner {id} in workspace {workspace}",
            key.kind.as_str(),
            key.value
        );
        Ok(Some(workspace))
    }

    /// The initial prompt of the planner for the bundle `members` of `key`,
    /// from the queue as it is now.
    fn draft_planner_material(
        &mut self,
        key: &BundleKey,
        members: &[(DraftTarget, usize)],
        answer: Option<&Ask>,
    ) -> Result<FittedPrompt> {
        let first = &members
            .first()
            .context("a bundle of drafts has at least one")?
            .0;
        let source = match first.material.get("source_task_id").and_then(Value::as_i64) {
            Some(id) => Some(self.queue.show(TaskId::new(id))?.task),
            None => None,
        };
        let receipt = match (
            first.origin,
            first.material.get("source_run_id").and_then(Value::as_str),
        ) {
            (DraftOrigin::FollowUp, Some(run)) => self
                .queue
                .run_events(&RunId::new(run)?)?
                .into_iter()
                .rev()
                .find(|event| event.kind == event_kind::INTEGRATION_RECEIPT)
                .map(|event| event.payload["receipt"].clone()),
            _ => None,
        };
        let ids: Vec<TaskId> = members.iter().map(|(target, _)| target.task.id()).collect();
        let mut goals = Vec::new();
        for (target, _) in members {
            let Some(goal) = target.task.goal_id() else {
                continue;
            };
            if goals
                .iter()
                .any(|(known, _, _): &(crate::domain::Goal, bool, Vec<_>)| known.id() == goal)
            {
                continue;
            }
            let detail = self.queue.show_goal(goal)?;
            let siblings = detail
                .tasks
                .iter()
                .filter(|task| !ids.contains(&task.id))
                .cloned()
                .collect();
            goals.push((detail.goal, detail.closed, siblings));
        }
        // The last decision about each draft whose revisit time came
        // (ADR-t1540-1): its questions and its notes.
        let mut revisits = Vec::new();
        for (target, _) in members.iter().filter(|(t, _)| t.revisit.is_some()) {
            let id = target.task.id();
            let detail = self.queue.show(id)?;
            revisits.push(RevisitHistory {
                task: id,
                asks: detail
                    .asks
                    .into_iter()
                    .filter(|ask| ask.kind == AskKind::PlannerQuestion && ask.run_id.is_none())
                    .collect(),
                notes: detail
                    .events
                    .iter()
                    .filter(|event| event.kind == event_kind::OBSERVATION && event.run_id.is_none())
                    .filter_map(|event| event.payload["text"].as_str().map(str::to_owned))
                    .collect(),
            });
        }
        draft_planner_prompt(&DraftPlannerMaterial {
            db: &self.layout.db,
            key,
            members,
            source: source.as_ref(),
            receipt: receipt.as_ref(),
            goals: &goals,
            answer,
            revisits: &revisits,
        })
    }
}
