//! The answers of `approve_landing` asks wired from the answer to what the
//! supervisor does with the run: a pass applies the answer whatever the
//! slots and closes the ask. A `land` queues the run for the serial
//! landing, selecting eligible runs by effective priority and entry time;
//! claims can use worker room beside that lane. A
//! `retry_review` reviews a failed review's run again; a supervisor that
//! drains applies the answers but starts no new work (ADR-t3330-1). Which
//! answer applies, and when an approved run lands, is
//! `supervise::execution::landing`'s judgment, tested there.

use std::sync::mpsc;

use serde_json::{Value, json};

use super::{
    agent::{Script, succeeded, succeeded_at_the_head, verdict},
    harness::Wiring,
};
use crate::{
    application::TaskReads,
    domain::{Ask, AskId, AskKind, RunId, RunStatus, TaskDetail, TaskId, TaskStatus},
    infrastructure::sqlite::SqliteQueue,
};

/// The payloads of `detail`'s events of `kind`, oldest first.
fn payloads<'a>(detail: &'a TaskDetail, kind: &str) -> Vec<&'a Value> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| &event.payload)
        .collect()
}

/// The kinds of `detail`'s events, oldest first: what an assertion shows.
fn kinds(detail: &TaskDetail) -> Vec<&str> {
    detail
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect()
}

/// The ID of the first event of `kind` of `task`.
fn first_event(queue: &mut SqliteQueue, task: TaskId, kind: &str) -> i64 {
    queue
        .show(task)
        .unwrap()
        .events
        .iter()
        .find(|event| event.kind.as_str() == kind)
        .unwrap_or_else(|| panic!("task {task} has no {kind}"))
        .id
        .as_i64()
}

/// What a review job prints that holds no verdict: the review fails.
fn no_verdict() -> Value {
    json!("no verdict here")
}

/// A review's `concern`, which asks a person to land.
fn concern() -> Value {
    verdict("concern", "a concern")
}

/// The `approve_landing` ask of `task`'s first run.
fn landing_ask(queue: &mut SqliteQueue, task: TaskId) -> Ask {
    let run = queue.show(task).unwrap().runs[0].id().clone();
    let asks = queue.asks(Default::default()).unwrap();
    asks.iter()
        .find(|ask| ask.kind == AskKind::ApproveLanding && ask.run_id.as_ref() == Some(&run))
        .unwrap_or_else(|| panic!("no approve_landing ask of task {task}: {asks:?}"))
        .clone()
}

/// The `runtime_delivers` of the answer of ask `ask` of `task`.
fn delivers(queue: &mut SqliteQueue, task: TaskId, ask: AskId) -> Value {
    queue
        .show(task)
        .unwrap()
        .events
        .into_iter()
        .find(|event| {
            event.kind.as_str() == "ask_answered" && event.payload["ask_id"] == json!(ask)
        })
        .expect("the answer is recorded")
        .payload["runtime_delivers"]
        .clone()
}

/// The `next` of the attention of ask `ask` in `status`.
fn ask_next(status: &Value, ask: AskId) -> Value {
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["ask_id"] == json!(ask))
        .unwrap_or_else(|| panic!("{status}"))["next"]
        .clone()
}

/// Whether `task`'s first run is at work in its turn.
fn running(queue: &mut SqliteQueue, task: TaskId) -> bool {
    queue
        .show(task)
        .unwrap()
        .runs
        .first()
        .is_some_and(|run| run.status() == RunStatus::Running)
}

/// A `land` answer, with a reason, given while another run occupies the
/// serial landing lane is applied on the next pass: the ask closes,
/// `landing_queued` (`via: approve`) is recorded and the run waits unleased,
/// `status` saying it is queued to land. A later task can claim worker
/// room while the approved run still waits for the occupied landing lane.
#[test]
fn a_land_answer_waits_unleased_and_does_not_hold_worker_claims_behind_a_landing() {
    const LAND_WITH_A_REASON: &str = "land: review 3, the findings are docs only";
    let mut wiring = Wiring::new(
        "a_land_answer_waits_unleased_and_does_not_hold_worker_claims_behind_a_landing",
        Script {
            receipts: vec![succeeded, succeeded, succeeded],
            verdicts: vec![concern(), verdict("pass", "fine")],
        },
    )
    .with_parallel(1);
    let first = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, first);

    let target =
        std::path::PathBuf::from(queue.show(first).unwrap().runs[0].worktree_path().unwrap());
    let measured = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw = measured.clone();
    wiring.moving_sizes = std::sync::Arc::new(move |path| {
        if path == target {
            saw.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(Some(0))
    });
    const SECOND_LANDING: &str = "second-landing-check";
    let second = wiring.ready_task(&[SECOND_LANDING]);
    let (release_landing, landing) = mpsc::channel();
    wiring.verifier.on_command(
        SECOND_LANDING,
        Box::new(move || {
            landing
                .recv_timeout(super::harness::LIMIT)
                .expect("the next worker claim releases the landing");
        }),
    );
    // Add the next worker only after observing the queued approval, so
    // its claim cannot race the observation behind the held landing.
    let mut third = None;
    let mut answered = false;
    let mut observed = None;
    let mut claimed_during_landing = false;
    wiring.supervise_once_while(|queue| {
        if !answered {
            if queue
                .show(second)
                .unwrap()
                .runs
                .first()
                .is_some_and(|run| run.status() == RunStatus::Integrating)
            {
                use crate::application::queue_commands::{AnswerParams, QueueCommand};
                use crate::domain::authorization::{AnswerPolicy, DenyReason};
                use crate::domain::{ActorContext, AuthorizationError};
                // Direct DB dispatch receives an ordinary actor, with no
                // deployment metadata supplied by the caller.
                let mut direct = wiring.queue().with_actor(ActorContext::user());
                let command = QueueCommand::Answer(AnswerParams {
                    original_text: None,
                    id: ask.id.as_i64(),
                    text: LAND_WITH_A_REASON.into(),
                });
                let before = direct.read_ask(ask.id).unwrap();
                for policy in [
                    AnswerPolicy::new(Some(true), 100),
                    AnswerPolicy::new(None, 100),
                ] {
                    let error = crate::compose::wiring_run_command_with_policy(
                        &mut direct,
                        std::path::Path::new("/wiring/queue"),
                        &command,
                        &policy,
                    )
                    .unwrap_err();
                    assert_eq!(
                        error.downcast_ref::<AuthorizationError>().unwrap().reason,
                        DenyReason::RecentLogin
                    );
                    let after = direct.read_ask(ask.id).unwrap();
                    assert_eq!(after.answer, before.answer);
                    assert_eq!(after.closed_at, before.closed_at);
                }
                crate::compose::wiring_run_command_with_policy(
                    &mut direct,
                    std::path::Path::new("/wiring/queue"),
                    &command,
                    &AnswerPolicy::new(Some(false), 100),
                )
                .unwrap();
                answered = true;
            }
            return false;
        }
        if observed.is_none() {
            if queue.read_ask(ask.id).unwrap().closed_at.is_none() {
                return false;
            }
            let detail = queue.show(first).unwrap();
            let queued: Vec<Value> = payloads(&detail, "landing_queued")
                .into_iter()
                .cloned()
                .collect();
            observed = Some((
                queue.show(second).unwrap().runs[0].status(),
                detail.runs[0].status(),
                queued,
                queue.run_lease(detail.runs[0].id()).unwrap().is_some(),
                wiring.status(),
            ));
            third = Some(wiring.ready_task(&["true"]));
            return false;
        }
        if queue.show(third.unwrap()).unwrap().runs.is_empty() {
            return false;
        }
        assert_eq!(
            queue.show(second).unwrap().runs[0].status(),
            RunStatus::Integrating
        );
        let approved = queue.show(first).unwrap().runs[0].clone();
        assert_eq!(approved.status(), RunStatus::AwaitingIntegration);
        assert!(queue.run_lease(approved.id()).unwrap().is_none());
        claimed_during_landing = true;
        release_landing.send(()).unwrap();
        true
    });
    let (second_status, first_status, queued, leased, status) =
        observed.expect("the answer was applied");
    assert!(claimed_during_landing);
    assert_eq!(second_status, RunStatus::Integrating);
    assert_eq!(first_status, RunStatus::AwaitingIntegration);
    assert_eq!(queued, [json!({"via": "approve", "ask_id": ask.id})]);
    assert!(!leased);
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["task_id"] == first.as_i64())
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(entry["next"], "queued to land (runtime)");
    assert_eq!(entry["kind"], "landing_queued");
    assert_eq!(delivers(&mut queue, first, ask.id), true);
    assert_eq!(
        queue.read_ask(ask.id).unwrap().answer.as_deref(),
        Some(LAND_WITH_A_REASON)
    );

    let third = third.expect("the next worker was added after observing the approval");
    for task in [first, second, third] {
        let detail = queue.show(task).unwrap();
        assert_eq!(
            detail.runs[0].status(),
            RunStatus::Integrated,
            "{task} {:?}",
            kinds(&detail)
        );
        assert_eq!(payloads(&detail, "integration_started").len(), 1, "{task}");
    }
    let approved = payloads(&queue.show(first).unwrap(), "integration_approved")
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0]["ask_id"], ask.id.as_i64());
    // Task 3 claims worker capacity while task 2 is held integrating and
    // the approved run waits unleased. The landing lane remains serial.
    assert!(
        first_event(&mut queue, second, "run_integrated")
            < first_event(&mut queue, first, "integration_started")
    );
    assert!(
        first_event(&mut queue, second, "integration_started")
            < first_event(&mut queue, third, "run_claimed")
    );
    assert!(
        first_event(&mut queue, third, "run_claimed")
            < first_event(&mut queue, second, "run_integrated")
    );
    assert!(
        queue
            .asks(Default::default())
            .unwrap()
            .iter()
            .all(|ask| ask.closed_at.is_some())
    );
    assert_eq!(wiring.agent.review_prompts().len(), 3);
    assert!(
        measured.load(std::sync::atomic::Ordering::SeqCst),
        "the unleased run queued by land was measured"
    );
}

const JOINT_CHECK: &str = "check-stacked-tree";

fn approved_batch(name: &str) -> (Wiring, [(TaskId, RunId); 2]) {
    approved_batch_with_commands(name, [&[JOINT_CHECK]; 2])
}

