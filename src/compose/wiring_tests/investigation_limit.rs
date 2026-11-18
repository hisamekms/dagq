//! An open investigation's requests hold another goal's Spike at claim.
//! Claim ordering and limit counts are pure domain decisions; the real
//! supervisor wires their result through the memdb queue and read views.
use super::{
    agent::Script,
    harness::{Wiring, connect},
};
use crate::{
    application::{TaskReads, TaskStore, supervisor_service::ServiceCaller},
    domain::{
        ExecutionClass, GoalId, LeaseToken, NewGoal, TaskDetail, TaskId,
        plan_request::NewPlanRequest, queue_service::UseCase,
    },
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// End this wiring at the committed claim, before session provisioning.
/// Every call reaches the real service; only a successful claim ends it.
struct ClaimEndpoint(Arc<dyn ServiceCaller>);
const CLAIM_REACHED: &str = "wiring reached its committed claim";
impl ServiceCaller for ClaimEndpoint {
    fn call(&self, case: UseCase, params: Value) -> anyhow::Result<Value> {
        let claiming = params["op"] == "claim_for_supervisor_holding";
        let result = self.0.call(case, params)?;
        if claiming && result["outcome"] == "claimed" {
            anyhow::bail!(CLAIM_REACHED);
        }
        Ok(result)
    }
    fn with_token(&self, path: &Path) -> anyhow::Result<Arc<dyn ServiceCaller>> {
        Ok(Arc::new(Self(self.0.with_token(path)?)))
    }
    fn sign_in(&self, token: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.0.sign_in(token, pid)
    }
    fn take_back(&self, token: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.0.take_back(token, pid)
    }
    fn hand_over(&self) {
        self.0.hand_over();
    }
    fn sign_out(&self) {
        self.0.sign_out();
    }
    fn credential(&self) -> Option<PathBuf> {
        self.0.credential()
    }
}

fn payloads(detail: &TaskDetail, kind: &str) -> Vec<Value> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| event.payload.clone())
        .collect()
}
fn reasons(detail: &TaskDetail, kind: &str) -> Vec<String> {
    payloads(detail, kind)
        .iter()
        .map(|p| p["reason"].as_str().unwrap().into())
        .collect()
}
fn add(w: &Wiring, class: ExecutionClass, goal: Option<GoalId>) -> TaskId {
    let task = w.ready_task(&["test -f seed.txt"]);
    connect(&w.db)
        .execute(
            "UPDATE tasks SET execution_class=?2, goal_id=?3 WHERE id=?1",
            rusqlite::params![
                task,
                if class.is_spike() {
                    "spike"
                } else {
                    "implementation"
                },
                goal
            ],
        )
        .unwrap();
    task
}
fn decline(w: &Wiring, spike: TaskId) {
    let mut q = w.queue();
    let request = q
        .plan_requests(false)
        .unwrap()
        .into_iter()
        .find(|r| r.spike_task == Some(spike))
        .unwrap()
        .id;
    let planner = q.request_planner(request).unwrap();
    q.decline_request(request, "nothing to plan on", "planner", planner)
        .unwrap();
    if let Some(planner) = planner {
        q.register_planner_wrapper(planner, 1).unwrap();
        q.register_planner_agent(planner, 1, 1).unwrap();
        q.planner_exited(planner, 1, 0).unwrap();
    }
}

fn read(w: &Wiring, use_case: crate::domain::queue_service::UseCase) -> Value {
    use crate::{
        application::queue_service::{Peer, QueueService},
        domain::queue_service::{API_VERSION, ServiceRequest},
    };
    let backend = w.service_backend();
    let answer = QueueService {
        backend: &*backend,
        build: "wiring",
        pid: 1,
    }
    .handle_from_process(
        &ServiceRequest {
            caller: None,
            api_version: API_VERSION,
            token: None,
            env_role: None,
            queue: None,
            binding: None,
            use_case,
            params: json!({}),
        },
        Peer::Owner,
        Some(std::process::id()),
    );
    assert!(answer.ok, "{answer:?}");
    answer.result.unwrap()
}

