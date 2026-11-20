//! Runtime planner routing and delivery through the supervisor and queue,
//! with the production Codex turn driver against a fake spawner.

use super::{agent::Script, harness::Wiring};
use crate::{
    application::{AskQuery, PlanReviewStore, RunFiles, TaskStore},
    domain::{
        AskId, AskKind, AskReason, EventKind, NewAsk, NewTask, PlannerId, PlannerOrigin,
        PlannerOwner, PlannerSession, ProposalId, Provider, RequestId, Submission,
        background_wrapper::BackgroundHandle,
        plan_request::{NewPlanRequest, RequestStatus},
        turn::{self, TurnOutcome, TurnRequest},
    },
    infrastructure::{location::planners_dir, sqlite::SqliteQueue},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, SystemTime},
};

fn fixture(name: &str) -> Wiring {
    Wiring::new(
        name,
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
}

fn plan(fx: &Wiring) {
    fx.supervise_planning(5, Duration::from_secs(3600));
}

fn configure(fx: &Wiring, provider: Provider) {
    fx.verifier.set_config(&format!(
        "[roles.runtime_planner]\nprovider = \"{}\"\nmodel = \"requested-model\"\neffort = \"low\"",
        provider.as_str()
    ));
}

fn request(queue: &mut SqliteQueue, text: &str) -> RequestId {
    queue
        .record_plan_request(
            &NewPlanRequest {
                text: text.into(),
                note: None,
                refs: vec![],
                priority: None,
            },
            "inbox",
            "inbox",
        )
        .unwrap()
        .id
}

fn for_request(queue: &SqliteQueue, request: RequestId) -> PlannerSession {
    queue
        .planners(false)
        .unwrap()
        .into_iter()
        .find(|row| row.request_id == Some(request))
        .expect("an open planner for the request")
}

fn revise(queue: &mut SqliteQueue, title: &str, workspace_id: Option<String>) -> ProposalId {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "Implement a small change".into(),
            acceptance: "Its regression test passes".into(),
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
        .id();
    let proposal = queue
        .submit(Submission {
            tasks: vec![task],
            goals: vec![],
            proposal: None,
            owner: PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id,
            },
        })
        .unwrap()
        .id();
    queue.send_back_proposal(proposal).unwrap();
    proposal
}

fn release_claude(queue: &mut SqliteQueue, ask: AskId) {
    queue.answer(ask, "done").unwrap();
    queue.close_ask(ask).unwrap();
}

fn payloads(queue: &SqliteQueue, kind: EventKind) -> Vec<Value> {
    queue
        .latest_events_of(kind.as_str(), 20)
        .unwrap()
        .into_iter()
        .map(|e| e.payload)
        .collect()
}

fn planner_dir(fx: &Wiring, id: PlannerId) -> PathBuf {
    planners_dir(&fx.db).join(id.to_string())
}

/// Complete the parked wrapper's first turn so the pass sees a successful,
/// idle session. The turn's actual provider is independent of today's role.
fn idle(fx: &Wiring, row: &PlannerSession, provider: Provider, since: i64) {
    let queue = fx.queue();
    let pid = BackgroundHandle::parse(row.workspace_id.as_deref().unwrap())
        .unwrap()
        .pid;
    queue.register_planner_wrapper(row.id, pid).unwrap();
    queue.register_planner_agent(row.id, pid, pid).unwrap();
    for kind in [EventKind::TurnStarted, EventKind::TurnFinished] {
        queue
            .record_queue_event(
                kind,
                json!({
                    "planner_id": row.id, "turn": 1, "provider": provider, "outcome": "succeeded"
                }),
            )
            .unwrap();
    }
    let marker = turn::idle_marker("thread", 1, TurnOutcome::Succeeded, &Default::default());
    fx.files.put(
        &crate::application::planner_idle_marker(&planner_dir(fx, row.id)),
        SystemTime::UNIX_EPOCH + Duration::from_secs((since + 1).try_into().unwrap()),
        &marker.to_string(),
    );
}

