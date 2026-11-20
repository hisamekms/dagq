//! Initial planner handoffs through the real supervisor and queue, using
//! parked wrappers for the recovery boundaries and a real turn for spawn refusal.
use super::{
    harness::Wiring,
    runtime_planner_provider::{
        configure, fixture, for_request, payloads, plan, planner_dir, request,
    },
};
use crate::{
    application::{
        PlanReviewStore, RunFiles, SessionRegistry, TaskStore, commands::planning::Planning,
    },
    domain::{
        ActorContext, ActorRole, AskId, AskKind, AskReason, DraftOrigin, EventKind, NewAsk,
        NewNote, NewTask, NoteTarget, PlannerId, PlannerOrigin, PlannerOwner, PlannerSession,
        ProposalId, Provider, RequestId, StaticPolicy, Submission, TaskEdit, TaskId,
        actor_model::{ActorLaunch, ModelRole, REVISE_ESCALATION},
        background_wrapper::BackgroundHandle,
        follow_up::RevisitChange,
        provider_switch::SwitchReason,
        turn::{self, TurnFailure, TurnOutcome},
    },
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::json;
use std::{
    sync::atomic::Ordering,
    time::{Duration, UNIX_EPOCH},
};

const ANSWER: &str = "Retain the exact response from the inbox";
const HANDOVER: &str = "The next planner should retain this implementation note";

fn switched_fixture(name: &str) -> Wiring {
    let fx = fixture(name)
        .with_codex()
        .with_tick(Duration::from_millis(1));
    configure(&fx, Provider::Claude);
    fx
}

fn drafts(queue: &mut SqliteQueue, titles: &[&str]) -> Vec<TaskId> {
    titles
        .iter()
        .map(|title| {
            queue
                .add(NewTask {
                    title: (*title).into(),
                    description: "Preserve this draft".into(),
                    acceptance: "The replacement sees the original draft".into(),
                    verification_commands: vec!["cargo test".into()],
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
                })
                .unwrap()
                .id()
        })
        .collect()
}

fn carried_answers(fx: &Wiring, tasks: &[TaskId], request: Option<RequestId>) -> Vec<AskId> {
    let mut queue = fx.queue();
    let prior = queue.open_planner(PlannerOrigin::Runtime, None).unwrap();
    let mut author = fx
        .queue()
        .with_actor(ActorContext::instance(ActorRole::Planner, prior.id));
    author
        .add_note(NewNote {
            target: NoteTarget::Task(tasks[0]),
            text: HANDOVER.into(),
            kind: None,
            by: "planner".into(),
        })
        .unwrap();
    let mut asks = Vec::new();
    for (index, &task) in tasks.iter().enumerate() {
        let id = author
            .ask(NewAsk {
                kind: AskKind::PlannerQuestion,
                task_id: request.is_none().then_some(task),
                run_id: None,
                question: format!("Which detail should the plan keep ({index})?"),
                options: vec![],
                asked_by: "planner".into(),
                reason_category: AskReason::Scope,
                topics: vec![],
                recommendation: None,
                confidence: None,
                finding_id: None,
                request_id: request,
            })
            .unwrap()
            .ask
            .id;
        asks.push(id);
    }
    assert!(
        queue
            .planner_answer_wait(
                prior.id,
                &asks,
                &json!({"planner_id": prior.id, "asks": asks})
            )
            .unwrap()
    );
    queue.close_planner(prior.id, None).unwrap();
    for (index, ask) in asks.iter().enumerate() {
        queue.answer(*ask, &format!("{ANSWER} ({index})")).unwrap();
    }
    asks
}

fn revise_recipient(queue: &SqliteQueue, proposal: ProposalId) -> PlannerSession {
    let id = queue
        .revising_proposals()
        .unwrap()
        .into_iter()
        .find(|revise| revise.proposal.id() == proposal)
        .unwrap()
        .planner_id
        .unwrap();
    queue.planner(id).unwrap()
}

fn begin_revise(fx: &Wiring, count: usize) -> (ProposalId, TaskId, Vec<AskId>, PlannerSession) {
    let mut queue = fx.queue();
    let tasks = drafts(
        &mut queue,
        &[
            "Revise a proposal with a carried answer",
            "Keep the adjacent task",
        ][..count],
    );
    let proposal = queue
        .submit(Submission {
            tasks: tasks.clone(),
            goals: vec![],
            proposal: None,
            owner: PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: None,
            },
        })
        .unwrap()
        .id();
    queue.send_back_proposal(proposal).unwrap();
    let task = tasks[0];
    let asks = carried_answers(fx, &tasks, None);
    plan(fx);
    for ask in &asks {
        assert!(queue.read_ask(*ask).unwrap().closed_at.is_some());
    }
    (proposal, task, asks, revise_recipient(&queue, proposal))
}