fn approved_batch_with_commands(
    name: &str,
    commands: [&[&str]; 2],
) -> (Wiring, [(TaskId, RunId); 2]) {
    use crate::domain::Priority;
    use std::time::Duration;

    let wiring = Wiring::new(
        name,
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_tick(Duration::from_millis(1));
    wiring
        .verifier
        .set_config("[verification]\nlanding_batch_size = 2\n");
    wiring.git.model_batch_trees();
    let mut queue = wiring.queue();
    let members = [
        (Priority::High, commands[0]),
        (Priority::Normal, commands[1]),
    ]
    .map(|(priority, commands)| approved_batch_member(&wiring, &mut queue, priority, commands));
    (wiring, members)
}

fn approved_batch_member(
    wiring: &Wiring,
    queue: &mut SqliteQueue,
    priority: crate::domain::Priority,
    commands: &[&str],
) -> (TaskId, RunId) {
    use crate::{application::integrate::binding, domain::EventKind};
    use std::path::Path;

    let run = direct_landing_fixture_with_commands(
        wiring,
        queue,
        priority,
        RunStatus::AwaitingIntegration,
        commands,
    );
    let receipt = succeeded_at_the_head(
        run.id(),
        Path::new(run.worktree_path().unwrap()),
        &wiring.git,
    )
    .unwrap();
    wiring.files.put(
        Path::new(run.receipt_path().unwrap()),
        std::time::SystemTime::UNIX_EPOCH,
        &receipt.to_string(),
    );
    let facts = binding::bind(
        &wiring.git,
        run.base_commit().as_str(),
        run.result_commit().unwrap().as_str(),
    )
    .unwrap();
    let mut judgment = json!({"verdict": "pass"});
    binding::put(&mut judgment, &facts);
    queue
        .record_runtime_event(run.id(), EventKind::ReviewFinished, judgment)
        .unwrap();
    queue
        .record_runtime_event(run.id(), EventKind::LandingQueued, json!({"via": "exit"}))
        .unwrap();
    (run.task_id(), run.id().clone())
}

#[test]
fn a_passing_batch_publishes_both_squashes_in_order_after_one_check() {
    use crate::domain::{
        ci_watch::FINDING_KIND,
        finding::{FindingStatus, FindingTarget, NewFinding},
        landing_verification::FIX_RUN_ENV,
    };

    let (wiring, [(first, _), (second, _)]) = approved_batch_with_commands(
        "a_passing_batch_publishes_both_squashes_in_order_after_one_check",
        [&[JOINT_CHECK], &[]],
    );
    wiring.verifier.set_config(&format!(
        "[verification]\nlanding_batch_size = 2\n[landing_verification]\nreplaces = ['{JOINT_CHECK}']\ncommand = 'shared-gate'\n"
    ));
    let mut queue = wiring.queue();
    let finding = queue
        .record_finding(NewFinding {
            kind: FINDING_KIND.to_owned(),
            target: FindingTarget::Queue,
            subject: "existing CI failure".to_owned(),
            summary: "the second member fixes this failure".to_owned(),
            detail: None,
            impact: None,
            evidence: vec![],
            propose: None,
            by: "human".to_owned(),
        })
        .unwrap();
    queue
        .set_finding_status_covered(
            finding.finding.id,
            FindingStatus::Dismissed,
            "covered by the second member",
            "human",
            Some(second),
        )
        .unwrap();
    wiring.supervise_once();
    let first_run = queue.show(first).unwrap().runs[0].clone();
    let second_run = queue.show(second).unwrap().runs[0].clone();
    assert_eq!(first_run.status(), RunStatus::Integrated);
    assert_eq!(second_run.status(), RunStatus::Integrated);
    assert_ne!(first_run.result_commit(), second_run.result_commit());
    assert!(
        first_event(&mut queue, first, "run_integrated")
            < first_event(&mut queue, second, "run_integrated")
    );
    assert_eq!(wiring.verifier.ran().len(), 1);
    let environments = wiring.verifier.environments.lock().unwrap();
    assert!(environments.iter().any(|(command, env)| {
        command == "shared-gate"
            && env
                .iter()
                .any(|(key, value)| key == FIX_RUN_ENV && value == "1")
    }));
}

#[test]
fn a_batch_and_claim_can_start_in_the_same_admission_pass() {
    use crate::{
        application::supervise::PausePoint,
        domain::{
            Priority,
            disk::{DiskConfig, DiskEstimate},
        },
    };
    use std::sync::{Arc, atomic::Ordering};

    let (wiring, [(first, first_run), (second, second_run)]) =
        approved_batch("a_batch_and_claim_can_start_in_the_same_admission_pass");
    let gib = 1 << 30;
    let wiring = wiring.with_parallel(1).with_disk(DiskConfig {
        estimate: DiskEstimate::Fixed,
        run_size: Some(20 * gib),
        landing_size: Some(20 * gib),
        ..DiskConfig::default()
    });
    let mut queue = wiring.queue();
    let (third, _) = approved_batch_member(&wiring, &mut queue, Priority::Low, &[JOINT_CHECK]);
    let claim = wiring.ready_task(&["true"]);
    wiring.fail_claim_launches(true);
    let free = wiring.free.clone();
    free.store(45 * gib, Ordering::SeqCst);
    let wiring = wiring.with_pauses(Arc::new(move |point, _| {
        if point == PausePoint::LandingStart {
            // A claim can use the current pass's budget. The following
            // pass will have no disk room for it.
            free.store(0, Ordering::SeqCst);
        }
    }));

    let outcome = wiring.supervise_once_reporting();
    assert!(
        outcome["errors"]
            .to_string()
            .contains("refuses to start the wrapper"),
        "{outcome}"
    );
    let first_detail = queue.show(first).unwrap();
    let second_detail = queue.show(second).unwrap();
    assert_eq!(first_detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(second_detail.runs[0].status(), RunStatus::Integrated);
    let progress = payloads(&first_detail, "landing_batch_progress");
    assert_eq!(progress[0]["runs"], json!([first_run, second_run]));
    assert_eq!(wiring.verifier.ran().len(), 1);
    assert!(payloads(&queue.show(third).unwrap(), "integration_started").is_empty());
    assert_eq!(
        payloads(&queue.show(claim).unwrap(), "run_claimed").len(),
        1
    );
    assert!(
        first_event(&mut queue, first, "integration_started")
            < first_event(&mut queue, claim, "run_claimed")
    );
}

/// Run a hook once the service commits a member's landing, before it
/// returns the answer to the landing thread.
fn after_landing_recorded(wiring: Wiring, run: RunId, hook: super::host::Hook) -> Wiring {
    let hook = std::sync::Mutex::new(Some(hook));
    with_queue_service_call(wiring, move |inner, case, params| {
        let landed = params["op"] == "finish_integration" && params["args"][0] == run.as_str();
        let result = inner.call(case, params)?;
        if landed {
            let hook = hook.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        Ok(result)
    })
}

fn with_queue_service_call(
    wiring: Wiring,
    call: impl Fn(
        &dyn crate::application::supervisor_service::ServiceCaller,
        crate::domain::queue_service::UseCase,
        Value,
    ) -> anyhow::Result<Value>
    + Send
    + Sync
    + 'static,
) -> Wiring {
    use crate::{
        application::supervisor_service::ServiceCaller,
        domain::{LeaseToken, queue_service::UseCase},
    };
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    struct Caller<F> {
        inner: Arc<dyn ServiceCaller>,
        call: Arc<F>,
    }

    impl<F> ServiceCaller for Caller<F>
    where
        F: Fn(&dyn ServiceCaller, UseCase, Value) -> anyhow::Result<Value> + Send + Sync + 'static,
    {
        fn call(&self, case: UseCase, params: Value) -> anyhow::Result<Value> {
            (self.call)(&*self.inner, case, params)
        }

        fn with_token(&self, file: &Path) -> anyhow::Result<Arc<dyn ServiceCaller>> {
            Ok(Arc::new(Self {
                inner: self.inner.with_token(file)?,
                call: self.call.clone(),
            }))
        }

        fn sign_in(&self, token: &LeaseToken, pid: u32) -> anyhow::Result<()> {
            self.inner.sign_in(token, pid)
        }

        fn take_back(&self, token: &LeaseToken, pid: u32) -> anyhow::Result<()> {
            self.inner.take_back(token, pid)
        }

        fn hand_over(&self) {
            self.inner.hand_over();
        }

        fn sign_out(&self) {
            self.inner.sign_out();
        }

        fn credential(&self) -> Option<PathBuf> {
            self.inner.credential()
        }
    }

    let call = Arc::new(call);
    wiring.with_service_wrap(Arc::new(move |inner| {
        Arc::new(Caller {
            inner,
            call: call.clone(),
        })
    }))
}

#[test]
fn an_unanswered_request_between_squashes_leaves_the_follower_with_the_batch() {
    use crate::application::supervisor_service::ServiceUnreachable;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering::SeqCst},
    };

    let (wiring, [(first, first_run), (second, second_run)]) =
        approved_batch("an_unanswered_request_between_squashes_leaves_the_follower_with_the_batch");
    let original = wiring.git.main();
    let (release, resume) = mpsc::channel();
    let (reconciling, reconciliation) = mpsc::channel();
    let resume = Mutex::new(Some(resume));
    let reconciling = Mutex::new(Some(reconciling));
    let first_recorded = AtomicBool::new(false);
    let lost = AtomicBool::new(false);
    let wiring = with_queue_service_call(
        wiring.with_continuous_passes(),
        move |inner, case, params| {
            let op = params["op"].as_str();
            if op == Some("active_runs") && first_recorded.load(SeqCst) && !lost.swap(true, SeqCst)
            {
                return Err(ServiceUnreachable::not_done("lost a supervisor pass's read").into());
            }
            if op == Some("runs_leased_by")
                && lost.load(SeqCst)
                && let Some(reconciling) = reconciling.lock().unwrap().take()
            {
                reconciling.send(()).unwrap();
            }
            let first_squash =
                op == Some("finish_integration") && params["args"][0] == first_run.as_str();
            let result = inner.call(case, params)?;
            if first_squash {
                let resume = resume.lock().unwrap().take();
                if let Some(resume) = resume {
                    first_recorded.store(true, SeqCst);
                    resume
                        .recv_timeout(super::harness::LIMIT)
                        .expect("observer releases the batch");
                }
            }
            Ok(result)
        },
    );

    let mut reconciliation_pass = None;
    let mut release = Some(release);
    let mut midpoint = None;
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        if reconciliation_pass.is_none() && reconciliation.try_recv().is_ok() {
            reconciliation_pass = Some(wiring.passes());
        }
        // Let the entire recovery pass step any slot it rebuilt before
        // allowing the batch to move on to the next squash.
        if release.is_some() && reconciliation_pass.is_some_and(|pass| wiring.passes() > pass) {
            let [owner] = queue.supervisors().unwrap().try_into().unwrap();
            midpoint = Some((
                queue.show(first).unwrap().runs[0].status(),
                queue.show(second).unwrap().runs[0].status(),
                queue.holds_lease(&second_run, &owner.token).unwrap(),
            ));
            release.take().unwrap().send(()).unwrap();
        }
        if payloads(&queue.show(first).unwrap(), "landing_batch_progress")
            .iter()
            .any(|progress| progress["finished"] == true)
        {
            stop();
            return true;
        }
        false
    });
    assert_eq!(
        midpoint,
        Some((RunStatus::Integrated, RunStatus::AwaitingIntegration, true)),
        "the in-flight batch must keep its follower's lease through reconciliation"
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let mut queue = wiring.queue();
    let first = queue.show(first).unwrap();
    let second = queue.show(second).unwrap();
    assert_eq!(second.runs[0].status(), RunStatus::Integrated);
    assert_eq!(payloads(&second, "integration_started").len(), 1);
    assert!(queue.run_lease(&second_run).unwrap().is_none());
    assert_eq!(
        wiring.git.main_line(),
        [
            original,
            first.runs[0].result_commit().unwrap().clone(),
            second.runs[0].result_commit().unwrap().clone(),
        ]
    );
    assert_eq!(wiring.verifier.ran().len(), 1);
}

#[test]
fn a_token_sweep_between_a_followers_squash_and_push_keeps_its_integrator() {
    use crate::{
        application::{Clock, queue_service::ServiceBackend},
        domain::queue_service::Principal,
    };
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering::SeqCst},
        },
        time::{Duration, Instant, SystemTime},
    };

    struct SweepClock {
        base: Arc<dyn Clock>,
        due: AtomicBool,
    }

    impl Clock for SweepClock {
        fn system_time(&self) -> SystemTime {
            self.base.system_time()
        }

        fn monotonic(&self) -> Instant {
            let now = self.base.monotonic();
            if self.due.load(SeqCst) {
                now + Duration::from_secs(61)
            } else {
                now
            }
        }
    }

    let (wiring, [(first, first_run), (second, second_run)]) =
        approved_batch("a_token_sweep_between_a_followers_squash_and_push_keeps_its_integrator");
    let clock = Arc::new(SweepClock {
        base: wiring.generators().clock,
        due: AtomicBool::new(false),
    });
    let backend = wiring.service_backend();
    let stale = Principal::worker(&first_run, first);
    let (release, resume) = mpsc::channel();
    let pause = {
        let clock = clock.clone();
        let backend = backend.clone();
        let db = wiring.db.clone();
        let stale = stale.clone();
        Box::new(move || {
            // A leftover token on the already landed leader tells us
            // when the periodic sweep has actually run.
            backend.access().issue(&db, &stale).unwrap();
            clock.due.store(true, SeqCst);
            resume
                .recv_timeout(super::harness::LIMIT)
                .expect("observer releases the second landing");
        })
    };
    let wiring = after_landing_recorded(
        wiring.with_clock(clock.clone()).with_continuous_passes(),
        second_run.clone(),
        pause,
    );

    let mut release = Some(release);
    let mut sweep_pass = None;
    let mut midpoint = None;
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        if release.is_some() && clock.due.load(SeqCst) {
            let principals = backend.principals().unwrap();
            if sweep_pass.is_none() && !principals.contains(&stale) {
                sweep_pass = Some(wiring.passes());
            }
            // Wait for the full sweep, including principals visited
            // after the stale worker, while the follower is paused.
            if sweep_pass.is_some_and(|pass| wiring.passes() > pass) {
                let detail = queue.show(second).unwrap();
                midpoint = Some((
                    detail.runs[0].status(),
                    payloads(&detail, "push_finished").len(),
                    backend
                        .principals()
                        .unwrap()
                        .iter()
                        .any(|p| p.landing_run().as_ref() == Some(&second_run)),
                ));
                release.take().unwrap().send(()).unwrap();
            }
        }
        if payloads(&queue.show(first).unwrap(), "landing_batch_progress")
            .iter()
            .any(|progress| progress["finished"] == true)
        {
            stop();
            return true;
        }
        false
    });
    assert_eq!(midpoint, Some((RunStatus::Integrated, 0, true)));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        payloads(&wiring.queue().show(second).unwrap(), "push_finished").len(),
        1
    );
    assert!(
        backend
            .principals()
            .unwrap()
            .iter()
            .all(|p| p.landing_run().as_ref() != Some(&second_run))
    );
}

