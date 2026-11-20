//! Move the work of an initial turn stopped at a provider wall. Queue
//! events own the handoff until the new wrapper records its first turn.
use super::{plan_review::once_per_planner, *};
use crate::{
    application::{planner, prompt::Carried},
    domain::{
        Ask, AskId, BundleKey, DraftRevisit, DraftTarget, EventKind, PlannerId, PlannerSession,
        ProposalId, Task,
        actor_model::{
            ActorLaunch, ModelRole, PlannerFirstTurn, PlannerSwitchConditions, REVISE_ESCALATION,
            planner_first_turn_switch,
        },
        follow_up::revisit_seconds,
        planner::{PLANNER_SWITCH_ATTEMPTS, PlannerSwitch},
        provider_switch::SwitchReason,
        task::TaskWire,
        turn::{TurnFailure, planner_resumes, planner_session_name},
    },
};
use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn planner_unusable(env: &PlanningEnv<'_>, provider: Provider) -> Option<SwitchReason> {
    env.jobs.job_unusable(provider).or_else(|| {
        env.layout
            .providers
            .iter()
            .any(|check| check.provider == provider && !check.found)
            .then_some(SwitchReason::ExecutableMissing)
    })
}

#[derive(Default)]
pub(super) struct SwitchHistory {
    /// Newest reservation first, including retired recipients.
    attempts: BTreeMap<PlannerId, Vec<PlannerSwitch>>,
    completed: BTreeSet<PlannerId>,
    stalled: BTreeSet<PlannerId>,
    received: BTreeSet<PlannerId>,
}

impl SwitchHistory {
    fn latest(&self, source: PlannerId) -> Option<&PlannerSwitch> {
        self.attempts.get(&source)?.first()
    }

    fn push(&mut self, reservation: PlannerSwitch) {
        self.attempts
            .entry(reservation.source)
            .or_default()
            .insert(0, reservation);
    }

    fn outstanding(&self, attempt: &PlannerSwitch) -> bool {
        !self.stalled.contains(&attempt.source) && !self.received.contains(&attempt.planner_id)
    }

    pub(super) fn holds_recipient(&self, planner: PlannerId) -> bool {
        self.attempts
            .values()
            .filter_map(|attempts| attempts.first())
            .any(|attempt| attempt.planner_id == planner && self.outstanding(attempt))
    }

    pub(super) fn holds_source(&self, source: PlannerId) -> bool {
        self.latest(source)
            .is_some_and(|attempt| self.outstanding(attempt))
    }

    pub(super) fn holds_proposal(&self, proposal: ProposalId) -> bool {
        self.attempts
            .values()
            .filter_map(|attempts| attempts.first())
            .any(|attempt| attempt.proposal_id == Some(proposal) && self.outstanding(attempt))
    }

    /// A handoff still takes one slot when its recipient died before
    /// receiving the work and its provider is held.
    pub(super) fn reserved_slots(&self, views: &[planner::PlannerView]) -> usize {
        self.attempts
            .values()
            .filter_map(|attempts| attempts.first())
            .filter(|attempt| {
                self.outstanding(attempt)
                    && !views.iter().any(|view| {
                        view.alive
                            && (view.planner.id == attempt.source
                                || view.planner.id == attempt.planner_id)
                    })
            })
            .count()
    }