fn ended_initial_turn(
    fx: &Wiring,
    row: &PlannerSession,
    provider: Provider,
    identified: bool,
    session_created: bool,
) {
    let queue = fx.queue();
    let pid = pid(row);
    queue.register_planner_wrapper(row.id, pid).unwrap();
    queue.register_planner_agent(row.id, pid, pid).unwrap();
    queue
        .record_queue_event(
            EventKind::TurnStarted,
            json!({
                "planner_id": row.id, "turn": 1, "provider": provider
            }),
        )
        .unwrap();
    if identified {
        queue.record_queue_event(EventKind::TurnSessionIdentified, json!({
            "planner_id": row.id, "turn": 1, "provider": provider, "session_id": "a-codex-thread"
        })).unwrap();
    }
    queue
        .record_queue_event(
            EventKind::TurnFinished,
            json!({
                "planner_id": row.id, "turn": 1, "provider": provider, "outcome": "failed",
                "failure": "usage_limit", "session_id": null, "session_created": session_created
            }),
        )
        .unwrap();
    let marker = turn::idle_marker(
        "failed",
        1,
        TurnOutcome::Failed,
        &turn::TurnResult {
            failure: Some(TurnFailure::UsageLimit),
            ..Default::default()
        },
    );
    fx.files.put(
        &crate::application::planner_idle_marker(&planner_dir(fx, row.id)),
        UNIX_EPOCH + Duration::from_secs(u64::try_from(row.created_at + 1).unwrap()),
        &marker.to_string(),
    );
}

fn pid(row: &PlannerSession) -> u32 {
    BackgroundHandle::parse(row.workspace_id.as_deref().unwrap())
        .unwrap()
        .pid
}

fn prompt(fx: &Wiring, id: PlannerId) -> String {
    fx.files
        .read_to_string(&planner_dir(fx, id).join(crate::application::planner::PLANNER_PROMPT_FILE))
        .unwrap()
}

fn assert_revise_handoff(
    fx: &Wiring,
    proposal: ProposalId,
    old: PlannerId,
    answers: &[AskId],
) -> PlannerSession {
    let queue = fx.queue();
    let new = revise_recipient(&queue, proposal);
    assert_ne!(old, new.id);
    assert_eq!(
        queue.planner(old).unwrap().error.as_deref(),
        Some("provider_switched")
    );
    let text = prompt(fx, new.id);
    for (index, ask) in answers.iter().enumerate() {
        assert!(
            text.contains(&format!("answer to ask {ask}: {ANSWER} ({index})")),
            "{text}"
        );
        assert_eq!(
            payloads(&queue, EventKind::AskDelivered)
                .iter()
                .filter(|event| event["ask_id"] == json!(ask))
                .count(),
            1
        );
    }
    assert_eq!(text.matches(HANDOVER).count(), 1, "{text}");
    assert_eq!(
        queue
            .planners(false)
            .unwrap()
            .into_iter()
            .filter(|planner| planner.proposal_id == Some(proposal))
            .count(),
        1
    );
    new
}