fn answer_request(queue: &mut SqliteQueue, request: RequestId) -> AskId {
    let ask = queue
        .ask(NewAsk {
            kind: AskKind::PlannerQuestion,
            task_id: None,
            run_id: None,
            question: "Which behavior should the plan keep?".into(),
            options: vec!["keep".into(), "replace".into()],
            asked_by: "planner".into(),
            reason_category: AskReason::Scope,
            topics: vec![],
            recommendation: Some("keep".into()),
            confidence: None,
            finding_id: None,
            request_id: Some(request),
        })
        .unwrap()
        .ask
        .id;
    queue.answer(ask, "keep").unwrap();
    ask
}

fn what(queue: &SqliteQueue, id: PlannerId) -> Vec<String> {
    queue
        .planner_turn_events(id)
        .unwrap()
        .iter()
        .filter(|e| e.kind == EventKind::TurnRequested)
        .map(|e| e.payload["what"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_queue_hold_routes_new_planners_to_codex_and_only_pauses_claudes_deliveries() {
    let fx = fixture("planner-queue-hold").with_codex();
    configure(&fx, Provider::Claude);
    let mut queue = fx.queue();
    let claude_revise = request(&mut queue, "Claude's revise");
    let claude_answer = request(&mut queue, "Claude's answer");
    plan(&fx);
    let claude_revise = for_request(&queue, claude_revise);
    let claude = for_request(&queue, claude_answer);
    let hold = fx.hold_claude_queue();
    let codex_revise = request(&mut queue, "Codex's revise");
    let codex_answer = request(&mut queue, "Codex's answer");
    plan(&fx);
    let codex_revise = for_request(&queue, codex_revise);
    let codex = for_request(&queue, codex_answer);
    let commands = fx.sessions.planner_commands();
    assert_eq!(commands.len(), 4);
    assert!(
        commands[..2]
            .iter()
            .all(|command| command.contains("'--claude' 'wiring-claude'"))
    );
    assert!(
        commands[2..]
            .iter()
            .all(|command| command.contains("'--codex' 'wiring-codex'"))
    );

    let claude_ask = answer_request(&mut queue, claude_answer);
    let codex_ask = answer_request(&mut queue, codex_answer);
    revise(
        &mut queue,
        "Claude revise",
        claude_revise.workspace_id.clone(),
    );
    revise(
        &mut queue,
        "Codex revise",
        codex_revise.workspace_id.clone(),
    );
    for (row, provider, since) in [
        (&claude_revise, Provider::Claude, claude_revise.created_at),
        (
            &claude,
            Provider::Claude,
            queue.read_ask(claude_ask).unwrap().created_at,
        ),
        (&codex_revise, Provider::Codex, codex_revise.created_at),
        (
            &codex,
            Provider::Codex,
            queue.read_ask(codex_ask).unwrap().created_at,
        ),
    ] {
        idle(&fx, row, provider, since);
    }
    plan(&fx);
    assert!(what(&queue, claude_revise.id).is_empty());
    assert!(what(&queue, claude.id).is_empty());
    assert!(queue.read_ask(claude_ask).unwrap().closed_at.is_none());
    assert_eq!(what(&queue, codex_revise.id), ["revise"]);
    assert_eq!(
        what(&queue, codex.id),
        [format!("answer of ask {codex_ask}")]
    );
    assert!(queue.read_ask(hold).unwrap().is_open());

    release_claude(&mut queue, hold);
    plan(&fx);
    assert_eq!(what(&queue, claude_revise.id), ["revise"]);
    assert_eq!(
        what(&queue, claude.id),
        [format!("answer of ask {claude_ask}")]
    );
    assert_eq!(fx.sessions.planner_launches(), 4);
    for sent in payloads(&queue, EventKind::PlanReviseSent) {
        assert_eq!(sent["effort_raised"], false);
        assert_eq!(sent["launch"], Value::Null);
    }
}

#[test]
fn new_planners_use_the_selected_provider_and_raise_effort_only_for_revises() {
    for (name, unavailable, no_claude, flag, before, after, switched) in [
        (
            "planner-configured",
            false,
            false,
            "'--claude' 'wiring-claude'",
            "low",
            "medium",
            Value::Null,
        ),
        (
            "planner-fallback",
            true,
            false,
            "'--codex' 'wiring-codex'",
            "medium",
            "high",
            json!("authentication"),
        ),
        (
            "planner-no-claude",
            false,
            true,
            "'--codex' 'wiring-codex'",
            "medium",
            "high",
            json!("provider_disabled"),
        ),
    ] {
        let fx = fixture(name).with_codex();
        let fx = if no_claude {
            fx.with_no_claude().without_job_fallback()
        } else {
            fx
        };
        if !no_claude {
            configure(&fx, Provider::Claude);
        }
        if unavailable {
            fx.hold_claude_queue();
        }
        let mut queue = fx.queue();
        revise(&mut queue, "Revise without an owner", None);
        let request = request(&mut queue, "Plan a request");
        plan(&fx);
        for_request(&queue, request);
        let commands = fx.sessions.planner_commands();
        assert_eq!(commands.len(), 2, "{commands:?}");
        assert!(
            commands.iter().all(|cmd| cmd.contains(flag)),
            "{commands:?}"
        );
        assert!(
            commands[0].contains(&format!("'--effort' '{after}'")),
            "{commands:?}"
        );
        if name == "planner-configured" {
            assert!(
                commands[1].contains("'--model' 'requested-model' '--effort' 'low'"),
                "{commands:?}"
            );
        } else {
            assert!(!commands[1].contains("--effort"), "{commands:?}");
        }
        let [sent] = payloads(&queue, EventKind::PlanReviseSent)
            .try_into()
            .expect("one revise");
        assert_eq!(sent["effort_raised"], true);
        assert_eq!(sent["launch"]["escalated_from"], before);
        assert_eq!(sent["launch"]["effort"], after);
        assert_eq!(sent["launch"]["switch_reason"], switched);
        if !switched.is_null() {
            assert_eq!(sent["launch"]["switched_from"], "claude");
        }
    }
}

#[test]
fn without_a_target_the_request_and_revise_wait_without_a_claim_or_attempt() {
    for name in [
        "planner-codex-missing",
        "planner-fallback-off",
        "planner-provider-unnamed",
    ] {
        let mut fx = fixture(name);
        let hold = match name {
            "planner-codex-missing" => {
                fx = fx.with_no_claude();
                None
            }
            "planner-fallback-off" => {
                fx = fx.with_codex().without_job_fallback();
                configure(&fx, Provider::Claude);
                Some(fx.hold_claude_queue())
            }
            _ => {
                fx = fx.with_codex();
                Some(fx.hold_claude_queue())
            }
        };
        let mut queue = fx.queue();
        let proposal = revise(&mut queue, "Waiting revise", None);
        let request = request(&mut queue, "Waiting request");
        plan(&fx);
        assert!(queue.planners(true).unwrap().is_empty());
        assert_eq!(queue.request_by_id(request).unwrap().planners, 0);
        let revising = queue.revising_proposals().unwrap();
        assert_eq!(revising.len(), 1);
        assert!(revising[0].sent_at.is_none());
        assert!(payloads(&queue, EventKind::PlanReviseSent).is_empty());
        if let Some(hold) = hold {
            release_claude(&mut queue, hold);
        } else {
            fx.set_codex_available(true);
        }
        plan(&fx);
        assert_eq!(queue.planners(false).unwrap().len(), 2);
        assert_eq!(queue.request_by_id(request).unwrap().planners, 1);
        assert_eq!(
            payloads(&queue, EventKind::PlanReviseSent)[0]["proposal_id"],
            json!(proposal)
        );
    }
}

#[test]
fn a_wrapper_start_error_closes_its_row_and_leaves_the_request_on_the_same_route() {
    let fx = fixture("planner-wrapper-failed").with_codex();
    configure(&fx, Provider::Claude);
    fx.fail_planner_launches(true);
    let mut queue = fx.queue();
    let id = request(&mut queue, "Request after a wrapper failure");
    plan(&fx);
    let [row] = queue
        .planners(true)
        .unwrap()
        .try_into()
        .expect("one attempted planner");
    assert!(row.closed_at.is_some());
    assert!(row.error.unwrap().contains("refuses to start the wrapper"));
    assert_eq!(fx.sessions.planner_launches(), 0);
    assert!(payloads(&queue, EventKind::ProviderHeld).is_empty());
    assert_eq!(queue.request_by_id(id).unwrap().status, RequestStatus::Open);
    fx.fail_planner_launches(false);
    plan(&fx);
    assert_ne!(for_request(&queue, id).id, row.id);
    let commands = fx.sessions.planner_commands();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].contains("'--claude' 'wiring-claude'"));
}

fn assert_codex_hold(queue: &SqliteQueue, id: PlannerId, reason: &str) {
    let held = payloads(queue, EventKind::ProviderHeld);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0]["provider"], "codex");
    assert_eq!(held[0]["reason"], reason);
    let waiting = payloads(queue, EventKind::ProviderWaiting);
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0]["provider"], "codex");
    assert_eq!(waiting[0]["reason"], reason);
    assert_eq!(waiting[0]["planner_id"], json!(id));
    assert_eq!(waiting[0]["ask_id"], Value::Null);
    assert!(
        queue
            .asks(AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .is_empty()
    );
    assert!(queue.planner(id).unwrap().closed_at.is_none());
}