#[test]
fn handoff_after_the_first_squash_leaves_the_following_member_queued() {
    const NEXT: &str = "/wiring/next/dagq";
    let (wiring, [(first, first_run), (second, second_run)]) =
        approved_batch("handoff_after_the_first_squash_leaves_the_following_member_queued");
    let original = wiring.git.main();
    let db = wiring.db.clone();
    let generators = wiring.generators();
    let wiring = after_landing_recorded(
        wiring.with_continuous_passes(),
        first_run.clone(),
        Box::new(move || {
            let queue = SqliteQueue::open(&db).unwrap().with_generators(generators);
            let [registration] = queue.supervisors().unwrap().try_into().unwrap();
            assert!(queue.request_handoff(&registration.token, NEXT).unwrap());
        }),
    );
    let outcome = wiring.supervise_once_reporting();
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    assert_eq!(outcome["binary"], NEXT);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let [landed] = outcome["runs"].as_array().unwrap().as_slice() else {
        panic!("expected one landed run, got {outcome}");
    };
    assert_eq!(landed["id"], json!(first_run));
    assert_eq!(landed["status"], "integrated");

    let mut queue = wiring.queue();
    let first = queue.show(first).unwrap();
    let second = queue.show(second).unwrap();
    assert_eq!(first.runs[0].status(), RunStatus::Integrated);
    assert_eq!(
        wiring.git.main_line(),
        [original, first.runs[0].result_commit().unwrap().clone()]
    );
    assert_eq!(second.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(payloads(&second, "integration_started").is_empty());
    assert!(queue.run_lease(&second_run).unwrap().is_none());
    let status = wiring.status();
    assert_eq!(status["landing_queue"][0]["run_id"], json!(second_run));
    assert_eq!(status["landing_batch"]["finished"], true);
    assert_eq!(status["landing_batch"]["removed"], json!([]));
    assert_eq!(wiring.verifier.ran().len(), 1);
}

#[test]
fn recheck_waits_between_squashes_and_checks_the_remaining_run_on_final_main() {
    const RECHECK: &str = "check-waiting-run";
    let (wiring, [(first, first_run), (second, second_run)]) =
        approved_batch("recheck_waits_between_squashes_and_checks_the_remaining_run_on_final_main");
    wiring.verifier.set_config(&format!(
        "[verification]\nlanding_batch_size = 2\n[recheck]\ncommand = \"{RECHECK}\"\n"
    ));
    let (waiting, ask) = reviewed_landing_waiting_for_an_answer(&wiring, &mut wiring.queue());
    let probe = wiring
        .files
        .observe_exclusive_lock(&wiring.db.parent().unwrap().join("landing-batch/turn"));
    let (paused, reached) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let count = probe.clone();
    let pause = Box::new(move || {
        paused.send(count.blocked_attempts()).unwrap();
        resume
            .recv_timeout(super::harness::LIMIT)
            .expect("observer releases the first landing");
    });
    // The batch still holds its lock and has no integrating member yet.
    let wiring = after_landing_recorded(wiring.with_continuous_passes(), first_run, pause);

    let mut at_pause = None;
    let mut release = Some(release);
    let mut midpoint = None;
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        if release.is_some() {
            if at_pause.is_none() {
                at_pause = reached.try_recv().ok();
            }
            let Some(at_pause) = at_pause else {
                return false;
            };
            let blocked = probe.blocked_attempts() > at_pause;
            let ran_early = wiring.verifier.ran().iter().any(|(cmd, _)| cmd == RECHECK);
            if !blocked && !ran_early {
                return false;
            }
            midpoint = Some((
                blocked,
                ran_early,
                wiring.git.main(),
                queue.show(first).unwrap().runs[0].status(),
                queue.show(second).unwrap().runs[0].status(),
            ));
            release.take().unwrap().send(()).unwrap();
        }
        if queue
            .latest_event_of("landing_recheck_finished")
            .unwrap()
            .is_some()
        {
            stop();
            return true;
        }
        false
    });
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let (blocked, ran_early, intermediate_main, first_status, second_status) = midpoint.unwrap();
    assert!(blocked, "recheck must attempt the held batch lock");
    assert!(!ran_early);
    assert_eq!(first_status, RunStatus::Integrated);
    assert_eq!(second_status, RunStatus::AwaitingIntegration);

    let mut queue = wiring.queue();
    let first = queue.show(first).unwrap();
    let second = queue.show(second).unwrap();
    let waiting = queue.show(waiting.task_id()).unwrap();
    assert_eq!(first.runs[0].result_commit(), Some(&intermediate_main));
    assert_eq!(second.runs[0].status(), RunStatus::Integrated);
    let final_main = wiring.git.main();
    assert_ne!(final_main, intermediate_main);
    assert_eq!(second.runs[0].result_commit(), Some(&final_main));
    assert!(queue.read_ask(ask).unwrap().is_open());
    for member in [&first, &second] {
        assert!(payloads(member, "landing_recheck_clean").is_empty());
    }
    assert!(payloads(&first, "landing_recheck_finished").is_empty());
    let [finished] = payloads(&second, "landing_recheck_finished")
        .try_into()
        .unwrap();
    assert_eq!(finished["main"], json!(final_main));
    assert_eq!(finished["checked"], 1);
    let [clean] = payloads(&waiting, "landing_recheck_clean")
        .try_into()
        .unwrap();
    assert_eq!(clean["main"], json!(final_main));
    assert_eq!(clean["landed_run_id"], json!(second_run));
    let ran = wiring.verifier.ran();
    assert_eq!(
        ran.iter()
            .map(|(command, _)| command.as_str())
            .collect::<Vec<_>>(),
        [JOINT_CHECK, RECHECK]
    );
}

#[test]
fn a_conflicting_member_is_sent_back_after_the_clean_prefix_lands() {
    use crate::application::Repository;

    let (wiring, [(first, first_run), (second, second_run)]) =
        approved_batch("a_conflicting_member_is_sent_back_after_the_clean_prefix_lands");
    let mut queue = wiring.queue();
    let original = wiring.git.main();
    let first_head = queue.show(first).unwrap().runs[0]
        .result_commit()
        .unwrap()
        .clone();
    let second_head = queue.show(second).unwrap().runs[0]
        .result_commit()
        .unwrap()
        .clone();
    wiring
        .git
        .conflict_with_prefix(&first_head, second_head, "same.txt");
    wiring.supervise_until_command(JOINT_CHECK);

    let first_detail = queue.show(first).unwrap();
    let landed = &first_detail.runs[0];
    assert_eq!(landed.status(), RunStatus::Integrated);
    let squash = landed.result_commit().unwrap();
    assert_ne!(squash, &first_head);
    assert_eq!(wiring.git.main_line(), [original, squash.clone()]);
    assert_eq!(
        wiring.git.tree_of(squash.as_str()).unwrap(),
        wiring.git.tree_of(first_head.as_str()).unwrap()
    );
    let second_detail = queue.show(second).unwrap();
    assert_eq!(second_detail.runs[0].status(), RunStatus::NeedsSession);
    let [deferred] = payloads(&second_detail, "integration_deferred")
        .try_into()
        .unwrap();
    assert_eq!(deferred["code"], "rebase_conflict");
    assert_eq!(deferred["conflicts"], json!(["same.txt"]));
    assert!(
        first_event(&mut queue, first, "run_integrated")
            < first_event(&mut queue, second, "integration_deferred")
    );
    let events = payloads(&first_detail, "landing_batch_progress");
    let stages: Vec<_> = events
        .iter()
        .filter_map(|event| event["stage"].as_object())
        .collect();
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0]["runs"], json!([first_run]));
    assert_eq!(stages[0]["depth"], 0);
    assert_eq!(wiring.verifier.ran().len(), 1);
    assert_eq!(wiring.status()["landing_queue"], json!([]));
    let finished = events.last().unwrap();
    assert_eq!(finished["finished"], true);
    assert_eq!(finished["removed"], json!([second_run]));
}

#[test]
fn a_member_without_an_admissible_receipt_takes_the_ordinary_path() {
    use std::path::Path;

    let (wiring, [(first, first_run), (second, second_run)]) =
        approved_batch("a_member_without_an_admissible_receipt_takes_the_ordinary_path");
    let mut queue = wiring.queue();
    let first = queue.show(first).unwrap();
    wiring.files.put(
        Path::new(first.runs[0].receipt_path().unwrap()),
        std::time::SystemTime::UNIX_EPOCH,
        "invalid receipt",
    );
    wiring.supervise_until_command(JOINT_CHECK);
    let first = queue.show(first.task.id()).unwrap();
    let second = queue.show(second).unwrap();
    assert_eq!(first.runs[0].status(), RunStatus::NeedsSession);
    let [deferred] = payloads(&first, "integration_deferred").try_into().unwrap();
    assert_eq!(deferred["code"], "receipt_invalid");
    assert_eq!(second.runs[0].status(), RunStatus::Integrated);
    let events = payloads(&first, "landing_batch_progress");
    let stages: Vec<_> = events
        .iter()
        .filter_map(|event| event["stage"].as_object())
        .collect();
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0]["runs"], json!([second_run]));
    assert_eq!(stages[0]["depth"], 0);
    assert_eq!(wiring.verifier.ran().len(), 1);
    assert_eq!(events.last().unwrap()["removed"], json!([first_run]));
}

#[test]
fn failed_joint_verification_exposes_the_serial_split_in_events_and_status() {
    let (wiring, [(first, first_run), (second, second_run)]) =
        approved_batch("failed_joint_verification_exposes_the_serial_split_in_events_and_status");
    let runs = [first_run, second_run];
    let mut queue = wiring.queue();
    wiring.verifier.fail_once(JOINT_CHECK);
    let (release, wait) = mpsc::channel();
    wiring.verifier.on_command(
        JOINT_CHECK,
        Box::new(move || {
            wait.recv_timeout(super::harness::LIMIT)
                .expect("batch start observed");
        }),
    );
    wiring.supervise_once_while(|_| {
        let status = wiring.status();
        let stage = &status["landing_batch"]["stage"];
        let commands = wiring.verifier.ran();
        if stage["depth"] != 0 || commands.is_empty() {
            return false;
        }
        assert_eq!(stage["runs"], json!(runs));
        assert_eq!(commands.len(), 1);
        release.send(()).unwrap();
        true
    });
    let detail = queue.show(first).unwrap();
    let stages: Vec<_> = payloads(&detail, "landing_batch_progress")
        .into_iter()
        .filter_map(|event| event.get("stage").filter(|stage| !stage.is_null()).cloned())
        .collect();
    assert_eq!(stages[0]["runs"], json!(runs));
    assert_eq!(stages[1]["runs"], json!([runs[0]]));
    assert_eq!(stages[1]["depth"], 1);
    assert_eq!(stages[2]["runs"], json!([runs[1]]));
    assert!(
        [first, second]
            .iter()
            .all(|&task| queue.show(task).unwrap().runs[0].status() == RunStatus::Integrated)
    );
    assert_eq!(wiring.verifier.ran().len(), 3);
}

/// `land` answers are applied while a `[run.env]` program is missing, but
/// nothing lands until it is found; then the approved runs land in the
/// order they were approved, each once, under a supervisor other than the
/// ones that applied the answers, without asking or reviewing again.
#[test]
fn approved_runs_wait_for_the_landing_and_land_oldest_approval_first_once() {
    let wiring = Wiring::new(
        "approved_runs_wait_for_the_landing_and_land_oldest_approval_first_once",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![concern()],
        },
    );
    let first = wiring.ready_task(&["true"]);
    let second = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let (first_ask, second_ask) = (
        landing_ask(&mut queue, first).id,
        landing_ask(&mut queue, second).id,
    );

    wiring.verifier.set_program_missing(true);
    // Task 2's run is approved first, then task 1's, each by a supervisor
    // of its own.
    for ask in [second_ask, first_ask] {
        queue.answer(ask, "land").unwrap();
        wiring.supervise_once();
        assert!(queue.read_ask(ask).unwrap().closed_at.is_some(), "{ask}");
    }
    for task in [first, second] {
        let detail = queue.show(task).unwrap();
        assert_eq!(
            detail.runs[0].status(),
            RunStatus::AwaitingIntegration,
            "{:?}",
            kinds(&detail)
        );
        assert!(queue.run_lease(detail.runs[0].id()).unwrap().is_none());
        assert!(payloads(&detail, "integration_started").is_empty());
    }

    wiring.verifier.set_program_missing(false);
    wiring.supervise_once();
    for task in [first, second] {
        let detail = queue.show(task).unwrap();
        assert_eq!(
            detail.runs[0].status(),
            RunStatus::Integrated,
            "{task} {:?}",
            kinds(&detail)
        );
        assert_eq!(payloads(&detail, "integration_started").len(), 1);
        assert_eq!(payloads(&detail, "integration_approved").len(), 1);
        assert_eq!(payloads(&detail, "ask_opened").len(), 1);
    }
    assert!(
        first_event(&mut queue, second, "run_integrated")
            < first_event(&mut queue, first, "integration_started")
    );
    assert_eq!(wiring.agent.review_prompts().len(), 2);
}