#[test]
fn a_first_wall_moves_the_revise_answers_handover_and_edit_authority() {
    let fx = switched_fixture("switch-revise");
    let (proposal, task, answers, original) = begin_revise(&fx, 2);
    ended_initial_turn(&fx, &original, Provider::Claude, false, false);
    fx.sessions
        .drive_codex_planners
        .store(true, Ordering::SeqCst);
    let db = fx.db.clone();
    fx.agent.on_next_turn(Box::new(move || {
        let queue = SqliteQueue::open(&db).unwrap();
        let row = revise_recipient(&queue, proposal);
        let started = queue.planner_turn_events(row.id).unwrap();
        let started = started
            .iter()
            .find(|event| event.kind == EventKind::TurnStarted)
            .unwrap();
        assert_eq!(started.payload["before_spawn"], true);
        assert_eq!(started.payload["turn"], 1);
    }));
    fx.agent.planner_turns.lock().unwrap().push_back(Some((
        format!(
            "{}\n{}\n",
            json!({"type": "thread.started", "thread_id": "switched-thread"}),
            json!({"type": "turn.completed", "usage": {"input_tokens": 4, "output_tokens": 1}})
        ),
        0,
    )));
    plan(&fx);
    let new = assert_revise_handoff(&fx, proposal, original.id, &answers);
    let queue = fx.queue();
    let sent = &payloads(&queue, EventKind::PlanReviseSent)[0];
    assert_eq!(sent["planner_id"], json!(new.id));
    assert_eq!(sent["opened"], true);
    assert_eq!(sent["effort_raised"], true);
    assert_eq!(sent["launch"]["provider"], "codex");
    assert_eq!(sent["launch"]["escalated_from"], "medium");
    assert_eq!(sent["launch"]["effort"], "high");
    assert_eq!(sent["launch"]["switch_reason"], "usage_limit");
    assert!(
        fx.sessions
            .planner_commands()
            .last()
            .unwrap()
            .contains("'--effort' 'high'")
    );

    for (id, allowed) in [(original.id, false), (new.id, true)] {
        let actor = ActorContext::instance(ActorRole::Planner, id);
        let mut author = fx.queue().with_actor(actor.clone());
        let result = Planning::new(&mut author, &actor, &StaticPolicy).edit(
            task,
            TaskEdit {
                description: Some("Revised by the recipient".into()),
                ..Default::default()
            },
        );
        assert_eq!(result.is_ok(), allowed, "{result:?}");
    }
}

#[test]
fn a_carried_request_moves_once_with_its_answer_and_handover() {
    let fx = switched_fixture("switch-request");
    let mut queue = fx.queue();
    let request = request(&mut queue, "Plan a request with a carried answer");
    let task = drafts(&mut queue, &["A note target"])[0];
    let asks = carried_answers(&fx, &[task], Some(request));
    plan(&fx);
    let original = for_request(&queue, request);
    ended_initial_turn(&fx, &original, Provider::Claude, false, false);
    plan(&fx);
    let new = for_request(&queue, request);
    assert_ne!(original.id, new.id);
    let text = prompt(&fx, new.id);
    let answer = format!("answer to ask {}: {ANSWER} (0)", asks[0]);
    assert_eq!(text.matches(&answer).count(), 1, "{text}");
    assert_eq!(text.matches(HANDOVER).count(), 1);
    assert_eq!(
        payloads(&queue, EventKind::AskDelivered)
            .iter()
            .filter(|event| event["ask_id"] == json!(asks[0]))
            .count(),
        1
    );

    assert_eq!(queue.request_by_id(request).unwrap().planners, 1);
    assert_eq!(payloads(&queue, EventKind::PlannerSwitchReserved).len(), 1);
}