#[test]
fn the_investigation_limit_holds_a_new_goal_s_spike_until_an_investigation_ends() {
    let w = Wiring::new(
        "investigation-limit",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_tick(Duration::from_millis(1))
    .with_continuous_passes()
    .with_service_wrap(Arc::new(|inner| Arc::new(ClaimEndpoint(inner))))
    .with_supervisor_file(Arc::new(|| {
        Ok(Some(crate::domain::slot_limits::SupervisorConfig {
            max_spikes: Some(1),
            max_investigations: Some(1),
            ..Default::default()
        }))
    }));
    let mut q = w.queue();
    let a = q
        .add_goal(NewGoal {
            title: "A".into(),
            ..Default::default()
        })
        .unwrap()
        .id();
    let b = q
        .add_goal(NewGoal {
            title: "B".into(),
            ..Default::default()
        })
        .unwrap()
        .id();
    let a1 = add(&w, ExecutionClass::Spike, Some(a));
    let a2 = add(&w, ExecutionClass::Spike, Some(a));
    // A completed round of two Spikes with open requests. Its historical
    // cycle and terminal tasks are fixture state; the landing/request
    // transaction has a separate Git/SQLite representative.
    let conn = connect(&w.db);
    conn.execute(
        "INSERT INTO spike_cycles(goal_id, task_id, opened_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![a, a1, w.generators().clock.now()],
    )
    .unwrap();
    for task in [a1, a2] {
        let request = q
            .record_plan_request(
                &NewPlanRequest {
                    text: "a completed Spike's verdict".into(),
                    note: None,
                    refs: vec![],
                    priority: None,
                },
                "inbox",
                "inbox",
            )
            .unwrap();
        conn.execute(
            "UPDATE plan_requests SET spike_task_id=?2 WHERE id=?1",
            rusqlite::params![request.id, task],
        )
        .unwrap();
        conn.execute("UPDATE tasks SET status='completed' WHERE id=?1", [task])
            .unwrap();
    }
    let b1 = add(&w, ExecutionClass::Spike, Some(b));
    let mut deferred = Vec::new();
    let outcome = w.supervise_once_while_stoppable(|queue, _| {
        let detail = queue.show(b1).unwrap();
        if payloads(&detail, "claim_deferred").is_empty() {
            return false;
        }
        assert!(payloads(&detail, "run_claimed").is_empty());
        deferred = reasons(&detail, "claim_deferred");
        assert_eq!(deferred, ["investigation_limit"]);
        let held = payloads(&detail, "claim_deferred").pop().unwrap();
        assert_eq!((&held["limit"], &held["running"]), (&json!(1), &json!(1)));
        let view = read(&w, crate::domain::queue_service::UseCase::Candidates);
        assert_eq!(view["candidates"], json!([]));
        assert_eq!(view["deferred"][0]["task_id"], json!(b1));
        assert_eq!(view["deferred"][0]["reason"], "investigation_limit");
        let graph = read(&w, crate::domain::queue_service::UseCase::Graph);
        assert_eq!(graph["candidates"], view["candidates"]);
        assert_eq!(graph["deferred"], view["deferred"]);
        // The remaining request keeps this investigation open; its last
        // decline must release the held Spike through the next claim pass.
        decline(&w, a2);
        assert_eq!(q.investigations().unwrap().open.len(), 1);
        assert!(payloads(&detail, "claim_deferral_ended").is_empty());
        decline(&w, a1);
        true
    });
    assert_eq!(deferred, ["investigation_limit"]);
    let errors = outcome["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{outcome}");
    assert!(errors[0].to_string().contains(CLAIM_REACHED), "{outcome}");
    let detail = q.show(b1).unwrap();
    let claimed = payloads(&detail, "run_claimed");
    assert_eq!(claimed.len(), 1, "{claimed:?}");
    assert_eq!(claimed[0]["investigation_opened"], true);
    assert_eq!(reasons(&detail, "claim_deferral_ended"), deferred);
}