/// A failed review's ask offers `retry_review` (ADR-t2521-1): the runtime
/// takes the answer (`runtime_delivers`, `applying the answer`), closes
/// the ask, records `review_retry_queued` once, and reviews the run again
/// as the next attempt on the receipt it has, without resuming the session
/// or asking for a receipt; the passing review lands it. A later
/// supervisor starts no review again. (The harness's supervisor does not
/// run an unreadable review once more, so each attempt is one job.)
#[test]
fn a_retry_review_answer_reviews_a_failed_review_s_run_again_without_a_resume() {
    let wiring = Wiring::new(
        "a_retry_review_answer_reviews_a_failed_review_s_run_again_without_a_resume",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![no_verdict(), verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, task);
    assert!(ask.options.iter().any(|o| o == "retry_review"), "{ask:?}");
    assert!(
        ask.question.contains(
            "retry_review: run the review again on the same run and receipt (no change, no resume of the session)"
        ),
        "{}",
        ask.question
    );

    queue.answer(ask.id, "retry_review").unwrap();
    assert_eq!(delivers(&mut queue, task, ask.id), true);
    assert_eq!(
        ask_next(&wiring.status(), ask.id),
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    wiring.supervise_once();
    // Another supervisor's passes start no review again.
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        kinds(&detail)
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(
        payloads(&detail, "review_retry_queued"),
        [&json!({"ask_id": ask.id, "answer": "retry_review"})]
    );
    let attempts: Vec<&Value> = payloads(&detail, "review_started")
        .into_iter()
        .map(|payload| &payload["attempt"])
        .collect();
    assert_eq!(attempts, [&json!(1), &json!(2)]);
    assert!(payloads(&detail, "resume_started").is_empty());
    assert!(payloads(&detail, "landing_decided").is_empty());
    assert!(payloads(&detail, "review_outcome").is_empty());
    assert_eq!(payloads(&detail, "receipt_observed").len(), 1);
    assert_eq!(wiring.agent.review_prompts().len(), 2);
}

/// `cancel: <reason>` chooses `cancel` as the bare option does: the run
/// fails, its task is canceled and the ask closes, the answer with its
/// reason recorded on `landing_decided`. `retry_review` is no answer of
/// the ask of a verdict (a `concern`): the runtime does not take it
/// (`runtime_delivers: false`, `read the answer`), and the ask stays open
/// with no review started again.
#[test]
fn a_cancel_with_a_reason_is_applied_and_a_concerns_retry_review_is_left_to_the_inbox() {
    let wiring = Wiring::new(
        "a_cancel_with_a_reason_is_applied_and_a_concerns_retry_review_is_left_to_the_inbox",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![concern()],
        },
    );
    let canceled = wiring.ready_task(&["true"]);
    let retried = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let cancel_ask = landing_ask(&mut queue, canceled);
    let retry_ask = landing_ask(&mut queue, retried);
    assert!(
        !retry_ask.options.iter().any(|o| o == "retry_review"),
        "{retry_ask:?}"
    );

    queue.answer(cancel_ask.id, "cancel: not needed").unwrap();
    queue.answer(retry_ask.id, "retry_review").unwrap();
    assert_eq!(delivers(&mut queue, canceled, cancel_ask.id), true);
    assert_eq!(delivers(&mut queue, retried, retry_ask.id), false);
    assert_eq!(
        ask_next(&wiring.status(), retry_ask.id),
        format!("read the answer of ask {} and close it", retry_ask.id)
    );
    let reviews = wiring.agent.review_prompts().len();
    wiring.supervise_once();

    let detail = queue.show(canceled).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Failed,
        "{:?}",
        kinds(&detail)
    );
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert!(queue.read_ask(cancel_ask.id).unwrap().closed_at.is_some());
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["answer"], "cancel: not needed", "{}", decided[0]);
    assert!(payloads(&detail, "integration_approved").is_empty());

    let detail = queue.show(retried).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(queue.read_ask(retry_ask.id).unwrap().closed_at.is_none());
    assert!(payloads(&detail, "review_retry_queued").is_empty());
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert_eq!(wiring.agent.review_prompts().len(), reviews);
}

/// A `retry_review` answered while the queue's hold ask holds Claude for
/// its usage limit is applied, and the review it queues waits held (no
/// second `review_started`) until the hold is answered; then it starts
/// once and the run lands, its session never resumed.
#[test]
fn a_retried_review_waits_for_the_held_provider() {
    let wiring = Wiring::new(
        "a_retried_review_waits_for_the_held_provider",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![no_verdict(), verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, task);
    let hold = crate::application::AskStore::hold(
        &mut queue,
        crate::domain::NewHold {
            reason_category: crate::domain::AskReason::Cost,
            subject: Some(crate::domain::queue_hold::USAGE_LIMIT_SUBJECT.to_owned()),
            run_id: None,
            job: None,
            question: "the usage limit".into(),
            options: crate::domain::HOLD_OPTIONS
                .iter()
                .map(|option| (*option).to_owned())
                .collect(),
            asked_by: "supervisor".into(),
        },
    )
    .unwrap()
    .ask;
    queue.answer(ask.id, "retry_review").unwrap();

    // Some passes after the answer is applied, the retried review still
    // waits; then the hold is answered.
    let mut applied_at = None;
    let mut held = None;
    wiring.supervise_once_while(|queue| {
        if queue.read_ask(ask.id).unwrap().closed_at.is_none() {
            return false;
        }
        let applied_at = *applied_at.get_or_insert(wiring.passes());
        if wiring.passes() < applied_at + 5 {
            return false;
        }
        let detail = queue.show(task).unwrap();
        held = Some((
            payloads(&detail, "review_retry_queued").len(),
            payloads(&detail, "review_started").len(),
            payloads(&detail, "review_failed").len(),
        ));
        queue.answer(hold.id, "done").unwrap();
        true
    });
    assert_eq!(held, Some((1, 1, 1)));
    let detail = queue.show(task).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        kinds(&detail)
    );
    assert_eq!(payloads(&detail, "review_started").len(), 2);
    assert!(payloads(&detail, "resume_started").is_empty());
    assert_eq!(wiring.agent.review_prompts().len(), 2);
}

/// Commit on main `[review.subagents.design]`, which reviews
/// `change.txt` (every range's change in the test's repository), and its
/// definition; no program review runs.
fn design_reviews_the_change(wiring: &Wiring) {
    const CONFIG: &str = "[review.subagents.design]\npaths = [\"change.txt\"]\n\
[review.programs.\"builtin:secrets\"]\nenabled = false\n";
    wiring.git.commit_files(
        &[
            ("dagq.toml", CONFIG),
            (
                ".dagq/agents/design/AGENT.md",
                "---\ndescription: design checks\n---\nCheck the design.\n",
            ),
        ],
        &["change.txt"],
    );
    wiring.verifier.set_config(CONFIG);
}

/// The one open ask of the queue, an `approve_landing`.
fn open_landing_ask(queue: &mut SqliteQueue) -> Ask {
    let open: Vec<Ask> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .filter(|ask| ask.closed_at.is_none())
        .collect();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].kind, AskKind::ApproveLanding);
    open[0].clone()
}

/// The reason a bare `send_back` to ask `ask` parks the run with: the
/// subagent's finding the ask showed, under its name.
fn design_sent_back(ask: AskId) -> String {
    format!("the review's findings were sent back by ask {ask}: design: add a unit test")
}

/// A review whose subagent sends the run back under a pass with no reason
/// of its own asks a person past the run's revises. A bare `send_back` to
/// that ask, given while the run waits, names the subagent's finding the
/// ask showed, under its name, as `landing_decided`'s reason and the
/// prompt of the resume it starts; the review after that resume passes
/// and the run lands. Which findings a send_back names is
/// `supervise::execution::landing`'s.
#[test]
fn a_bare_send_back_of_a_subagents_send_back_carries_its_findings() {
    let wiring = Wiring::new(
        "a_bare_send_back_of_a_subagents_send_back_carries_its_findings",
        Script {
            receipts: vec![
                succeeded,
                succeeded_at_the_head,
                succeeded_at_the_head,
                succeeded_at_the_head,
            ],
            verdicts: vec![verdict("pass", "looks fine")],
        },
    );
    design_reviews_the_change(&wiring);
    // `design` sends the run back on its first three reviews (the last
    // past the run's revises, a person's ask), then passes.
    wiring.agent.review_agent_says(
        "design",
        &[
            ("revise", &["add a unit test"]),
            ("revise", &["add a unit test"]),
            ("revise", &["add a unit test"]),
            ("pass", &[]),
        ],
    );
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let reviewed = queue.show(task).unwrap();
    let finished = payloads(&reviewed, "review_finished");
    assert_eq!(finished.len(), 3, "{finished:?} {:?}", kinds(&reviewed));
    assert_eq!(finished[2]["verdict"], "pass");
    assert_eq!(finished[2]["reasons"], json!([]));
    assert_eq!(finished[2]["route"]["destination"], "send_back");
    let ask = open_landing_ask(&mut queue);
    assert!(
        ask.question.contains("\n- design: add a unit test"),
        "{}",
        ask.question
    );

    queue.answer(ask.id, "send_back").unwrap();
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 1, "{decided:?}");
    let reason = design_sent_back(ask.id);
    assert_eq!(decided[0]["reason"], reason);
    let prompts = wiring.agent.turn_prompts();
    assert!(
        prompts.iter().any(|prompt| prompt.contains(&reason)),
        "{prompts:?}"
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        kinds(&detail)
    );
}

/// A bare `send_back` given while the landing recheck parks the waiting
/// run for a conflict (the supervisor stands still where the recheck found
/// it: another's landing moved main) applies once the run waits
/// again: `landing_decided` names the review's findings and the ask, the
/// resume's prompt holds them, and the ask closes. That an answer waits
/// while the run is not awaiting integration is
/// `supervise::execution::landing::landing_answer_to_apply`'s.
#[test]
fn a_bare_send_back_given_while_the_recheck_parks_the_run_applies_once_it_waits_again() {
    let landing_ask_id = std::sync::Arc::new(std::sync::Mutex::new(None::<AskId>));
    let answered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wiring = Wiring::new(
        "a_bare_send_back_given_while_the_recheck_parks_the_run_applies_once_it_waits_again",
        Script {
            receipts: vec![
                succeeded,
                succeeded_at_the_head,
                succeeded_at_the_head,
                succeeded_at_the_head,
            ],
            verdicts: vec![
                json!({"verdict": "concern", "reasons": ["add a unit test"], "summary": "a concern"}),
                verdict("pass", "fine"),
            ],
        },
    );
    // Answer the ask as the recheck parks the run, once.
    let wiring = {
        let (db, ask, answered) = (wiring.db.clone(), landing_ask_id.clone(), answered.clone());
        let generators = wiring.generators();
        wiring.with_pauses(std::sync::Arc::new(move |at, _| {
            let ask = *ask
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if at == crate::application::supervise::PausePoint::RecheckFailure
                && let Some(ask) = ask
                && !answered.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                SqliteQueue::open(&db)
                    .unwrap()
                    .with_generators(generators.clone())
                    .answer(ask, "send_back")
                    .unwrap();
            }
        }))
    };
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, task);
    *landing_ask_id
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ask.id);

    // Main moves on by another's landing, and the recheck finds the
    // waiting run conflicting with it.
    wiring.git.conflict_once_main_moves(&["change.txt"]);
    wiring.git.move_main();
    wiring.supervise_once();
    assert!(answered.load(std::sync::atomic::Ordering::SeqCst));
    // A later pass applies it if the first supervisor ended before it.
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    assert_eq!(
        payloads(&detail, "landing_recheck_failed").len(),
        1,
        "{:?}",
        kinds(&detail)
    );
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 1, "{decided:?} {:?}", kinds(&detail));
    let reason = format!(
        "the review's findings were sent back by ask {}: add a unit test",
        ask.id
    );
    assert_eq!(decided[0]["reason"], reason);
    let prompts = wiring.agent.turn_prompts();
    assert!(
        prompts.iter().any(|prompt| prompt.contains(&reason)),
        "{prompts:?}"
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
}