#[test]
fn a_switched_planners_first_turn_wall_waits_without_a_second_switch() {
    let fx = switched_fixture("switch-recipient-wall");
    let (proposal, _, answers, original) = begin_revise(&fx, 1);
    ended_initial_turn(&fx, &original, Provider::Claude, false, false);

    let fx = fx.with_continuous_passes();
    fx.sessions
        .drive_codex_planners
        .store(true, Ordering::SeqCst);
    let db = fx.db.clone();
    let generators = fx.generators();
    let source = original.id;
    fx.agent.on_next_turn(Box::new(move || {
        let mut queue = SqliteQueue::open(&db).unwrap().with_generators(generators);
        let waiting = payloads(&queue, EventKind::ProviderWaiting);
        let hold = &waiting[0];
        assert_eq!(hold["planner_id"], json!(source));
        assert_eq!(hold["provider"], "claude");
        let ask = AskId::new(hold["ask_id"].as_i64().unwrap());
        // Claude must be usable again when the Codex recipient hits its wall.
        queue.answer(ask, "done").unwrap();
        queue.close_ask(ask).unwrap();
    }));
    fx.agent.planner_turns.lock().unwrap().push_back(None);

    let outcome = fx.supervise_once_while_stoppable(|queue, stop| {
        let waiting = payloads(queue, EventKind::ProviderWaiting);
        if waiting.iter().any(|event| event["provider"] == "codex") {
            stop();
            return true;
        }
        false
    });
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let queue = fx.queue();
    let new = assert_revise_handoff(&fx, proposal, original.id, &answers);
    let started = payloads(&queue, EventKind::TurnStarted);
    assert_eq!(started[0]["planner_id"], json!(new.id));
    assert_eq!(started[0]["turn"], 1);
    let finished = payloads(&queue, EventKind::TurnFinished);
    assert_eq!(finished[0]["planner_id"], json!(new.id));
    assert_eq!(finished[0]["provider"], "codex");
    assert_eq!(finished[0]["failure"], "launch");
    assert!(payloads(&queue, EventKind::TurnSessionIdentified).is_empty());
    let waiting = payloads(&queue, EventKind::ProviderWaiting);
    assert_eq!(waiting[0]["planner_id"], json!(new.id));
    assert_eq!(waiting[0]["provider"], "codex");
    assert_eq!(waiting[0]["reason"], "launch_failed");
    assert_eq!(payloads(&queue, EventKind::PlannerSwitchReserved).len(), 1);
    assert_eq!(payloads(&queue, EventKind::PlannerSwitchCompleted).len(), 1);
    assert_eq!(fx.sessions.planner_launches(), 2);
    assert!(
        !queue
            .read_ask(AskId::new(waiting[1]["ask_id"].as_i64().unwrap()))
            .unwrap()
            .is_open()
    );
}

#[test]
fn a_real_agent_spawn_refusal_switches_but_shared_preparation_and_wrapper_errors_do_not() {
    let fx = fixture("switch-agent-spawn")
        .with_codex()
        .with_tick(Duration::from_millis(1))
        .with_continuous_passes();
    configure(&fx, Provider::Codex);
    fx.sessions
        .drive_codex_planners
        .store(true, Ordering::SeqCst);
    fx.agent.planner_turns.lock().unwrap().push_back(None);
    let mut queue = fx.queue();
    let asked = request(&mut queue, "Plan after a Codex spawn refusal");
    fx.supervise_once_while_stoppable(|queue, stop| {
        if payloads(queue, EventKind::PlannerSwitchCompleted).is_empty() {
            false
        } else {
            stop();
            true
        }
    });
    let reserved = &payloads(&queue, EventKind::PlannerSwitchReserved)[0];
    assert_eq!(reserved["launch"]["provider"], "claude");
    assert_eq!(reserved["launch"]["switch_reason"], "launch_failed");
    assert_eq!(reserved["planner_id"], json!(for_request(&queue, asked).id));
    let finished = &payloads(&queue, EventKind::TurnFinished)[0];
    assert_eq!(finished["planner_id"], reserved["source"]);
    assert_eq!(finished["failure"], "launch");
    assert_eq!(fx.agent.planner_commands.lock().unwrap().len(), 1);

    for failure in ["input", "wrapper"] {
        let fx = switched_fixture(failure);
        if failure == "input" {
            fx.files.reject_writes_ending_with("prompt.txt");
        } else {
            fx.fail_planner_launches(true);
        }
        let mut queue = fx.queue();
        request(&mut queue, "Common launch error");
        plan(&fx);
        let rows = queue.planners(true).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].closed_at.is_some());
        assert!(payloads(&queue, EventKind::PlannerSwitchReserved).is_empty());
        assert!(payloads(&queue, EventKind::TurnStarted).is_empty());
    }
}