    fn was_recipient(&self, planner: PlannerId) -> bool {
        self.completed.contains(&planner)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SwitchStep {
    Received,
    Keep,
    RetryOriginal,
    RetryRecipient,
    Stall,
}

/// Before completion the original owns the work and can wait at its wall.
/// After completion, only another launch on the reserved provider can
/// deliver the original inputs. A first turn_started ends that obligation.
fn next_switch_step(
    completed: bool,
    started: bool,
    unavailable: bool,
    attempts: usize,
) -> SwitchStep {
    if started {
        SwitchStep::Received
    } else if !unavailable {
        SwitchStep::Keep
    } else if attempts >= PLANNER_SWITCH_ATTEMPTS {
        SwitchStep::Stall
    } else if completed {
        SwitchStep::RetryRecipient
    } else {
        SwitchStep::RetryOriginal
    }
}

impl PlanningState {
    pub(super) fn recover_planner_switches(
        &mut self,
        env: &mut PlanningEnv<'_>,
    ) -> Result<SwitchHistory> {
        let mut history = SwitchHistory::default();
        for event in env.queue.planner_switch_events()? {
            match event.kind.known() {
                Some(EventKind::PlannerSwitchReserved) => {
                    history.push(serde_json::from_value(event.payload)?);
                }
                Some(EventKind::PlannerSwitchCompleted) => {
                    history
                        .completed
                        .insert(serde_json::from_value(event.payload["planner_id"].clone())?);
                }
                Some(EventKind::PlannerSwitchStalled) => {
                    history
                        .stalled
                        .insert(serde_json::from_value(event.payload["source"].clone())?);
                }
                _ => {}
            }
        }
        let originals: Vec<_> = history.attempts.keys().copied().collect();
        for source in originals {
            if history.stalled.contains(&source) {
                continue;
            }
            if let Err(error) = self.recover_switch(env, &mut history, source) {
                warn!(error = %format_args!("{error:#}"), "planner {source}: the provider handoff could not be recovered");
            }
        }
        Ok(history)
    }

    fn recover_switch(
        &mut self,
        env: &mut PlanningEnv<'_>,
        history: &mut SwitchHistory,
        source: PlannerId,
    ) -> Result<()> {
        let attempts = &history.attempts[&source];
        let latest = attempts[0].clone();
        let target = env.queue.planner(latest.planner_id)?;
        let has_completed = attempts
            .iter()
            .any(|attempt| history.completed.contains(&attempt.planner_id));
        if has_completed {
            let original = env.queue.planner(source)?;
            stop_original_wrapper(env, &original);
        }
        let started = first_turn_started(&env.queue.planner_turn_events(target.id)?)
            || (!has_completed
                && env.queue.planner_turn_events(source)?.iter().any(|event| {
                    event.kind == EventKind::TurnStarted
                        && event.payload["turn"].as_u64().is_some_and(|turn| turn > 1)
                }));
        let unavailable = !started
            && (target.closed_at.is_some()
                || target.workspace_id.is_none()
                || self.planner_gone(env, &self.judge_planner(env, target.clone())?));
        match next_switch_step(has_completed, started, unavailable, attempts.len()) {
            SwitchStep::Received => {
                history.received.insert(target.id);
            }
            SwitchStep::Keep => {}
            SwitchStep::Stall => {
                let original = env.queue.planner(source)?;
                let input = switch_inputs(env, &original)?;
                let asks: Vec<_> = input.answers.iter().map(|ask| ask.id).collect();
                if retire(env, &latest, &target, Some(&asks))? {
                    history.stalled.insert(source);
                }
            }
            SwitchStep::RetryOriginal => {
                if !retire(env, &latest, &target, None)? {
                    return Ok(());
                }
                let original = self.judge_planner(env, env.queue.planner(source)?)?;
                if self.planner_gone(env, &original) {
                    let events = env.queue.planner_turn_events(source)?;
                    if let Some(event) = events
                        .iter()
                        .rfind(|event| event.kind == EventKind::TurnFinished)
                        && let Some((provider, failure)) = recorded_wall(&event.payload)
                    {
                        self.try_first_turn_switch(
                            env, history, &original, &events, provider, failure,
                        )?;
                    }
                }
            }
            SwitchStep::RetryRecipient => {
                if !retire(env, &latest, &target, None)? {
                    return Ok(());
                }
                if planner_unusable(env, latest.launch.provider).is_none() {
                    let original = env.queue.planner(source)?;
                    self.switch_planner(env, history, &original, latest.launch)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn try_first_turn_switch(
        &mut self,
        env: &mut PlanningEnv<'_>,
        history: &mut SwitchHistory,
        view: &planner::PlannerView,
        events: &[RunEvent],
        provider: Provider,
        failure: TurnFailure,
    ) -> Result<bool> {
        let id = view.planner.id;
        let mut started = events
            .iter()
            .filter(|event| event.kind == EventKind::TurnStarted);
        let Some(first) = started.next().filter(|event| event.payload["turn"] == 1) else {
            return Ok(false);
        };
        if history.stalled.contains(&id) || started.next().is_some() {
            return Ok(false);
        }
        let role = ModelRole::RuntimePlanner;
        let roles = env.role_models(role);
        if !roles.switchable(role) || !env.jobs.fallback_jobs() {
            return Ok(false);
        }
        let resumes = planner_resumes(
            planner_session_name(&view.dir.display().to_string(), view.planner.created_at),
            provider,
            events,
            |name| {
                env.jobs
                    .reviewer()
                    .turn_session_exists(&env.layout.repo_root, name)
            },
        );
        let conditions = PlannerSwitchConditions {
            switchable: roles.switchable(role),
            no_claude: env.jobs.no_claude(),
            fallback: env.jobs.fallback_jobs(),
            resumes,
            operated: env.queue.planner_has_operations(id)?,
            already_switched: history.was_recipient(id)
                || first.payload["launch"]["switched_from"].is_string(),
        };
        let Some(PlannerFirstTurn::Switch(mut launch, _)) = planner_first_turn_switch(
            &roles.launch(role),
            provider,
            failure,
            conditions,
            |provider| planner_unusable(env, provider),
        ) else {
            return Ok(false);
        };
        if view.planner.proposal_id.is_some() {
            launch = launch.escalated(REVISE_ESCALATION);
        }
        self.switch_planner(env, history, &view.planner, launch)
    }

    fn switch_planner(
        &mut self,
        env: &mut PlanningEnv<'_>,
        history: &mut SwitchHistory,
        original: &PlannerSession,
        launch: ActorLaunch,
    ) -> Result<bool> {
        let inputs = switch_inputs(env, original)?;
        let previous = history
            .latest(original.id)
            .map(|attempt| attempt.planner_id);
        let Some(target) = env
            .queue
            .reserve_planner_switch(original.id, previous, &launch)?
        else {
            return Ok(false);
        };
        let id = target.id;
        history.push(PlannerSwitch {
            source: original.id,
            planner_id: id,
            launch: launch.clone(),
            request_id: original.request_id,
            finding_id: original.finding_id,
            proposal_id: original.proposal_id,
            drafts: env.queue.planner_draft_tasks(id)?,
        });
        match self.launch_switched_planner(env, target, inputs, launch) {
            Ok(()) => {
                history.completed.insert(id);
                stop_original_wrapper(env, original);
            }
            Err(error) => {
                env.queue.close_planner(id, Some(&format!("{error:#}")))?;
                warn!(error = %format_args!("{error:#}"), "planner {id}: its reserved wrapper did not start");
            }
        }
        Ok(true)
    }

    fn launch_switched_planner(
        &mut self,
        env: &mut PlanningEnv<'_>,
        planner: PlannerSession,
        inputs: SwitchInputs,
        launch: ActorLaunch,
    ) -> Result<()> {
        let answer = inputs.answers.first();
        let (kind, prompt, snapshot) = match inputs.work {
            SwitchWork::Request(id, attempt) => {
                let request = env.queue.plan_request(id)?;
                let (prompt, snapshot) =
                    self.request_planner_material(env, &planner, &request, attempt, answer)?;
                ("request", prompt, snapshot)
            }
            SwitchWork::Finding(id, attempt) => (
                "finding",
                self.finding_planner_material(env, id, attempt, answer)?,
                None,
            ),
            SwitchWork::Draft(key, members) => (
                "draft",
                self.draft_planner_material(env, &key, &members, answer)?,
                None,
            ),
            SwitchWork::Revise(id, reasons) => {
                let proposal = env.queue.show_proposal(id)?;
                self.open_planner_for(
                    env,
                    launch,
                    &proposal,
                    &reasons,
                    Carried {
                        answers: &inputs.answers,
                        handovers: &inputs.handovers,
                        replacement: None,
                    },
                    Some(planner),
                )?;
                return Ok(());
            }
        };
        planner::open_draft_planner(
            &Self::planner_wrapper_launch(env, launch),
            planner,
            (kind, &prompt),
            snapshot.as_deref(),
        )?;
        Ok(())
    }
}

fn retire(
    env: &PlanningEnv<'_>,
    reservation: &PlannerSwitch,
    target: &PlannerSession,
    stalled: Option<&[AskId]>,
) -> Result<bool> {
    let retired =
        env.queue
            .retire_planner_switch(reservation, target.workspace_id.as_deref(), stalled)?;
    if retired {
        stop_planner_wrapper(env, target)?;
    }
    Ok(retired)
}

fn stop_original_wrapper(env: &PlanningEnv<'_>, original: &PlannerSession) {
    if let Err(error) = stop_planner_wrapper(env, original) {
        warn!(error = %format_args!("{error:#}"), "planner {}: its switched wrapper could not be stopped", original.id);
    }
}

fn stop_planner_wrapper(env: &PlanningEnv<'_>, planner: &PlannerSession) -> Result<()> {
    if let Some(handle) = planner.workspace_id.as_deref()
        && env.sessions.exists(handle)?
    {
        stop_session(
            env.sessions,
            handle,
            crate::domain::background_wrapper::StopRoute::Planner,
        )?;
    }
    Ok(())
}

pub(super) fn recorded_wall(payload: &Value) -> Option<(Provider, TurnFailure)> {
    let provider = payload["provider"]
        .as_str()
        .and_then(|value| value.parse().ok())
        .unwrap_or(Provider::Claude);
    let failure = payload["failure"].as_str()?.parse().ok()?;
    Some((provider, failure))
}

fn first_turn_started(events: &[RunEvent]) -> bool {
    events
        .iter()
        .any(|event| event.kind == EventKind::TurnStarted && event.payload["turn"] == 1)
}

enum SwitchWork {
    Request(crate::domain::RequestId, usize),
    Finding(crate::domain::FindingId, usize),
    Draft(BundleKey, Vec<(DraftTarget, usize)>),
    Revise(ProposalId, Vec<String>),
}

struct SwitchInputs {
    work: SwitchWork,
    answers: Vec<Ask>,
    handovers: Vec<crate::domain::PlannerHandover>,
}

fn events_of(env: &PlanningEnv<'_>, kind: EventKind) -> Result<Vec<RunEvent>> {
    env.queue.latest_events_of(kind.as_str(), usize::MAX >> 1)
}

fn opened_event(env: &PlanningEnv<'_>, kind: EventKind, planner: PlannerId) -> Result<RunEvent> {
    events_of(env, kind)?
        .into_iter()
        .find(|event| event.payload["planner_id"] == json!(planner))
        .with_context(|| format!("planner {planner} has no {kind} record"))
}

/// Rebuild the initial input from the original open/delivery records.
/// Requests and findings name their carried ask at open. A draft's open
/// also records its own material and the revisit consumed for the bundle.
/// A revise records its reasons and the asks closed in its delivery
/// transaction. Closed asks and their handovers are read as material.
fn switch_inputs(env: &mut PlanningEnv<'_>, original: &PlannerSession) -> Result<SwitchInputs> {
    let id = original.id;
    let (work, ask_ids) = if let Some(request) = original.request_id {
        let event = opened_event(env, EventKind::RequestPlannerOpened, id)?;
        (
            SwitchWork::Request(
                request,
                serde_json::from_value(event.payload["attempt"].clone())?,
            ),
            open_answer(&event.payload)?,
        )
    } else if let Some(finding) = original.finding_id {
        let event = opened_event(env, EventKind::FindingPlannerOpened, id)?;
        (
            SwitchWork::Finding(
                finding,
                serde_json::from_value(event.payload["attempt"].clone())?,
            ),
            open_answer(&event.payload)?,
        )
    } else if let Some(proposal) = original.proposal_id {
        let event = opened_event(env, EventKind::PlanReviseSent, id)?;
        anyhow::ensure!(
            event.payload["opened"] == true && event.payload["proposal_id"] == json!(proposal),
            "planner {id} did not open for its revise"
        );
        let reasons = match event.payload.get("reasons") {
            Some(reasons) => serde_json::from_value(reasons.clone())?,
            None => {
                env.queue
                    .revising_proposals()?
                    .into_iter()
                    .find(|revise| revise.proposal.id() == proposal)
                    .context("the proposal is no longer revising")?
                    .reasons
            }
        };
        let asks = match event.payload.get("answers") {
            Some(answers) => serde_json::from_value(answers.clone())?,
            None => {
                // Older deliveries wrote the workspace only. A new
                // planner has a unique handle and has taken no follow-up.
                let mut deliveries = events_of(env, EventKind::AskDelivered)?;
                deliveries.reverse();
                deliveries
                    .into_iter()
                    .filter(|delivered| {
                        delivered.id > event.id
                            && delivered.payload["workspace_id"] == event.payload["workspace_id"]
                    })
                    .map(|delivered| {
                        Ok(serde_json::from_value(delivered.payload["ask_id"].clone())?)
                    })
                    .collect::<Result<Vec<_>>>()?
            }
        };
        (SwitchWork::Revise(proposal, reasons), asks)
    } else {
        let lead = original
            .draft_task_id
            .context("planner has no work to hand off")?;
        let bundle = env
            .queue
            .draft_bundle(id)?
            .context("planner has no draft bundle")?;
        let key = BundleKey {
            kind: bundle.key_kind.parse()?,
            value: bundle.key_value,
        };
        let lead_opened = draft_opened(id, &env.queue.task_events(lead)?)?.clone();
        let asks = open_answer(&lead_opened.payload)?;
        let tasks: Vec<crate::domain::TaskId> =
            serde_json::from_value(lead_opened.payload["members"].clone())?;
        let recorded = env.queue.planner_draft_tasks(id)?;
        anyhow::ensure!(
            tasks.first() == Some(&lead)
                && tasks.len() == recorded.len()
                && tasks.iter().all(|id| recorded.contains(id)),
            "planner {id} has inconsistent draft bundle members"
        );
        let mut members = Vec::new();
        for task in tasks {
            let detail = env.queue.show(task)?;
            let opened = draft_opened(id, &detail.events)?;
            members.push(restore_member(
                detail.task,
                &opened.payload,
                &detail.events,
                original.id,
                original.created_at,
            )?);
        }
        (SwitchWork::Draft(key, members), asks)
    };
    read_carried(
        work,
        ask_ids,
        |ask| env.queue.read_ask(ask),
        |ask| env.queue.planner_handover(ask),
    )
}

fn read_carried(
    work: SwitchWork,
    ask_ids: Vec<AskId>,
    read_ask: impl Fn(AskId) -> Result<Ask>,
    read_handover: impl Fn(AskId) -> Result<Option<crate::domain::PlannerHandover>>,
) -> Result<SwitchInputs> {
    let mut answers = Vec::new();
    let mut handovers = Vec::new();
    for id in ask_ids {
        answers.push(read_ask(id)?);
        handovers.extend(read_handover(id)?);
    }
    Ok(SwitchInputs {
        work,
        answers,
        handovers: once_per_planner(handovers),
    })
}

fn draft_opened(planner: PlannerId, events: &[RunEvent]) -> Result<&RunEvent> {
    events
        .iter()
        .find(|event| {
            event.kind == EventKind::DraftPlannerOpened
                && event.payload["planner_id"] == json!(planner)
        })
        .context("draft has no open record for this planner")
}

fn open_answer(opened: &Value) -> Result<Vec<AskId>> {
    Ok(
        serde_json::from_value::<Option<AskId>>(opened["ask_id"].clone())?
            .into_iter()
            .collect(),
    )
}

fn restore_member(
    task: Task,
    opened: &Value,
    events: &[RunEvent],
    original: PlannerId,
    opened_at: i64,
) -> Result<(DraftTarget, usize)> {
    let mut task = TaskWire::from(task);
    task.goal_id = serde_json::from_value(opened["goal_id"].clone())?;
    let attempt: usize = serde_json::from_value(opened["attempt"].clone())?;
    let revisit = if let Some(at) = opened["revisit_at"].as_str() {
        let due = events
            .iter()
            .find(|event| {
                event.kind == EventKind::DraftRevisitDue
                    && event.payload["planner_id"] == json!(original)
            })
            .context("the consumed draft revisit has no due event")?;
        Some(DraftRevisit {
            task_id: task.id,
            revisit_at: revisit_seconds(at).context("invalid recorded revisit time")?,
            revisit_at_utc: at.to_owned(),
            note: serde_json::from_value(due.payload["note"].clone())?,
            set_by: serde_json::from_value(opened["revisit_set_by"].clone())?,
            set_by_id: serde_json::from_value(due.payload["set_by_id"].clone())?,
            created_at: opened_at,
            opened_at: Some(opened_at),
            planner_id: Some(original),
        })
    } else {
        None
    };
    Ok((
        DraftTarget {
            task: task.into(),
            origin: serde_json::from_value(opened["origin"].clone())?,
            material: opened.clone(),
            planners: attempt,
            revisit,
        },
        attempt,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, NewTask, TaskId};

    #[test]
    fn switch_recovery_follows_reservation_handle_and_first_turn() {
        assert_eq!(
            next_switch_step(false, false, true, 1),
            SwitchStep::RetryOriginal
        );
        assert_eq!(next_switch_step(true, false, false, 1), SwitchStep::Keep);
        assert_eq!(
            next_switch_step(true, false, true, 2),
            SwitchStep::RetryRecipient
        );
        assert_eq!(next_switch_step(true, false, true, 3), SwitchStep::Stall);
        assert_eq!(next_switch_step(true, true, true, 3), SwitchStep::Received);
    }

    #[test]
    fn recorded_request_and_revise_inputs_keep_closed_answers_and_one_handover() {
        let read_ask = |id| {
            Ok(serde_json::from_value(json!({
                "id": id, "kind": "planner_question", "task_id": null, "run_id": null,
                "question": "What should be kept?", "options": [], "answer": format!("Keep detail {id}"),
                "asked_by": "planner", "reason_category": "scope",
                "created_at": 5, "answered_at": 6, "closed_at": 7,
            }))?)
        };
        let handover = |_: AskId| {
            Ok(Some(crate::domain::PlannerHandover {
                planner_id: PlannerId::new(3),
                notes: vec![(11, "Keep the notes".into())],
                drafts: vec![],
            }))
        };

        let request = crate::domain::RequestId::new(5);
        let request_input = read_carried(
            SwitchWork::Request(request, 2),
            open_answer(&json!({"ask_id": 8})).unwrap(),
            read_ask,
            handover,
        )
        .unwrap();
        assert!(matches!(request_input.work, SwitchWork::Request(id, 2) if id == request));
        assert_eq!(request_input.answers[0].id, AskId::new(8));
        assert_eq!(request_input.answers[0].closed_at, Some(7));
        assert_eq!(
            request_input.answers[0].answer.as_deref(),
            Some("Keep detail 8")
        );
        assert_eq!(request_input.handovers[0].notes[0].1, "Keep the notes");

        let proposal = ProposalId::new(9);
        let revise =
            json!({"proposal_id": proposal, "reasons": ["Preserve results"], "answers": [10, 12]});
        let revise_input = read_carried(
            SwitchWork::Revise(
                proposal,
                serde_json::from_value(revise["reasons"].clone()).unwrap(),
            ),
            serde_json::from_value(revise["answers"].clone()).unwrap(),
            read_ask,
            handover,
        )
        .unwrap();
        assert!(matches!(revise_input.work, SwitchWork::Revise(id, reason)
            if id == proposal && reason == ["Preserve results"]));
        assert_eq!(
            revise_input
                .answers
                .iter()
                .map(|ask| ask.answer.as_deref())
                .collect::<Vec<_>>(),
            [Some("Keep detail 10"), Some("Keep detail 12")]
        );
        assert!(
            revise_input
                .answers
                .iter()
                .all(|ask| ask.closed_at == Some(7))
        );
        assert_eq!(revise_input.handovers.len(), 1);
        assert_eq!(revise_input.handovers[0].planner_id, PlannerId::new(3));
    }

    #[test]
    fn restores_each_members_recorded_revisit_and_attempt() {
        let original = PlannerId::new(8);
        let members = [TaskId::new(11), TaskId::new(13)];
        for (index, id) in members.into_iter().enumerate() {
            let task = Task::new(
                id,
                NewTask {
                    title: format!("Draft {id}"),
                    description: "Preserve drafted work".into(),
                    acceptance: "Its prompt contains the revisit".into(),
                    verification_commands: vec![],
                    dependencies: vec![],
                    goal_dependencies: vec![],
                    goal_id: None,
                    context: String::new(),
                    required_evidence: vec![],
                    paths: vec![],
                    priority: None,
                    change: None,
                    provider: None,
                    worker_mode: None,
                    wait_for_build: false,
                    execution_class: Default::default(),
                },
                "2026-11-21T00:00:00Z".into(),
            )
            .unwrap();
            let note = format!("revisit {id}");
            let opened = json!({
                "planner_id": original, "members": members, "origin": "follow_up",
                "source_task_id": 5, "goal_id": 9, "attempt": index + 2,
                "revisit_at": "2026-11-20T23:10:00Z", "revisit_set_by": "inbox",
                "ask_id": if index == 0 { Some(7) } else { None },
            });
            let due = RunEvent {
                id: EventId::new(1),
                task_id: Some(id),
                goal_id: None,
                run_id: None,
                kind: EventKind::DraftRevisitDue.into(),
                payload: json!({"planner_id": original, "note": note, "set_by_id": "inbox:on-call"}),
                created_at: "2026-11-21T00:00:00Z".into(),
                actor: None,
            };
            let (target, attempt) = restore_member(task, &opened, &[due], original, 42).unwrap();
            assert_eq!(attempt, index + 2);
            assert_eq!(target.task.id(), id);
            assert_eq!(target.task.goal_id(), Some(crate::domain::GoalId::new(9)));
            assert_eq!(target.material["source_task_id"], 5);
            let revisit = target.revisit.unwrap();
            assert_eq!(revisit.note.as_deref(), Some(note.as_str()));
            assert_eq!(revisit.opened_at, Some(42));
            assert_eq!(revisit.planner_id, Some(original));
            assert_eq!(revisit.set_by, "inbox");
            assert_eq!(revisit.set_by_id, "inbox:on-call");
        }
    }
}