#[test]
fn a_codex_spawn_refusal_becomes_a_launch_wall_inside_the_planner_wrapper() {
    let fx = fixture("planner-spawn-refused")
        .with_codex()
        .with_continuous_passes();
    configure(&fx, Provider::Codex);
    fx.sessions
        .drive_codex_planners
        .store(true, Ordering::SeqCst);
    fx.agent.planner_turns.lock().unwrap().push_back(None);
    let mut queue = fx.queue();
    let request = request(&mut queue, "A request whose first Codex spawn fails");
    let mut observed = None;
    fx.supervise_once_while_stoppable(|queue, stop| {
        let waiting = payloads(queue, EventKind::ProviderWaiting);
        if let Some(wait) = waiting.first() {
            observed = wait["planner_id"].as_i64().map(PlannerId::new);
            stop();
            return true;
        }
        false
    });
    let id = observed.expect("the real wrapper's failed turn was tended");
    let finished = payloads(&queue, EventKind::TurnFinished);
    assert_eq!(finished[0]["turn"], 1);
    assert_eq!(finished[0]["outcome"], "failed");
    assert_eq!(finished[0]["failure"], "launch");
    assert!(
        finished[0]["message"]
            .as_str()
            .unwrap()
            .contains("refuses to spawn Codex")
    );
    assert_codex_hold(&queue, id, "launch_failed");
    assert_eq!(fx.agent.planner_commands.lock().unwrap().len(), 1);
    assert_eq!(queue.request_by_id(request).unwrap().planners, 1);
}