#[test]
fn a_retained_thread_or_queue_operation_keeps_the_original_provider() {
    for (name, provider, identified, created, operated) in [
        ("switch-codex-thread", Provider::Codex, true, false, false),
        (
            "switch-claude-session",
            Provider::Claude,
            false,
            true,
            false,
        ),
        ("switch-operation", Provider::Claude, false, false, true),
    ] {
        let fx = fixture(name)
            .with_codex()
            .with_tick(Duration::from_millis(1));
        configure(&fx, provider);
        let mut queue = fx.queue();
        let request = request(&mut queue, "The original planner already started");
        plan(&fx);
        let row = for_request(&queue, request);
        if operated {
            drafts(
                &mut fx
                    .queue()
                    .with_actor(ActorContext::instance(ActorRole::Planner, row.id)),
                &["A draft written by the original planner"],
            );
        }
        ended_initial_turn(&fx, &row, provider, identified, created);
        plan(&fx);
        assert_eq!(for_request(&queue, request).id, row.id);
        assert!(payloads(&queue, EventKind::PlannerSwitchReserved).is_empty());
        assert_eq!(
            payloads(&queue, EventKind::ProviderWaiting)[0]["provider"],
            json!(provider)
        );
    }
}

#[test]
fn a_failed_launch_or_a_restarted_supervisor_keeps_the_revise_with_its_original_until_handle() {
    for interrupted in [false, true] {
        let fx = switched_fixture(if interrupted {
            "switch-reservation-restart"
        } else {
            "switch-wrapper-refused"
        });
        let (proposal, _, answers, original) = begin_revise(&fx, 1);
        ended_initial_turn(&fx, &original, Provider::Claude, false, false);
        let queue = fx.queue();
        if interrupted {
            // Only the real reservation transaction survived the old pass.
            let launch = ActorLaunch::default_of(ModelRole::RuntimePlanner)
                .switched(Provider::Codex, SwitchReason::UsageLimit)
                .escalated(REVISE_ESCALATION);
            let reserved = queue
                .reserve_planner_switch(original.id, None, &launch)
                .unwrap()
                .unwrap();
            assert!(reserved.workspace_id.is_none());
        } else {
            fx.fail_planner_launches(true);
            plan(&fx);
            fx.fail_planner_launches(false);
        }
        assert_eq!(revise_recipient(&queue, proposal).id, original.id);
        assert!(queue.planner(original.id).unwrap().closed_at.is_none());
        assert!(payloads(&queue, EventKind::PlannerSwitchCompleted).is_empty());
        let previous = payloads(&queue, EventKind::PlannerSwitchReserved)[0]["planner_id"]
            .as_i64()
            .map(PlannerId::new)
            .unwrap();
        plan(&fx);
        let new = assert_revise_handoff(&fx, proposal, original.id, &answers);
        assert_ne!(new.id, previous);
        assert_eq!(
            queue.planner(previous).unwrap().error.as_deref(),
            Some("switch_not_started")
        );
        assert_eq!(payloads(&queue, EventKind::PlannerSwitchReserved).len(), 2);
    }
}

#[test]
fn wrapper_loss_after_handle_replays_the_same_inputs_and_stalls_after_three_launches() {
    let fx = switched_fixture("switch-receipt-restart");
    let (proposal, _, answers, original) = begin_revise(&fx, 1);
    ended_initial_turn(&fx, &original, Provider::Claude, false, false);
    plan(&fx);
    let mut recipient = assert_revise_handoff(&fx, proposal, original.id, &answers);
    for attempts in [2, 3] {
        fx.processes().end(pid(&recipient));
        plan(&fx);
        let new = assert_revise_handoff(&fx, proposal, original.id, &answers);
        assert_ne!(new.id, recipient.id);
        assert_eq!(
            fx.queue().planner(recipient.id).unwrap().error.as_deref(),
            Some("switch_not_started")
        );
        assert_eq!(
            payloads(&fx.queue(), EventKind::PlannerSwitchReserved).len(),
            attempts
        );
        recipient = new;
    }
    fx.processes().end(pid(&recipient));
    plan(&fx);
    let queue = fx.queue();
    let notices = queue
        .latest_events_of(EventKind::PlannerSwitchStalled.as_str(), 5)
        .unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].payload["asks"], json!(answers));
    assert_eq!(notices[0].payload["source"], json!(original.id));
    assert_eq!(notices[0].payload["proposal_id"], json!(proposal));
    assert_eq!(
        crate::domain::event_attention(&notices[0].kind, &notices[0].payload),
        Some(crate::domain::AttentionNext::CheckPlanner)
    );
    assert_eq!(payloads(&queue, EventKind::PlannerSwitchReserved).len(), 3);
    assert_ne!(revise_recipient(&queue, proposal).id, recipient.id);
}