/// A supervisor that drains for a stop applies the `land`, `send_back`,
/// `cancel` and `retry_review` answers of runs nobody leases on its passes
/// and closes their asks (ADR-t3330-1), but starts no new work: no claim,
/// no resume of the run sent back, no review of the run queued to be
/// reviewed again, and, with no run of its own left, no landing of the
/// approved run, which stays queued.
#[test]
fn a_draining_supervisor_applies_the_landing_answers_and_starts_no_new_work() {
    // One slot: the tasks are reviewed one after another, task 4's review
    // failing.
    let wiring = Wiring::new(
        "a_draining_supervisor_applies_the_landing_answers_and_starts_no_new_work",
        Script {
            receipts: vec![succeeded, succeeded, succeeded, succeeded],
            verdicts: vec![concern(), concern(), concern(), no_verdict()],
        },
    )
    .with_parallel(1);
    let tasks: Vec<TaskId> = (0..4).map(|_| wiring.ready_task(&["true"])).collect();
    wiring.supervise_once();
    let reviews = wiring.agent.review_prompts().len();
    let mut queue = wiring.queue();
    let answers: Vec<(AskId, &str)> = tasks
        .iter()
        .zip(["land", "send_back", "cancel", "retry_review"])
        .map(|(task, answer)| (landing_ask(&mut queue, *task).id, answer))
        .collect();
    for (ask, answer) in &answers {
        queue.answer(*ask, answer).unwrap();
    }
    let ready = wiring.ready_task(&["true"]);

    // The drain has room for every run: what it starts no slot keeps.
    wiring.set_parallel(4);
    wiring.supervise_draining();
    for (ask, _) in &answers {
        assert!(queue.read_ask(*ask).unwrap().closed_at.is_some(), "{ask}");
    }
    let landed = queue.show(tasks[0]).unwrap();
    assert_eq!(
        landed.runs[0].status(),
        RunStatus::AwaitingIntegration,
        "{:?}",
        kinds(&landed)
    );
    assert!(queue.run_lease(landed.runs[0].id()).unwrap().is_none());
    assert_eq!(payloads(&landed, "integration_approved").len(), 1);
    assert_eq!(payloads(&landed, "landing_queued").len(), 1);
    assert!(payloads(&landed, "integration_started").is_empty());
    let sent_back = queue.show(tasks[1]).unwrap();
    assert_eq!(sent_back.runs[0].status(), RunStatus::NeedsSession);
    assert_eq!(payloads(&sent_back, "landing_decided").len(), 1);
    assert!(payloads(&sent_back, "resume_started").is_empty());
    let canceled = queue.show(tasks[2]).unwrap();
    assert_eq!(canceled.runs[0].status(), RunStatus::Failed);
    assert_eq!(canceled.task.status(), TaskStatus::Canceled);
    let retried = queue.show(tasks[3]).unwrap();
    assert_eq!(payloads(&retried, "review_retry_queued").len(), 1);
    assert_eq!(payloads(&retried, "review_started").len(), 1);
    assert!(queue.show(ready).unwrap().runs.is_empty());
    assert_eq!(wiring.agent.review_prompts().len(), reviews);
}

/// A `land` answer given while a supervisor drains for a stop, its own run
/// still at work, is applied and the approved run lands in that drain,
/// before the drain ends with its own run (ADR-t3330-1). The approved run
/// is not reviewed again.
#[test]
fn a_drain_lands_an_approved_run_while_its_own_run_keeps_it_going() {
    let wiring = Wiring::new(
        "a_drain_lands_an_approved_run_while_its_own_run_keeps_it_going",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![concern(), verdict("pass", "fine")],
        },
    );
    let approved = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, approved);

    let own = wiring.ready_task(&["true"]);
    let (release, released) = mpsc::channel();
    wiring.agent.hold_next_turn(released);
    let mut stopped = false;
    let mut answered = false;
    let mut at_landing = None;
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        if !stopped {
            if running(queue, own) {
                stop();
                stopped = true;
            }
            return false;
        }
        if !answered {
            // Answered once the drain is recorded: no pass at work applies it.
            let draining = queue
                .all_events()
                .unwrap()
                .iter()
                .any(|event| event.kind.as_str() == "supervisor_draining");
            if draining {
                queue.answer(ask.id, "land").unwrap();
                answered = true;
            }
            return false;
        }
        if queue.show(approved).unwrap().runs[0].status() != RunStatus::Integrated {
            return false;
        }
        // Landed while the drain went on for its own run.
        at_landing = Some((
            running(queue, own),
            queue.read_ask(ask.id).unwrap().closed_at.is_some(),
        ));
        let _ = release.send(());
        true
    });
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(at_landing, Some((true, true)));
    let detail = queue.show(approved).unwrap();
    assert_eq!(payloads(&detail, "integration_approved").len(), 1);
    assert_eq!(payloads(&detail, "integration_started").len(), 1);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    let own = queue.show(own).unwrap();
    assert_eq!(
        own.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        kinds(&own)
    );
    assert_eq!(wiring.agent.review_prompts().len(), 2);
}

/// A drain with no run of its own applies a `land` answer and ends without
/// landing it; the next supervisor lands the approved run from its events,
/// without reviewing or asking again (ADR-t3330-1).
#[test]
fn a_drain_with_no_run_of_its_own_leaves_the_approved_run_to_the_next_supervisor() {
    let wiring = Wiring::new(
        "a_drain_with_no_run_of_its_own_leaves_the_approved_run_to_the_next_supervisor",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![concern()],
        },
    );
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, task);
    queue.answer(ask.id, "land").unwrap();

    wiring.supervise_draining();
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let detail = queue.show(task).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::AwaitingIntegration,
        "{:?}",
        kinds(&detail)
    );
    assert_eq!(payloads(&detail, "landing_queued").len(), 1);
    assert!(payloads(&detail, "integration_started").is_empty());

    let base = wiring.git.main();
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        kinds(&detail)
    );
    assert_ne!(wiring.git.main(), base);
    assert_eq!(payloads(&detail, "integration_approved").len(), 1);
    assert_eq!(payloads(&detail, "integration_started").len(), 1);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert_eq!(payloads(&detail, "ask_opened").len(), 1);
    assert_eq!(wiring.agent.review_prompts().len(), 1);
}

/// A higher-priority worker keeps the only slot while a send-back parks
/// an otherwise resumable run. Full passes persist one wait, status reads
/// it, and releasing the worker lets the same run resume and end the wait.
#[test]
fn a_resume_waits_once_for_an_occupied_slot_then_resumes() {
    use crate::application::TaskStore;
    use crate::domain::{AttentionNext, Priority};
    let wiring = Wiring::new(
        "a_resume_waits_once_for_an_occupied_slot_then_resumes",
        Script {
            receipts: vec![succeeded, succeeded, succeeded_at_the_head],
            verdicts: vec![concern(), verdict("pass", "fine")],
        },
    )
    .with_parallel(1)
    .with_tick(std::time::Duration::from_millis(1));
    let first = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, first);
    let second = wiring.ready_task(&["true"]);
    queue.set_priority(second, Some(Priority::Urgent)).unwrap();
    let (release, held) = mpsc::channel();
    wiring.agent.hold_next_turn(held);
    let mut release = Some(release);
    // Both candidates enter the same fill line: the urgent claim takes
    // the slot before the normal-priority resume.
    queue
        .answer(ask.id, "send_back: add the missing check")
        .unwrap();
    let mut observed = 0;
    wiring.supervise_once_while(|queue| {
        let detail = queue.show(first).unwrap();
        let waits = payloads(&detail, "resume_slot_wait_started");
        if waits.is_empty() {
            return false;
        }
        assert_eq!(waits.len(), 1);
        assert_eq!(waits[0]["reason"], "slots_full");
        assert_eq!(waits[0]["used_slots"], 1);
        assert_eq!(waits[0]["resumes"]["counted_limit"], 3);
        let status = wiring.status();
        assert!(
            status["attention"].as_array().unwrap().iter().any(|row| {
                row["task_id"] == json!(first)
                    && row["next"] == AttentionNext::WaitingForResumeSlot.to_string()
            }),
            "{status}"
        );
        observed += 1;
        if observed < 3 {
            return false;
        }
        release.take().unwrap().send(()).unwrap();
        true
    });
    let detail = wiring.queue().show(first).unwrap();
    assert_eq!(payloads(&detail, "resume_slot_wait_started").len(), 1);
    assert_eq!(payloads(&detail, "resume_slot_wait_ended").len(), 1);
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    assert_eq!(detail.runs.len(), 1);
    assert!(observed >= 3);
}

/// Answer first, pass later: both paths share one queue, and releasing the
/// first verifier lets the next supervisor pass start the later run.
#[test]
fn an_earlier_land_answer_and_a_later_pass_share_one_visible_landing_order() {
    let wiring = Wiring::new(
        "an_earlier_land_answer_and_a_later_pass_share_one_visible_landing_order",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![concern(), verdict("pass", "fine")],
        },
    )
    .with_parallel(2)
    .with_tick(std::time::Duration::from_millis(1))
    .with_continuous_passes();
    let first = wiring.ready_task(&["first-queue-check"]);
    let (release, wait) = mpsc::channel();
    wiring.verifier.on_command(
        "first-queue-check",
        Box::new(move || {
            wait.recv_timeout(super::harness::LIMIT)
                .expect("observer releases first landing");
        }),
    );
    let mut release = Some(release);
    let mut second = None;
    let mut observed = None;
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        let first_detail = queue.show(first).unwrap();
        let Some(second_id) = second else {
            // Keep one supervisor alive across the answer and the later
            // pass. The later task is created only after answering land.
            let Some(run) = first_detail.runs.first() else {
                return false;
            };
            let Some(ask) = queue
                .asks(Default::default())
                .unwrap()
                .into_iter()
                .find(|ask| {
                    ask.kind == AskKind::ApproveLanding && ask.run_id.as_ref() == Some(run.id())
                })
            else {
                return false;
            };
            queue.answer(ask.id, "land").unwrap();
            second = Some(wiring.ready_task(&["second-queue-check"]));
            return false;
        };
        let second_detail = queue.show(second_id).unwrap();
        if observed.is_none() {
            if payloads(&first_detail, "integration_started").is_empty()
                || payloads(&second_detail, "landing_queue_entered").is_empty()
                || queue
                    .run_lease(second_detail.runs[0].id())
                    .unwrap()
                    .is_some()
            {
                return false;
            }
            observed = Some((first_detail, second_detail, wiring.status()));
            if let Some(release) = release.take() {
                let _ = release.send(());
            }
            return false;
        }
        if first_detail.task.status() == TaskStatus::Completed
            && second_detail.task.status() == TaskStatus::Completed
        {
            stop();
            return true;
        }
        false
    });
    // Assert only after the held landing and the supervisor were joined.
    let second = second.expect("the answer enabled the later task");
    let (first_detail, second_detail, status) =
        observed.expect("both runs were observed in the queue");
    assert!(payloads(&second_detail, "integration_started").is_empty());
    assert_eq!(payloads(&first_detail, "landing_queue_entered").len(), 1);
    let line = status["landing_queue"].as_array().unwrap();
    assert_eq!(line.len(), 2, "{status}");
    assert_eq!(line[0]["task_id"], json!(first));
    assert_eq!(line[1]["task_id"], json!(second));
    assert_eq!(line[0]["position"], 1);
    assert_eq!(line[1]["position"], 2);
    assert_eq!(line[1]["waiting_for"], "integrating");
    let mut queue = wiring.queue();
    assert!(
        first_event(&mut queue, first, "integration_started")
            < first_event(&mut queue, second, "integration_started")
    );
    for task in [first, second] {
        let detail = queue.show(task).unwrap();
        assert_eq!(detail.task.status(), TaskStatus::Completed);
        let entered = payloads(&detail, "landing_queue_entered");
        let left = payloads(&detail, "landing_queue_left");
        assert_eq!((entered.len(), left.len()), (1, 1));
        assert_eq!(left[0]["entry_id"], entered[0]["entry_id"]);
        assert_eq!(left[0]["reason"], "landed");
    }
    let reported = outcome["runs"].as_array().unwrap();
    assert_eq!(reported.len(), 2, "{outcome}");
    for (run, task) in reported.iter().zip([first, second]) {
        assert_eq!(run["task_id"], json!(task));
        assert_eq!(run["status"], "integrated");
    }
    assert_eq!(wiring.status()["landing_queue"], json!([]));
}

/// Set up a run at the direct approval boundary without running a worker
/// or supervisor. Each test supplies its own authorization history.
fn direct_landing_fixture(
    wiring: &Wiring,
    queue: &mut SqliteQueue,
    priority: crate::domain::Priority,
    status: RunStatus,
) -> crate::domain::TaskRun {
    direct_landing_fixture_with_commands(wiring, queue, priority, status, &[])
}

fn direct_landing_fixture_with_commands(
    wiring: &Wiring,
    queue: &mut SqliteQueue,
    priority: crate::domain::Priority,
    status: RunStatus,
    commands: &[&str],
) -> crate::domain::TaskRun {
    use crate::application::{Repository, TaskStore};
    use crate::domain::ClaimOutcome;
    use std::path::Path;

    let task = wiring.ready_task(commands);
    queue.set_priority(task, Some(priority)).unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&wiring.git.main()).unwrap() else {
        panic!("claim the fixture task");
    };
    assert_eq!(run.task_id(), task);
    let connection = super::harness::connect(&wiring.db);
    connection
        .execute(
            "UPDATE task_runs SET status=?2, worktree_path=?3, branch=?4, run_dir=?5, receipt_path=?6 WHERE id=?1",
            rusqlite::params![
                run.id().as_str(),
                status.as_str(),
                format!("/wiring/direct-{task}/worktree"),
                format!("direct-{task}"),
                format!("/wiring/direct-{task}/run"),
                format!("/wiring/direct-{task}/run/receipt.json"),
            ],
        )
        .unwrap();
    let run = queue.run(run.id()).unwrap();
    wiring.git.create_worktree(&run).unwrap();
    wiring.files.put_dir(Path::new(run.run_dir().unwrap()));
    let head = wiring
        .git
        .commit_in(Path::new(run.worktree_path().unwrap()))
        .unwrap();
    connection
        .execute(
            "UPDATE task_runs SET result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id().as_str(), head.as_str()],
        )
        .unwrap();
    queue.run(run.id()).unwrap()
}

