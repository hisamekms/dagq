use super::*;
use crate::{
    application::{commands::replans::Replans, unix_seconds},
    domain::{
        ActorContext, ClaimOutcome, CommitSha, SessionRole,
        authorization::StaticPolicy,
        provider_switch::{ProviderHold, SwitchReason},
        task_replan::{NewReplan, ReplanMove, ReplanResolution, ReplanStatus, ResolveReplan},
    },
};

fn inbox(fx: &Wiring) -> Value {
    fx.planning_status(Some(SessionRole::Inbox))
}

fn request_row(status: &Value, request: RequestId) -> Option<&Value> {
    status["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == json!(request))
}

fn waits(status: &Value) -> Vec<&Value> {
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == "planner_start_blocked")
        .collect()
}

fn assert_wait(status: &Value, requests: &[RequestId], proposal: ProposalId, providers: &Value) {
    let [wait] = waits(status).try_into().expect("one standing start wait");
    assert_eq!(wait["requests"], json!(requests));
    assert_eq!(wait["proposals"], json!([proposal]));
    assert_eq!(&wait["providers"], providers);
    assert_eq!(wait["next"], "check planner providers");
    for &request in requests {
        let row = request_row(status, request).expect("open waiting request");
        assert_eq!(row["waiting_for"], "no_provider");
        assert_eq!(&row["provider_reasons"], providers);
    }
}

fn drive(fx: &Wiring, mut step: impl FnMut(&mut SqliteQueue) -> bool) {
    let mut panic = None;
    let outcome = fx.supervise_planning_while(5, Duration::from_secs(3600), |queue, stop| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step(queue))) {
            Ok(false) => false,
            Ok(true) => {
                stop();
                true
            }
            Err(error) => {
                panic = Some(error);
                stop();
                true
            }
        }
    });
    if let Some(error) = panic {
        std::panic::resume_unwind(error);
    }
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// Wait for a complete pass after observing the first blocked record.
fn repeated_pass(fx: &Wiring, blocked: &[Value], seen: &mut Option<u64>) -> bool {
    !blocked.is_empty() && fx.passes() >= *seen.get_or_insert_with(|| fx.passes()) + 2
}