#[test]
fn a_draft_bundle_moves_all_members_without_opening_another_attempt() {
    let fx = switched_fixture("switch-draft-bundle");
    let mut queue = fx.queue();
    let members = drafts(&mut queue, &["First bundled draft", "Second bundled draft"]);
    for &task in &members {
        queue
            .record_draft_origin(task, DraftOrigin::GoalGap, &json!({"goal_review_id": 73}))
            .unwrap();
    }
    plan(&fx);
    let original = queue.planners(false).unwrap().pop().unwrap();
    assert_eq!(queue.planner_draft_tasks(original.id).unwrap(), members);
    ended_initial_turn(&fx, &original, Provider::Claude, false, false);
    plan(&fx);
    let rows = queue.planners(false).unwrap();
    assert_eq!(rows.len(), 1);
    let new = &rows[0];
    assert_ne!(new.id, original.id);
    assert_eq!(
        payloads(&queue, EventKind::PlannerSwitchReserved)[0]["launch"]["provider"],
        "codex"
    );
    assert_eq!(queue.planner_draft_tasks(new.id).unwrap(), members);
    let text = prompt(&fx, new.id);
    assert!(
        text.contains("First bundled draft") && text.contains("Second bundled draft"),
        "{text}"
    );
    assert!(
        queue
            .draft_bundle(new.id)
            .unwrap()
            .unwrap()
            .members
            .iter()
            .all(|member| member.attempt == 1)
    );
    for task in members {
        let events = queue.task_events(task).unwrap();
        let opened: Vec<_> = events
            .iter()
            .filter(|event| event.kind == EventKind::DraftPlannerOpened)
            .collect();
        assert_eq!(opened.len(), 1, "draft {task}");
        assert_eq!(opened[0].payload["planner_id"], json!(original.id));
        assert_eq!(opened[0].payload["attempt"], 1);
    }
    assert!(queue.planner_drafts().unwrap().is_empty());
}

#[test]
fn a_single_draft_revisit_moves_without_consuming_its_context_twice() {
    let fx = switched_fixture("switch-revisit");
    let mut queue = fx.queue();
    let task = drafts(&mut queue, &["Revisit this proposal"])[0];
    // ADR-t1540-1 gives each due revisit a bundle of one draft.
    let note = "Retain the consumed revisit note";
    let revisit = queue
        .revisit_draft(
            task,
            RevisitChange::Set {
                at: fx.generators().clock.now(),
                note: Some(note.into()),
            },
            "inbox",
            "inbox",
        )
        .unwrap()
        .unwrap();
    plan(&fx);
    let original = queue.planners(false).unwrap().pop().unwrap();
    ended_initial_turn(&fx, &original, Provider::Claude, false, false);
    plan(&fx);
    let [new] = queue.planners(false).unwrap().try_into().unwrap();
    assert_ne!(new.id, original.id);
    assert_eq!(queue.planner_draft_tasks(new.id).unwrap(), [task]);
    let text = prompt(&fx, new.id);
    let context = format!(
        "Its revisit time came: {} (set by inbox inbox). What to look at then: {note}",
        revisit.revisit_at_utc
    );
    assert_eq!(text.matches(&context).count(), 1, "{text}");
    let used = payloads(&queue, EventKind::DraftRevisitDue);
    assert_eq!(used.len(), 1);
    assert_eq!(used[0]["planner_id"], json!(original.id));
    assert_eq!(
        queue.draft_bundle(new.id).unwrap().unwrap().members[0].attempt,
        1
    );
}