fn codex_output(event: Value) -> String {
    format!(
        "{}\n{event}\n",
        json!({"type":"thread.started","thread_id":"planner-thread"})
    )
}

#[test]
fn a_codex_wall_retries_the_failed_request_in_the_same_thread_before_queued_work() {
    let fx = fixture("planner-retry-in-thread")
        .with_codex()
        .with_continuous_passes();
    configure(&fx, Provider::Claude);
    let hold = fx.hold_claude_queue();
    fx.sessions
        .drive_codex_planners
        .store(true, Ordering::SeqCst);
    let ok =
        codex_output(json!({"type":"turn.completed","usage":{"input_tokens":8,"output_tokens":2}}));
    let failed = codex_output(
        json!({"type":"turn.failed","error":{"message":"You've hit your usage limit."}}),
    );
    fx.agent.planner_turns.lock().unwrap().extend([
        Some((ok.clone(), 0)),
        Some((failed, 1)),
        Some((ok.clone(), 0)),
        Some((ok, 0)),
    ]);
    let mut queue = fx.queue();
    let request = request(&mut queue, "Use the alternate provider");
    // Queue both requests before the first turn completes. An idle planner
    // without work can be ended by the next supervisor pass.
    let files = fx.files.clone();
    let db = fx.db.clone();
    fx.agent.on_next_turn(Box::new(move || {
        let queue = SqliteQueue::open(&db).unwrap();
        let row = for_request(&queue, request);
        let dir = planners_dir(&db).join(row.id.to_string());
        for (seq, prompt) in [(1, "retry these words"), (2, "later words")] {
            let request = TurnRequest {
                seq,
                what: "planner request".into(),
                prompt: prompt.into(),
            };
            files
                .write(
                    &turn::request_path(&dir, seq),
                    &serde_json::to_vec(&request).unwrap(),
                )
                .unwrap();
        }
    }));
    let mut held_at = None;
    let mut released = false;
    let mut completed = None;
    fx.supervise_once_while_stoppable(|queue, stop| {
        let finished = payloads(queue, EventKind::TurnFinished);
        let Some(turn) = finished.first() else {
            return false;
        };
        let id = PlannerId::new(turn["planner_id"].as_i64().unwrap());
        if !released && !payloads(queue, EventKind::ProviderWaiting).is_empty() {
            let pass = *held_at.get_or_insert_with(|| fx.passes());
            if fx.passes() > pass + 1 {
                assert!(what(queue, id).is_empty());
                assert!(queue.planner(id).unwrap().closed_at.is_none());
                assert_eq!(queue.request_by_id(request).unwrap().planners, 1);
                // The open planner keeps Codex even though a new planner
                // would now have to wait for Claude.
                fx.verifier.set_config("");
                queue
                    .record_queue_event(EventKind::ProviderReleased, json!({"provider":"codex"}))
                    .unwrap();
                released = true;
            }
        }
        if turn["turn"] == 4 {
            completed = Some(id);
            stop();
            return true;
        }
        false
    });
    let id = completed.expect("the wrapper finished the retried turn and the one behind it");
    let opened = payloads(&queue, EventKind::SessionOpened);
    let opened = opened
        .iter()
        .find(|span| span["planner_id"] == json!(id))
        .unwrap();
    assert_eq!(opened["provider"], "codex");
    assert_eq!(opened["launch"]["switched_from"], "claude");
    assert_eq!(opened["launch"]["switch_reason"], "authentication");
    assert_eq!(queue.request_by_id(request).unwrap().planners, 1);
    assert!(
        queue.read_ask(hold).unwrap().is_open(),
        "Codex retry ignores Claude's hold"
    );
    let held = payloads(&queue, EventKind::ProviderHeld);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0]["provider"], "codex");
    assert_eq!(held[0]["reason"], "usage_limit");
    let waits = payloads(&queue, EventKind::ProviderWaiting);
    assert_eq!(waits[0]["provider"], "codex");
    assert_eq!(waits[0]["ask_id"], Value::Null);
    assert_eq!(
        queue
            .asks(AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .len(),
        1
    );
    assert_eq!(what(&queue, id), ["provider retry"]);
    let commands = fx.agent.planner_commands.lock().unwrap();
    assert_eq!(commands.len(), 4);
    assert!(
        commands[2]
            .get_stdin()
            .unwrap()
            .contains("retry these words")
    );
    assert_eq!(commands[3].get_stdin(), Some("later words"));
    for command in &commands[1..] {
        let args: Vec<_> = command.get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("resume")));
        assert!(args.contains(&std::ffi::OsStr::new("planner-thread")));
    }
}