#[test]
fn no_claude_and_no_codex_publish_one_wait_until_codex_starts_the_work() {
    let fx = fixture("start-wait-no-claude")
        .with_no_claude()
        .with_codex()
        .with_continuous_passes()
        .with_tick(Duration::from_millis(1));
    let mut queue = fx.queue();
    let since = unix_seconds(fx.generators().clock.system_time());
    let hold = ProviderHold {
        provider: Provider::Codex,
        reason: SwitchReason::UsageLimit,
        since,
        retry_at: since + 3600,
    };
    queue
        .record_queue_event(EventKind::ProviderHeld, hold.held_payload(None))
        .unwrap();
    let proposal = revise(&mut queue, "Waiting revise", None);
    let waiting = request(&mut queue, "Waiting request");
    let questioned = request(&mut queue, "Waiting for a person");
    ask_request(&mut queue, questioned);
    let providers = json!([
        {"provider": "claude", "reason": "provider_disabled"},
        {"provider": "codex", "reason": "usage_limit"}
    ]);

    let mut seen = None;
    let mut released = false;
    drive(&fx, |queue| {
        if released {
            return !payloads(queue, EventKind::PlannerStartResumed).is_empty();
        }
        let blocked = payloads(queue, EventKind::PlannerStartBlocked);
        if !repeated_pass(&fx, &blocked, &mut seen) {
            return false;
        }
        assert!(queue.planners(true).unwrap().is_empty());
        assert_eq!(
            blocked,
            [json!({"providers": providers, "requests": [waiting], "proposals": [proposal]})]
        );
        let events = crate::application::watch::events_in(
            queue,
            &crate::application::watch::EventsQuery {
                after: crate::domain::EventId::new(0),
                limit: 100,
                all: false,
                full: false,
                filter: Default::default(),
            },
        )
        .unwrap();
        let event = events["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["kind"] == "planner_start_blocked")
            .expect("the event is visible to attention readers");
        assert_eq!(event["proposals"], json!([proposal]));
        assert_eq!(event["next"], "check planner providers");
        let status = inbox(&fx);
        assert_wait(&status, &[waiting], proposal, &providers);
        assert_wait(&fx.planning_status(None), &[waiting], proposal, &providers);
        let row = request_row(&status, questioned).unwrap();
        assert_eq!(row["waiting_for"], "answer");
        assert!(row.get("provider_reasons").is_none());

        queue
            .record_queue_event(EventKind::ProviderReleased, json!({"provider": "codex"}))
            .unwrap();
        released = true;
        false
    });
    assert_eq!(
        payloads(&queue, EventKind::PlannerStartResumed),
        [json!({})]
    );
    assert_eq!(queue.planners(false).unwrap().len(), 2);
    assert_eq!(
        payloads(&queue, EventKind::PlanReviseSent)[0]["proposal_id"],
        json!(proposal)
    );
    let status = inbox(&fx);
    assert!(waits(&status).is_empty());
    let row = request_row(&status, waiting).unwrap();
    assert_eq!(row["waiting_for"], "planner");
    assert!(row.get("provider_reasons").is_none());
    assert_eq!(
        request_row(&status, questioned).unwrap()["waiting_for"],
        "answer"
    );
}

#[test]
fn a_wait_on_the_explicit_provider_tracks_request_arrival_and_withdrawal() {
    let fx = fixture("start-wait-fallback-off")
        .with_codex()
        .without_job_fallback()
        .with_continuous_passes()
        .with_tick(Duration::from_millis(1));
    configure(&fx, Provider::Claude);
    let hold = fx.hold_claude_queue();
    fx.ready_task(&["cargo test"]);
    let mut queue = fx.queue();
    let base = CommitSha::try_from("0000000000000000000000000000000000000001").unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&base).unwrap() else {
        panic!("claim the task to replan");
    };
    let actor = ActorContext::user();
    let recorded = Replans::new(&mut queue, &actor, &StaticPolicy)
        .request(&NewReplan {
            run: run.id().clone(),
            reason: "split the work".into(),
            evidence: vec![],
        })
        .unwrap();
    let waiting = recorded.request.unwrap().id;
    let mut op = recorded.operation;
    // Settle the park through the same store transitions as the runtime.
    let step = |to| ReplanMove {
        to,
        reason: "park for planning".into(),
        snapshot_id: (to == ReplanStatus::Parked).then(|| "saved-work".into()),
        diagnosis: None,
    };
    for phase in [ReplanStatus::PauseRequested, ReplanStatus::Snapshotting] {
        op = queue
            .move_replan(op.id, op.generation, &step(phase))
            .unwrap();
    }
    (op, _) = queue
        .settle_replan(
            op.id,
            op.generation,
            &step(ReplanStatus::Parked),
            None,
            json!({}),
        )
        .unwrap();

    let proposal = revise(&mut queue, "Waiting revise", None);
    let providers = json!([{"provider": "claude", "reason": "authentication"}]);
    let mut seen = None;
    let mut added = None;
    let mut withdrawn = false;
    let mut released = false;
    drive(&fx, |queue| {
        let blocked = payloads(queue, EventKind::PlannerStartBlocked);
        if released {
            return !payloads(queue, EventKind::PlannerStartResumed).is_empty();
        }
        let Some(new) = added else {
            if !repeated_pass(&fx, &blocked, &mut seen) {
                return false;
            }
            assert!(queue.planners(true).unwrap().is_empty());
            assert_eq!(
                blocked,
                [json!({"providers": providers, "requests": [waiting], "proposals": [proposal]})]
            );
            assert_wait(&inbox(&fx), &[waiting], proposal, &providers);
            added = Some(request(queue, "New request during the wait"));
            return false;
        };
        if !withdrawn {
            if blocked.len() < 2 {
                return false;
            }
            assert_eq!(blocked.len(), 2);
            assert_wait(&inbox(&fx), &[waiting, new], proposal, &providers);
            op = queue
                .move_replan(op.id, op.generation, &step(ReplanStatus::Blocked))
                .unwrap();
            Replans::new(queue, &actor, &StaticPolicy)
                .resolve(
                    &ResolveReplan {
                        operation: op.id,
                        generation: op.generation,
                        resolution: ReplanResolution::Withdraw,
                        reason: "continue the original run".into(),
                    },
                    |_| None,
                )
                .unwrap();
            assert_eq!(
                queue.request_by_id(waiting).unwrap().status,
                RequestStatus::Declined
            );
            assert_eq!(queue.request_by_id(waiting).unwrap().planners, 0);
            withdrawn = true;
            return false;
        }
        if blocked.len() < 3 {
            return false;
        }
        assert_eq!(blocked.len(), 3);
        let status = inbox(&fx);
        assert!(request_row(&status, waiting).is_none());
        assert_wait(&status, &[new], proposal, &providers);
        release_claude(queue, hold);
        released = true;
        false
    });
    let added = added.unwrap();
    assert_eq!(
        payloads(&queue, EventKind::PlannerStartResumed),
        [json!({})]
    );
    for_request(&queue, added);
    assert_eq!(
        payloads(&queue, EventKind::PlanReviseSent)[0]["proposal_id"],
        json!(proposal)
    );
    let status = inbox(&fx);
    assert!(waits(&status).is_empty());
    let row = request_row(&status, added).unwrap();
    assert_eq!(row["waiting_for"], "planner");
    assert!(row.get("provider_reasons").is_none());
}