fn authorize_resolved_resume(queue: &mut SqliteQueue, run: &crate::domain::TaskRun) {
    use crate::domain::EventKind;
    queue
        .record_runtime_event(
            run.id(),
            EventKind::IntegrationApproved,
            json!({"ask_id": 1}),
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::ResumeFinished,
            json!({"outcome": "resolved", "status": "needs_session", "approved": true}),
        )
        .unwrap();
}

fn reviewed_landing_waiting_for_an_answer(
    wiring: &Wiring,
    queue: &mut SqliteQueue,
) -> (crate::domain::TaskRun, AskId) {
    use crate::domain::{AskReason, EventKind, NewAsk};

    let head = direct_landing_fixture(
        wiring,
        queue,
        crate::domain::Priority::High,
        RunStatus::AwaitingIntegration,
    );
    queue
        .record_runtime_event(
            head.id(),
            EventKind::ReviewFinished,
            json!({"verdict":"pass"}),
        )
        .unwrap();
    queue
        .record_runtime_event(head.id(), EventKind::LandingQueued, json!({"via":"review"}))
        .unwrap();
    let ask = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: vec!["adr_conflict".into()],
            kind: AskKind::WorkerQuestion,
            task_id: Some(head.task_id()),
            run_id: Some(head.id().clone()),
            question: "Which behavior is required?".into(),
            options: vec![],
            asked_by: "worker".into(),
            reason_category: AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    (head, ask.id)
}

#[test]
fn a_named_landing_with_an_open_ask_keeps_its_disk_reservation() {
    use crate::application::{
        integrate::IntegrateTarget, supervise::repository_config::StartupRules,
    };
    use std::{
        sync::{Arc, atomic::Ordering},
        time::Duration,
    };

    const GIB: u64 = 1 << 30;
    let wiring = Wiring::new(
        "a_named_landing_with_an_open_ask_keeps_its_disk_reservation",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_tick(Duration::from_millis(1))
    .with_repository_startup_file(Arc::new(|| {
        let config = crate::infrastructure::run_env::parse_config(
            "[disk]\nestimate = 'fixed'\nrun_size = '20GiB'\nlanding_size = '30GiB'\n",
        )?;
        Ok(StartupRules {
            stall: config.stall,
            disk: config.disk,
            resume: config.resume,
            review_jobs: config.review_jobs,
        })
    }));
    let mut queue = wiring.queue();
    queue.bind_repository("/wiring/repo/.git").unwrap();
    let (run, ask) = reviewed_landing_waiting_for_an_answer(&wiring, &mut queue);
    let landing = wiring
        .approve_integration(IntegrateTarget::Task(run.task_id()))
        .unwrap()
        .unwrap();
    assert_eq!(landing.run.status(), RunStatus::Integrating);

    // Keep the named landing at its start boundary while the supervisor
    // reads the space that a claim would need with its budget occupied.
    wiring.free.store(35 * GIB, Ordering::SeqCst);
    let mut observed = None;
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        observed = queue
            .supervisors()
            .unwrap()
            .into_iter()
            .find_map(|owner| queue.supervisor_disk_status(&owner.token).unwrap());
        if observed.is_some() {
            stop();
        }
        observed.is_some()
    });

    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(queue.read_ask(ask).unwrap().is_open());
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::Integrating
    );
    let disk = observed.expect("the supervisor's disk decision");
    assert_eq!(disk["integration_slot"], json!([run.id()]));
    assert_eq!(disk["needs"]["moving"], 1, "{disk}");
    assert_eq!(disk["needs"]["integrators"], 1, "{disk}");
    assert_eq!(disk["needs"]["remaining"], 30 * GIB, "{disk}");
    assert_eq!(disk["claims"], 0, "{disk}");
}

/// An unanswered ask blocks the review pass at the head of LandingQueue.
/// While it cannot progress, the idle disk rule lets an interrupt claim.
#[test]
fn admission_leaves_a_review_pass_waiting_and_claims_an_interrupt() {
    use crate::application::TaskStore;
    use crate::domain::Priority;

    let wiring = Wiring::new(
        "admission_leaves_a_review_pass_waiting_and_claims_an_interrupt",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_tick(std::time::Duration::from_millis(1));
    // Stop after the claim write, before an unrelated worker lifecycle.
    wiring.fail_claim_launches(true);
    let mut queue = wiring.queue();
    let (head, _) = reviewed_landing_waiting_for_an_answer(&wiring, &mut queue);
    let interrupt = wiring.ready_task(&["true"]);
    queue
        .set_priority(interrupt, Some(Priority::Interrupt))
        .unwrap();
    let outcome = wiring.supervise_once_reporting();
    assert!(
        outcome["errors"]
            .to_string()
            .contains("refuses to start the wrapper"),
        "{outcome}"
    );

    let detail = queue.show(head.task_id()).unwrap();
    assert_eq!(
        payloads(&detail, "landing_queue_entered")[0]["basis"],
        "review_finished"
    );
    assert_eq!(
        payloads(&detail, "landing_queue_waiting")[0]["waiting_for"],
        "answer"
    );
    assert!(payloads(&detail, "integration_started").is_empty());
    assert!(queue.run_lease(head.id()).unwrap().is_none());
    assert_eq!(
        payloads(&queue.show(interrupt).unwrap(), "run_claimed").len(),
        1
    );
}

fn disk_admission_fixture(
    name: &str,
    free_at_admission: u64,
    needs_e2e: bool,
) -> (Wiring, TaskId, TaskId, crate::domain::EventId) {
    use crate::application::supervise::PausePoint;
    use std::sync::{Arc, atomic::Ordering};

    let wiring = disk_admission_wiring(
        name,
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let free = wiring.free.clone();
    free.store(free_at_admission, Ordering::SeqCst);
    let wiring = wiring.with_pauses(Arc::new(move |point, _| {
        if point == PausePoint::LandingStart {
            // Any later pass would read too little space for a new claim.
            free.store(0, Ordering::SeqCst);
        }
    }));
    wiring.fail_claim_launches(true);
    let (landing, approval_id) = approved_disk_landing(&wiring, needs_e2e);
    let claim = wiring.ready_task(&["true"]);
    (wiring, landing.task_id(), claim, approval_id)
}

fn disk_admission_wiring(name: &str, script: Script) -> Wiring {
    use crate::domain::disk::{DiskConfig, DiskEstimate};

    let wiring = Wiring::new(name, script)
        .with_tick(std::time::Duration::from_millis(1))
        .with_disk(DiskConfig {
            estimate: DiskEstimate::Fixed,
            run_size: Some(20 * (1 << 30)),
            landing_size: Some(20 * (1 << 30)),
            ..DiskConfig::default()
        });
    wiring.git.allow_clean_merges();
    wiring
}

fn approved_disk_landing(
    wiring: &Wiring,
    needs_e2e: bool,
) -> (crate::domain::TaskRun, crate::domain::EventId) {
    approved_landing_with_commands(wiring, needs_e2e, &[])
}

fn approved_landing_with_commands(
    wiring: &Wiring,
    needs_e2e: bool,
    commands: &[&str],
) -> (crate::domain::TaskRun, crate::domain::EventId) {
    use crate::application::{RunFiles, integrate::binding};
    use crate::domain::{EventKind, Priority};
    use std::path::Path;

    let mut queue = wiring.queue();
    let landing = direct_landing_fixture_with_commands(
        wiring,
        &mut queue,
        Priority::High,
        RunStatus::AwaitingIntegration,
        commands,
    );
    let receipt = succeeded_at_the_head(
        landing.id(),
        Path::new(landing.worktree_path().unwrap()),
        &wiring.git,
    )
    .unwrap();
    wiring
        .files
        .write(
            Path::new(landing.receipt_path().unwrap()),
            &serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
    if needs_e2e {
        queue
            .record_runtime_event(
                landing.id(),
                EventKind::ValidationFinished,
                json!({"e2e_requirement": {"required": true}}),
            )
            .unwrap();
    }
    let mut approval = json!({"push": false});
    let events = queue.run_events(landing.id()).unwrap();
    binding::put(
        &mut approval,
        &binding::of_run(&wiring.git, &landing, &events).unwrap(),
    );
    queue
        .record_runtime_event(landing.id(), EventKind::IntegrationApproved, approval)
        .unwrap();
    let approval_id = queue
        .run_events(landing.id())
        .unwrap()
        .into_iter()
        .find(|event| event.kind == EventKind::IntegrationApproved)
        .unwrap()
        .id;
    (landing, approval_id)
}

fn source_update_wiring(name: &str) -> (Wiring, crate::domain::CommitSha) {
    use crate::application::update;
    use crate::domain::EventKind;
    use std::sync::atomic::Ordering;

    let wiring = disk_admission_wiring(
        name,
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_auto_update();
    wiring.free.store(100 * (1 << 30), Ordering::SeqCst);
    wiring.git.set_dagq_source();
    wiring.git.commit_files(
        &[(
            "dagq.toml",
            "[review.programs.\"builtin:secrets\"]\nenabled = false\n",
        )],
        &["src/runtime.rs"],
    );
    let queue = wiring.queue();
    let base = wiring.git.main();
    for step in [EventKind::UpdateStarted, EventKind::UpdateFailed] {
        update::record(&queue, step, Some(base.as_str()), json!({"pid": 1})).unwrap();
    }
    wiring.git.move_main();
    (wiring, base)
}

fn source_update_starts(queue: &SqliteQueue) -> Vec<Value> {
    queue
        .update_events(20)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == crate::domain::EventKind::UpdateStarted)
        .map(|event| event.payload)
        .collect()
}

#[test]
fn a_runnable_landing_starts_before_the_source_update_wait_limit() {
    use crate::application::heavy_turn::HeavyTurn;

    let (wiring, base) =
        source_update_wiring("a_runnable_landing_starts_before_the_source_update_wait_limit");
    let (landing, _) = approved_landing_with_commands(&wiring, false, &["verify-first"]);
    let (pid, _) = wiring.processes().start();
    let (_release, held) = mpsc::channel();
    wiring.agent.hold_next_update(pid, held);

    wiring.supervise_once();

    let mut queue = wiring.queue();
    let detail = queue.show(landing.task_id()).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        payloads(&detail, "integration_held"),
    );
    assert!(
        wiring
            .verifier
            .ran()
            .iter()
            .any(|(cmd, _)| cmd == "verify-first")
    );
    assert_eq!(source_update_starts(&queue).len(), 1);

    let turn = HeavyTurn::under(wiring.db.parent().unwrap());
    let pending = turn
        .pending_update(&*wiring.files)
        .unwrap()
        .expect("queued update");
    assert_eq!(pending.since, wiring.generators().clock.now());
    assert_eq!(pending.base.as_deref(), Some(base.as_str()));
}

#[test]
fn landing_precedes_source_update_until_the_shared_wait_expires() {
    use crate::application::{
        heavy_turn::{HeavyTurn, PendingUpdate},
        update,
    };
    use crate::domain::{EventKind, heavy_turn::HeavyJob};
    use std::sync::Arc;
    use std::time::Duration;

    let (wiring, base) =
        source_update_wiring("landing_precedes_source_update_until_the_shared_wait_expires");
    let turn = HeavyTurn::under(wiring.db.parent().unwrap());
    let probe = wiring.files.observe_exclusive_lock(&turn.turn);
    let clock = wiring.generators().clock;
    {
        // Start with the pending record of an earlier supervisor. The
        // new one sees only the replacement head and the advanced clock.
        let _schedule = turn.try_schedule_update(&*wiring.files).unwrap().unwrap();
        turn.begin_update_wait(
            &*wiring.files,
            &PendingUpdate {
                since: clock.now(),
                base: Some(base.as_str().to_owned()),
            },
        )
        .unwrap();
    }
    let clock = Arc::new(super::host::StoppedClock::new(
        clock.system_time() + Duration::from_secs(7_201),
        clock.monotonic(),
    ));
    let wiring = wiring.with_clock(clock).with_continuous_passes();
    let head = wiring.git.move_main();
    let (second, _) = approved_landing_with_commands(&wiring, false, &["verify-second"]);
    let (pid, _) = wiring.processes().start();
    let (release, held) = mpsc::channel();
    wiring.agent.hold_next_update(pid, held);
    let queue = wiring.queue();
    let (mut gate, mut blocked_landing) = (None, None);
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        let detail = queue.show(second.task_id()).unwrap();
        if !payloads(&detail, "integration_started").is_empty() {
            stop();
            return true;
        }
        if blocked_landing.is_some() {
            return false;
        }
        if let Some((held, attempts)) = gate.take() {
            if probe.blocked_attempts() == attempts {
                gate = Some((held, attempts));
                return false;
            }
            // The supervisor reached the turn while the fake job held it.
            blocked_landing = Some((
                detail.runs[0].status(),
                wiring
                    .verifier
                    .ran()
                    .iter()
                    .any(|(cmd, _)| cmd == "verify-second"),
            ));
            update::record(
                queue,
                EventKind::UpdateFailed,
                Some(head.as_str()),
                json!({"pid": pid}),
            )
            .unwrap();
            wiring.processes().end(pid);
            release.send(()).unwrap();
            drop(held);
        } else if payloads(&detail, "landing_queue_waiting")
            .last()
            .is_some_and(|event| event["waiting_for"] == "turn")
        {
            // The child has not taken the lock: the reserved turn alone
            // kept this landing queued on the supervisor's first pass.
            assert_eq!(
                turn.try_e2e(&*wiring.files, wiring.processes())
                    .unwrap()
                    .err(),
                Some(HeavyJob::UpdateGate)
            );
            let held = turn
                .job_gate(&*wiring.files, pid, |_| {
                    panic!("the build should start immediately")
                })
                .unwrap();
            update::record(
                queue,
                EventKind::UpdateBuilt,
                Some(head.as_str()),
                json!({"pid": pid}),
            )
            .unwrap();
            gate = Some((held, probe.blocked_attempts()));
        }
        false
    });
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        blocked_landing,
        Some((RunStatus::AwaitingIntegration, false))
    );
    let updates = source_update_starts(&queue);
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0]["pid"], pid);
    assert_eq!(updates[0]["commit"], head.as_str());
    assert!(turn.pending_update(&*wiring.files).unwrap().is_none());
    assert_eq!(
        queue.run(second.id()).unwrap().status(),
        RunStatus::Integrated
    );
    assert!(
        wiring
            .verifier
            .ran()
            .iter()
            .any(|(cmd, _)| cmd == "verify-second")
    );
}

