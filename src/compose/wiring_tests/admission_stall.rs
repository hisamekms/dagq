use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
};

use anyhow::Result;
use serde_json::{Value, json};

use super::{
    agent::{Script, succeeded, verdict},
    harness::Wiring,
};
use crate::{
    application::{TaskReads, supervisor_service::ServiceCaller},
    domain::{ClaimOutcome, LeaseToken, SessionRole, queue_service::UseCase},
};

#[derive(Default)]
struct Claims {
    attempts: AtomicUsize,
    opened_at: AtomicUsize,
    allow: AtomicBool,
}

/// Decline the transaction while leaving the ready candidate visible to
/// the real Admission pass. The next attempt after release uses the store.
struct Caller(Arc<dyn ServiceCaller>, Arc<Claims>);

impl ServiceCaller for Caller {
    fn call(&self, case: UseCase, params: Value) -> Result<Value> {
        match params["op"].as_str() {
            Some("claim_for_supervisor_holding") => {
                self.1.attempts.fetch_add(1, SeqCst);
                if !self.1.allow.load(SeqCst) {
                    return Ok(json!(ClaimOutcome::NoReadyTask));
                }
            }
            Some("record_queue_event") if params["args"][0] == "admission_stalled" => {
                self.1.opened_at.store(self.1.attempts.load(SeqCst), SeqCst);
            }
            _ => {}
        }
        self.0.call(case, params)
    }

    fn with_token(&self, file: &Path) -> Result<Arc<dyn ServiceCaller>> {
        self.0.with_token(file)
    }

    fn sign_in(&self, token: &LeaseToken, pid: u32) -> Result<()> {
        self.0.sign_in(token, pid)
    }

    fn take_back(&self, token: &LeaseToken, pid: u32) -> Result<()> {
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

#[test]
fn a_stalled_admission_notifies_the_inbox_once_and_closes_on_a_start() {
    let claims = Arc::new(Claims::default());
    let fault = claims.clone();
    let wiring = Wiring::new(
        "a_stalled_admission_notifies_the_inbox_once_and_closes_on_a_start",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    )
    .with_continuous_passes()
    .with_supervisor_file(Arc::new(|| {
        Ok(Some(crate::domain::slot_limits::SupervisorConfig {
            admission_stall_passes: Some(2),
            ..Default::default()
        }))
    }))
    .with_service_wrap(Arc::new(move |inner| {
        Arc::new(Caller(inner, fault.clone()))
    }));
    let task = wiring.ready_task(&[]);
    let mut observed = None;
    let mut closed = false;
    let outcome = wiring.supervise_once_while_stoppable(|_, stop| {
        let status = wiring.status_for(Some(SessionRole::Inbox));
        let attention = status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["kind"] == "admission_stalled");
        if !claims.allow.load(SeqCst) {
            if let Some(attention) = attention
                && claims.attempts.load(SeqCst) >= 5
            {
                observed = attention["last_error"].as_str().map(str::to_owned);
                claims.allow.store(true, SeqCst);
            }
        } else if attention.is_none() {
            closed = true;
            stop();
            return true;
        }
        false
    });
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(claims.opened_at.load(SeqCst), 3);
    assert_eq!(observed.as_deref(), Some("NoCandidates"));
    assert!(closed);
    assert_eq!(wiring.queue().show(task).unwrap().runs.len(), 1);

    let watched: Vec<_> = wiring
        .inbox_watch()
        .into_iter()
        .filter(|event| event["kind"] == "admission_stalled")
        .collect();
    assert_eq!(watched.len(), 1, "{watched:?}");
    assert_eq!(watched[0]["reasons"], json!(["NoCandidates"]));
    let events = wiring.queue().all_events().unwrap();
    let resumed: Vec<_> = events
        .iter()
        .filter(|event| event.kind.as_str() == "admission_resumed")
        .collect();
    assert_eq!(resumed.len(), 1);
    let claimed = events
        .iter()
        .find(|event| event.kind.as_str() == "run_claimed")
        .unwrap();
    assert!(claimed.id < resumed[0].id);
}