#[test]
fn admission_starts_the_queued_landing_and_claim_from_one_disk_reading() {
    use std::sync::atomic::Ordering;

    let (wiring, landing, claim, approval_id) = disk_admission_fixture(
        "admission_starts_the_queued_landing_and_claim_from_one_disk_reading",
        45 * (1 << 30),
        false,
    );
    let mut queue = wiring.queue();
    let outcome = wiring.supervise_once_reporting();

    assert!(
        outcome["errors"]
            .to_string()
            .contains("refuses to start the wrapper"),
        "{outcome}"
    );
    assert_eq!(wiring.free.load(Ordering::SeqCst), 0);
    let landed = queue.show(landing).unwrap();
    assert_eq!(
        payloads(&landed, "landing_queue_waiting").first().copied(),
        Some(&json!({"entry_id": approval_id, "judgment": approval_id, "waiting_for": null}))
    );
    assert_eq!(payloads(&landed, "integration_started").len(), 1);
    assert_eq!(
        payloads(&queue.show(claim).unwrap(), "run_claimed").len(),
        1
    );
}

#[test]
fn a_landing_that_loses_its_turn_leaves_a_disk_hold_until_claims_can_start() {
    use crate::application::{heavy_turn::HeavyTurn, supervise::PausePoint};
    use crate::domain::{EventKind, SessionRole};
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    const GIB: u64 = 1 << 30;
    // With the moving worker measured at its budget, standalone claims
    // need 45 GiB. Sharing with the measured landing needs only 25 GiB.
    const SHORT: u64 = 44 * GIB;
    const ROOM: u64 = 100 * GIB;
    let mut wiring = disk_admission_wiring(
        "a_landing_that_loses_its_turn_leaves_a_disk_hold_until_claims_can_start",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![verdict("pass", "meets the acceptance"); 2],
        },
    );
    let (landing, _) = approved_disk_landing(&wiring, false);
    wiring.moving_sizes = Arc::new(|path: &Path| {
        Ok(path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("worktree"))
            .then_some(20 * GIB))
    });
    let worker = wiring.ready_task(&["true"]);
    let (release, released) = mpsc::channel();
    wiring.agent.hold_next_turn(released);
    let files = wiring.files.clone();
    let turn = HeavyTurn::under(wiring.db.parent().unwrap());
    let free = wiring.free.clone();
    free.store(ROOM, Ordering::SeqCst);
    let allow = Arc::new(AtomicBool::new(false));
    let allow_landing = allow.clone();
    let lost_turn = Arc::new(AtomicBool::new(false));
    let lost_turn_at_start = lost_turn.clone();
    let landing_run = landing.id().clone();
    let wiring = wiring.with_pauses(Arc::new(move |point, run| {
        if point == PausePoint::LandingStart
            && run == Some(&landing_run)
            && !allow_landing.load(Ordering::SeqCst)
        {
            // Take the turn between the queue selection and landing start.
            files.hold_next_shared_lock(&turn.turn);
            if free.load(Ordering::SeqCst) == SHORT {
                lost_turn_at_start.store(true, Ordering::SeqCst);
            }
        }
    }));

    let mut claim = None;
    let mut held_id = None;
    let mut release = Some(release);
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        let Some(task) = claim else {
            let disk =
                queue.supervisors().unwrap().first().and_then(|supervisor| {
                    queue.supervisor_disk_status(&supervisor.token).unwrap()
                });
            let Some(disk) = disk else {
                return false;
            };
            // Wait for both observations from the size walk. The worker
            // has no unused reservation and the landing needs only the reserve.
            if !running(queue, worker)
                || disk["needs"]["claim"] != 45 * GIB
                || disk["needs"]["landing"] != 5 * GIB
            {
                return false;
            }
            wiring.free.store(SHORT, Ordering::SeqCst);
            if disk["free"] == SHORT {
                claim = Some(wiring.ready_task(&["true"]));
                lost_turn.store(false, Ordering::SeqCst);
            }
            return false;
        };
        let events = queue.all_events().unwrap();
        let status = || {
            wiring.status_at(
                wiring.generators().clock.system_time(),
                Some(SessionRole::Inbox),
            )
        };
        if let Some(id) = held_id {
            let Some(resumed) = events
                .iter()
                .find(|event| event.kind == EventKind::ClaimResumed && event.id > id)
            else {
                return false;
            };
            assert_eq!(resumed.payload["reason"], "disk_space");
            assert!(!payloads(&queue.show(task).unwrap(), "run_claimed").is_empty());
            assert_eq!(status()["claim_hold"], Value::Null);
            stop();
            release.take().unwrap().send(()).unwrap();
            return true;
        }
        let Some(held) = events
            .iter()
            .rev()
            .find(|event| event.kind == EventKind::ClaimHeld)
        else {
            return false;
        };
        let sample = events
            .iter()
            .rev()
            .find(|event| event.kind == EventKind::CandidatesSampled)
            .unwrap();
        if !lost_turn.load(Ordering::SeqCst) || sample.payload["candidates"] != 1 {
            return false;
        }
        assert_eq!(held.payload["reason"], "disk_space");
        assert_eq!(held.payload["admission_reason"], "Disk");
        assert_eq!(held.payload["threshold"], (45 * GIB) as f64);
        assert_eq!(status()["claim_hold"]["reason"], "disk_space");
        assert_eq!(sample.payload["claims_held"], true);
        assert_eq!(sample.payload["admission_reason"], "Disk");
        assert!(queue.show(task).unwrap().runs.is_empty());
        assert!(
            payloads(
                &queue.show(landing.task_id()).unwrap(),
                "integration_started"
            )
            .is_empty()
        );
        held_id = Some(held.id);
        wiring.free.store(ROOM, Ordering::SeqCst);
        allow.store(true, Ordering::SeqCst);
        false
    });
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(release.is_none());
}

#[test]
fn disk_competition_reaches_the_claim_hold_and_kpi_sample() {
    use crate::domain::EventKind;

    let (wiring, landing, claim, _) = disk_admission_fixture(
        "disk_competition_reaches_the_claim_hold_and_kpi_sample",
        10 * (1 << 30),
        false,
    );
    wiring.supervise_once();

    let mut queue = wiring.queue();
    assert_eq!(
        payloads(&queue.show(landing).unwrap(), "integration_started").len(),
        1
    );
    assert!(queue.show(claim).unwrap().runs.is_empty());
    let events = queue.all_events().unwrap();
    let sample = events
        .iter()
        .find(|event| event.kind == EventKind::CandidatesSampled)
        .unwrap();
    assert_eq!(sample.payload["admission_reason"], "DiskCompetition");
    assert_eq!(sample.payload["claims_held"], true);
    assert_eq!(sample.payload["candidates"], 1);
    let held = events
        .iter()
        .find(|event| event.kind == EventKind::ClaimHeld)
        .unwrap();
    assert_eq!(held.payload["admission_reason"], "DiskCompetition");
}

#[test]
fn a_queued_e2e_lease_leaves_an_equal_priority_claim_waiting_for_disk() {
    use crate::application::TaskStore;
    use crate::domain::{EventKind, Priority};

    let (wiring, landing, claim, _) = disk_admission_fixture(
        "a_queued_e2e_lease_leaves_an_equal_priority_claim_waiting_for_disk",
        10 * (1 << 30),
        true,
    );
    let mut queue = wiring.queue();
    queue.set_priority(claim, Some(Priority::High)).unwrap();
    wiring.supervise_once();

    let detail = queue.show(landing).unwrap();
    let lease = detail
        .events
        .iter()
        .find(|event| event.kind == EventKind::LeaseAcquired && event.payload["reason"] == "e2e")
        .expect("the selected landing leased its worktree for e2e");
    let events = queue.all_events().unwrap();
    let sample = events
        .iter()
        .find(|event| event.kind == EventKind::CandidatesSampled)
        .expect("the first admission pass recorded its claim decision");
    assert!(lease.id.as_i64() < sample.id.as_i64());
    assert_eq!(sample.payload["admission_reason"], "DiskCompetition");
    assert_eq!(sample.payload["claims_held"], true);
    assert_eq!(sample.payload["candidates"], 1);
    assert!(queue.show(claim).unwrap().runs.is_empty());
}

/// A real supervisor pass starts the queue head while its only worker
/// is still running. The e2e executor is a bounded fake.
#[test]
fn queued_e2e_starts_with_all_worker_slots_used() {
    use crate::application::{
        install::{E2eOutcome, E2eSettings},
        supervise::RunE2ePort,
    };
    use std::sync::{Arc, Mutex};

    let (release_e2e, e2e_held) = mpsc::channel();
    let e2e_held = Mutex::new(e2e_held);
    let (started, e2e_started) = mpsc::channel();
    let wiring = disk_admission_wiring(
        "queued_e2e_starts_with_all_worker_slots_used",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![concern()],
        },
    )
    .with_parallel(1)
    .with_run_e2e(RunE2ePort {
        prepare: Arc::new(|settings| Ok(Some(settings.clone()))),
        settings: E2eSettings {
            command: None,
            timeout: super::harness::LIMIT,
            run_env_root: None,
            queue_dir: None,
            scratch: Default::default(),
            log: Default::default(),
            utc_offset_secs: 0,
            lock: None,
            git_env: Vec::new(),
            ended: None,
        },
        run: Arc::new(move |_, _| {
            started.send(())?;
            e2e_held
                .lock()
                .unwrap()
                .recv_timeout(super::harness::LIMIT)?;
            Ok(E2eOutcome {
                passed: true,
                ..Default::default()
            })
        }),
        retry: std::time::Duration::ZERO,
    });
    let worker = wiring.ready_task(&["true"]);
    let (release_worker, worker_held) = mpsc::channel();
    wiring.agent.hold_next_turn(worker_held);
    let mut landing = None;
    wiring.supervise_once_while(|queue| {
        if !running(queue, worker) {
            return false;
        }
        if landing.is_none() {
            let status = wiring.status();
            let slots = &status["supervisors"][0]["slots"];
            assert_eq!(slots["used"], 1);
            assert_eq!(slots["parallel"], 1);
            assert_eq!(slots["landing_queue"], 0);
            landing = Some(approved_disk_landing(&wiring, true).0);
            return false;
        }
        if e2e_started.try_recv().is_err() {
            return false;
        }
        let status = wiring.status();
        let slots = &status["supervisors"][0]["slots"];
        assert_eq!(slots["parallel"], 1);
        assert_eq!(slots["used"], 1);
        assert_eq!(slots["landing_queue"], 1);
        let entry = &status["landing_queue"][0];
        assert_eq!(entry["run_id"], json!(landing.as_ref().unwrap().id()));
        assert_eq!(entry["position"], 1);
        assert_eq!(entry["e2e"], true);
        assert_eq!(entry["waiting_for"], "e2e_running");
        release_e2e.send(()).unwrap();
        release_worker.send(()).unwrap();
        true
    });
    let landing = landing.expect("the worker occupied the only worker slot");
    let detail = wiring.queue().show(landing.task_id()).unwrap();
    assert_eq!(payloads(&detail, "run_e2e_started").len(), 1);
    assert_eq!(
        payloads(&detail, "run_e2e_finished")[0]["outcome"],
        "passed"
    );
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
}

/// The same SQLite lease feeds status and the observations behind stats.
#[test]
fn status_separates_landing_from_workers_while_stats_counts_the_reservation() {
    use crate::application::live_inputs::{LiveSources, live_inputs};
    use crate::domain::{LeaseToken, Priority, Reason, ReasonCode, live_alerts};

    let wiring = Wiring::new(
        "status_separates_landing_from_workers_while_stats_counts_the_reservation",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let mut queue = wiring.queue();
    let run = direct_landing_fixture(
        &wiring,
        &mut queue,
        Priority::Normal,
        RunStatus::AwaitingIntegration,
    );
    wiring.ready_task_after(&[], &[run.task_id()]);
    let (pid, _) = wiring.processes().start();
    let live = LeaseToken::new("live");
    queue.register_supervisor(&live, pid, 2, "test").unwrap();
    let sources = LiveSources {
        files: &*wiring.files,
        signals: &*wiring.agent,
        config_file: &|| Ok(None),
    };
    let idle_slots = |queue: &SqliteQueue| {
        let now = wiring.generators().clock.now();
        let events = queue.all_events().unwrap();
        let inputs = live_inputs(queue, wiring.processes(), now, &events, &sources).unwrap();
        let judged = live_alerts::judge(&events, now, &inputs);
        assert_eq!(judged.slot_alerts.len(), 1);
        assert_eq!(judged.slot_alerts[0].kind, "idle_slots");
        judged.slot_alerts[0].value
    };
    for (token, used, free) in [(live, 1, 1), (LeaseToken::new("by-hand"), 0, 2)] {
        let landing = queue
            .begin_integration(run.id(), &token, &wiring.git.main())
            .unwrap();
        assert_eq!(landing.status(), RunStatus::Integrating);
        assert_eq!(
            wiring.status()["supervisors"][0]["slots"],
            json!({"used": 0, "landing_queue": used, "parallel": 2, "source": null}),
            "{token}"
        );
        assert_eq!(idle_slots(&queue), free, "{token}");

        let back = queue
            .abort_integration(
                run.id(),
                &token,
                "awaiting_integration",
                "back to review",
                &Reason::new(ReasonCode::Other),
            )
            .unwrap();
        assert_eq!(back.status(), RunStatus::AwaitingIntegration);
        assert!(queue.lease_for_review(run.id(), &token).unwrap().is_some());
        assert_eq!(
            wiring.status()["supervisors"][0]["slots"],
            json!({"used": used, "landing_queue": 0, "parallel": 2, "source": null}),
            "{token}"
        );
        assert_eq!(idle_slots(&queue), free, "{token}");
        queue.release_lease(run.id(), &token).unwrap();
    }
}

/// A stopped head keeps its queue position while the next entry lands.
#[test]
fn admission_lands_the_next_entry_while_the_head_waits_for_an_answer() {
    use crate::application::{RunFiles, integrate::binding};
    use crate::domain::{EventKind, Priority};
    use std::path::Path;

    let wiring = Wiring::new(
        "admission_lands_the_next_entry_while_the_head_waits_for_an_answer",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_tick(std::time::Duration::from_millis(1));
    wiring.git.allow_clean_merges();
    let mut queue = wiring.queue();
    let (head, ask) = reviewed_landing_waiting_for_an_answer(&wiring, &mut queue);
    let next = direct_landing_fixture(
        &wiring,
        &mut queue,
        Priority::Normal,
        RunStatus::AwaitingIntegration,
    );
    let receipt = succeeded_at_the_head(
        next.id(),
        Path::new(next.worktree_path().unwrap()),
        &wiring.git,
    )
    .unwrap();
    wiring
        .files
        .write(
            Path::new(next.receipt_path().unwrap()),
            &serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
    let mut approval = json!({"push": false});
    let events = queue.run_events(next.id()).unwrap();
    binding::put(
        &mut approval,
        &binding::of_run(&wiring.git, &next, &events).unwrap(),
    );
    queue
        .record_runtime_event(next.id(), EventKind::IntegrationApproved, approval)
        .unwrap();

    wiring.supervise_once();

    let head = queue.show(head.task_id()).unwrap();
    assert!(queue.read_ask(ask).unwrap().is_open());
    assert_eq!(head.runs[0].status(), RunStatus::AwaitingIntegration);
    assert_eq!(payloads(&head, "landing_queue_entered").len(), 1);
    assert_eq!(
        payloads(&head, "landing_queue_waiting").last().unwrap()["waiting_for"],
        "answer"
    );
    assert!(payloads(&head, "landing_queue_left").is_empty());
    assert!(payloads(&head, "integration_started").is_empty());
    let next = queue.show(next.task_id()).unwrap();
    assert_eq!(payloads(&next, "integration_started").len(), 1);
    assert_eq!(
        next.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        payloads(&next, "integration_error")
    );
}

#[test]
fn a_named_integrate_can_land_when_the_e2e_host_is_unavailable() {
    use crate::domain::{EventKind, run_e2e};

    let wiring = Wiring::new(
        "a_named_integrate_can_land_when_the_e2e_host_is_unavailable",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    wiring.git.allow_clean_merges();
    let mut queue = wiring.queue();
    queue.bind_repository("/wiring/repo/.git").unwrap();
    let (run, _) = approved_disk_landing(&wiring, true);
    queue
        .record_runtime_event(
            run.id(),
            EventKind::RunE2eFinished,
            json!({"commit": run.result_commit().unwrap(), "outcome": run_e2e::UNAVAILABLE}),
        )
        .unwrap();

    assert_eq!(
        wiring.integrate_outcome(run.task_id())["outcome"],
        "integrated"
    );
    let detail = queue.show(run.task_id()).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert!(payloads(&detail, "run_e2e_started").is_empty());
    let e2e = payloads(&detail, "run_e2e_finished");
    assert_eq!(e2e.len(), 1);
    assert_eq!(e2e[0]["outcome"], "unavailable");
}

/// A named approval is durable even when an earlier resolved resume
/// owns the next turn in the shared order.
#[test]
fn a_named_integrate_queues_its_approval_behind_an_earlier_resolved_resume() {
    use crate::application::integrate::IntegrateTarget;
    use crate::domain::Priority;

    let wiring = Wiring::new(
        "a_named_integrate_queues_its_approval_behind_an_earlier_resolved_resume",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let mut queue = wiring.queue();
    queue.bind_repository("/wiring/repo/.git").unwrap();
    let earlier = direct_landing_fixture(
        &wiring,
        &mut queue,
        Priority::Normal,
        RunStatus::NeedsSession,
    );
    authorize_resolved_resume(&mut queue, &earlier);
    let later = direct_landing_fixture(
        &wiring,
        &mut queue,
        Priority::Normal,
        RunStatus::AwaitingIntegration,
    );

    let error = wiring
        .approve_integration(IntegrateTarget::Task(later.task_id()))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("queued behind an earlier landing"),
        "{error:#}"
    );

    let earlier_detail = queue.show(earlier.task_id()).unwrap();
    let later_detail = queue.show(later.task_id()).unwrap();
    assert_eq!(payloads(&later_detail, "integration_approved").len(), 1);
    for (detail, run, basis) in [
        (&earlier_detail, &earlier, "integration_approved"),
        (&later_detail, &later, "integration_approved"),
    ] {
        let entries = payloads(detail, "landing_queue_entered");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["run_id"], json!(run.id()));
        assert_eq!(entries[0]["basis"], basis);
        assert!(payloads(detail, "integration_started").is_empty());
    }
}

/// `--next` skips a higher-priority run awaiting an answer and selects
/// among ready authorizations by priority and entry time.
#[test]
fn integrate_next_uses_priority_then_entry_time_including_resolved_resumes() {
    use crate::application::{TaskStore, integrate::IntegrateTarget};
    use crate::domain::{EventKind, Priority};

    let wiring = Wiring::new(
        "integrate_next_uses_priority_then_entry_time_including_resolved_resumes",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let mut queue = wiring.queue();
    queue.bind_repository("/wiring/repo/.git").unwrap();
    let (unanswered, ask) = reviewed_landing_waiting_for_an_answer(&wiring, &mut queue);
    queue
        .set_priority(unanswered.task_id(), Some(Priority::Urgent))
        .unwrap();
    let pending = direct_landing_fixture(
        &wiring,
        &mut queue,
        Priority::Urgent,
        RunStatus::NeedsSession,
    );
    queue
        .record_runtime_event(
            pending.id(),
            EventKind::IntegrationApproved,
            json!({"ask_id": 1}),
        )
        .unwrap();
    let normal = direct_landing_fixture(
        &wiring,
        &mut queue,
        Priority::Normal,
        RunStatus::AwaitingIntegration,
    );
    let later_high = direct_landing_fixture(
        &wiring,
        &mut queue,
        Priority::High,
        RunStatus::AwaitingIntegration,
    );
    let earlier_high =
        direct_landing_fixture(&wiring, &mut queue, Priority::High, RunStatus::NeedsSession);
    queue
        .record_runtime_event(
            normal.id(),
            EventKind::IntegrationApproved,
            json!({"ask_id": 1}),
        )
        .unwrap();
    queue
        .record_runtime_event(
            earlier_high.id(),
            EventKind::IntegrationApproved,
            json!({"ask_id": 1}),
        )
        .unwrap();
    queue
        .record_runtime_event(
            later_high.id(),
            EventKind::IntegrationApproved,
            json!({"ask_id": 2}),
        )
        .unwrap();
    // The first approval resolves after the second. Resolution enables
    // the start without assigning a new arrival time.
    queue
        .record_runtime_event(
            earlier_high.id(),
            EventKind::ResumeFinished,
            json!({"outcome": "resolved", "status": "needs_session", "approved": true}),
        )
        .unwrap();

    let request = wiring
        .approve_integration(IntegrateTarget::Next)
        .unwrap()
        .unwrap();
    assert_eq!(request.run.id(), earlier_high.id());
    assert_eq!(request.previous, RunStatus::NeedsSession);
    let unanswered_detail = queue.show(unanswered.task_id()).unwrap();
    assert!(queue.read_ask(ask).unwrap().is_open());
    assert!(payloads(&unanswered_detail, "integration_started").is_empty());
    assert!(queue.run_lease(unanswered.id()).unwrap().is_none());
    let pending_detail = queue.show(pending.task_id()).unwrap();
    assert!(payloads(&pending_detail, "landing_queue_entered").is_empty());
    assert!(payloads(&pending_detail, "integration_started").is_empty());
    for run in [&normal, &later_high, &earlier_high] {
        let detail = queue.show(run.task_id()).unwrap();
        let entries = payloads(&detail, "landing_queue_entered");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["run_id"], json!(run.id()));
        assert_eq!(
            payloads(&detail, "integration_started").len(),
            usize::from(run.id() == earlier_high.id())
        );
    }
}

/// A parked run keeps its approval when another landing holds the slot.
/// The supervisor must resume it before putting it in the landing queue.
#[test]
fn an_approved_run_blocked_by_an_active_landing_still_resumes() {
    use crate::application::integrate::IntegrateTarget;
    use crate::domain::{EventKind, Priority};

    let wiring = Wiring::new(
        "an_approved_run_blocked_by_an_active_landing_still_resumes",
        Script {
            receipts: vec![succeeded_at_the_head],
            verdicts: vec![],
        },
    )
    .with_tick(std::time::Duration::from_millis(1));
    let mut queue = wiring.queue();
    queue.bind_repository("/wiring/repo/.git").unwrap();
    let active = direct_landing_fixture(
        &wiring,
        &mut queue,
        Priority::Normal,
        RunStatus::AwaitingIntegration,
    );
    wiring
        .approve_integration(IntegrateTarget::Task(active.task_id()))
        .unwrap()
        .unwrap();
    let parked = direct_landing_fixture(
        &wiring,
        &mut queue,
        Priority::Normal,
        RunStatus::NeedsSession,
    );
    wiring.files.put(
        &std::path::Path::new(parked.run_dir().unwrap()).join("prompt.txt"),
        std::time::SystemTime::UNIX_EPOCH,
        "Resolve the conflict in the original worktree.",
    );
    queue
        .record_runtime_event(
            parked.id(),
            EventKind::IntegrationDeferred,
            json!({"reason": "resolve the conflict and continue"}),
        )
        .unwrap();

    let error = wiring
        .approve_integration(IntegrateTarget::Task(parked.task_id()))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains(&format!("run {} is integrating", active.id())),
        "{error:#}"
    );
    let detail = queue.show(parked.task_id()).unwrap();
    assert_eq!(payloads(&detail, "integration_approved").len(), 1);
    assert!(payloads(&detail, "landing_queue_entered").is_empty());
    assert_eq!(
        wiring.status()["landing_queue"].as_array().unwrap().len(),
        1
    );

    wiring.supervise_once();

    let detail = queue.show(parked.task_id()).unwrap();
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    let resumed = first_event(&mut queue, parked.task_id(), "resume_finished");
    assert_eq!(payloads(&detail, "resume_finished")[0]["approved"], true);
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
    assert!(resumed < first_event(&mut queue, parked.task_id(), "landing_queue_entered"));
    assert!(payloads(&detail, "integration_started").is_empty());
}
