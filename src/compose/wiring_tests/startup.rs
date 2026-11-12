//! Exec startup through the production entry point, with one lost service
//! reply at a selected operation. No host process or queue is involved.
use super::{
    agent::{Script, succeeded, verdict},
    harness::Wiring,
};
use crate::{
    application::{
        RunFiles, TaskReads,
        supervisor_service::{ServiceCaller, ServiceUnreachable},
        telemetry_tokens::Retries,
    },
    domain::{LeaseToken, queue_service::UseCase},
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

pub(super) struct Fault {
    op: &'static str,
    applied: bool,
    fired: Mutex<bool>,
}
impl Fault {
    pub(super) fn caller(
        self: &Arc<Self>,
        inner: Arc<dyn ServiceCaller>,
    ) -> Arc<dyn ServiceCaller> {
        Arc::new(Caller {
            inner,
            fault: self.clone(),
        })
    }
}
struct Caller {
    inner: Arc<dyn ServiceCaller>,
    fault: Arc<Fault>,
}
impl Caller {
    fn request(&self, case: UseCase, params: Value, now: bool) -> Result<Value> {
        let fire = {
            let mut fired = self.fault.fired.lock().unwrap();
            if !*fired && params["op"] == self.fault.op {
                *fired = true;
                true
            } else {
                false
            }
        };
        if fire {
            if matches!(self.fault.op, "record_runtime_event" | "record_queue_event") {
                assert!(now, "startup records must bypass automatic holding");
            }
            if self.fault.applied {
                if now {
                    self.inner.call_now(case, params)?;
                } else {
                    self.inner.call(case, params)?;
                }
                return Err(ServiceUnreachable::maybe_done("EOF").into());
            }
            return Err(ServiceUnreachable::not_done("EOF").into());
        }
        if now {
            self.inner.call_now(case, params)
        } else {
            self.inner.call(case, params)
        }
    }
}
impl ServiceCaller for Caller {
    fn call(&self, case: UseCase, params: Value) -> Result<Value> {
        self.request(case, params, false)
    }
    fn call_now(&self, case: UseCase, params: Value) -> Result<Value> {
        self.request(case, params, true)
    }
    fn with_token(&self, file: &Path) -> Result<Arc<dyn ServiceCaller>> {
        Ok(self.fault.caller(self.inner.with_token(file)?))
    }
    fn sign_in(&self, token: &LeaseToken, pid: u32) -> Result<()> {
        self.inner.sign_in(token, pid)
    }
    fn take_back(&self, token: &LeaseToken, pid: u32) -> Result<()> {
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
    fn send_held(&self) -> Result<bool> {
        self.inner.send_held()
    }
    fn holds_for_busy_queue(&self) -> bool {
        self.inner.holds_for_busy_queue()
    }
    fn keep(&self, case: UseCase, params: Value) -> Result<()> {
        self.inner.keep(case, params)
    }
}

/// Each test exercises one startup boundary on its own fixture, keeping
/// the in-process wiring tests within the layer's short time budget.
fn startup_rebuilds_with_saved_revocations(op: &'static str, applied: bool) {
    let wiring = Wiring::new(
        "startup-eof",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("concern", "inspect")],
        },
    );
    let task = wiring.ready_task(&["test -f seed.txt"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let run = queue.show(task).unwrap().runs[0].id().clone();
    let token = LeaseToken::new("exec-startup");
    wiring.register_at(&token, SystemTime::UNIX_EPOCH + super::host::STOPPED_AT);
    assert!(queue.lease_for_review(&run, &token).unwrap().is_some());
    let before = queue.run_events(&run).unwrap().last().unwrap().id;
    let fault = Arc::new(Fault {
        op,
        applied,
        fired: Mutex::new(false),
    });
    // Use the production persistence format, and observe the revocation
    // attempt independently at the telemetry port. Ordinary sweeps do not
    // revoke a token under this still-live lease.
    let retries: Retries = serde_json::from_value(json!({
        "pending": [{"lease_lost": run}]
    }))
    .unwrap();
    let saved = Retries::file(wiring.db.parent().unwrap(), &token);
    retries.save(&*wiring.files, &saved, &token).unwrap();
    let wiring = wiring.with_startup_fault(token.clone(), fault.clone());
    let passes = wiring.passes();
    wiring.supervise_once();
    assert_eq!(
        *fault.fired.lock().unwrap(),
        !op.is_empty(),
        "fault not reached: {op}"
    );
    assert!(wiring.passes() > passes, "no pass after {op}");
    let events = queue.run_events(&run).unwrap();
    let events: Vec<_> = events.iter().filter(|e| e.id > before).collect();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind.as_str() == "supervisor_handed_off")
            .count(),
        1,
        "{op}: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind.as_str() == "run_phase_changed" && e.payload["cause"] == "handoff"),
        "no rebuilt slot after {op}: {events:?}"
    );
    assert!(
        !wiring.files.is_file(&saved),
        "saved retries were not taken"
    );
    let revocations = wiring.telemetry_revocations();
    assert_eq!(
        revocations
            .iter()
            .filter(|(target, under)| target.id == run.as_str() && under.as_ref() == Some(&token))
            .count(),
        1,
        "saved revocation was lost or repeated after {op}: {revocations:?}"
    );
}

#[test]
fn clean_exec_keeps_saved_telemetry_revocations() {
    startup_rebuilds_with_saved_revocations("", false);
}

#[test]
fn exec_hold_read_eof_rebuilds_its_leased_slot() {
    startup_rebuilds_with_saved_revocations("asks", false);
}

#[test]
fn exec_lease_read_eof_keeps_saved_revocations_on_retry() {
    startup_rebuilds_with_saved_revocations("runs_leased_by", false);
}

#[test]
fn exec_plan_interrupt_eof_rebuilds_its_leased_slot() {
    startup_rebuilds_with_saved_revocations("interrupt_plan_reviews_for_handoff", true);
}

#[test]
fn exec_goal_interrupt_eof_rebuilds_its_leased_slot() {
    startup_rebuilds_with_saved_revocations("interrupt_goal_reviews_for_handoff", true);
}

#[test]
fn exec_handed_off_eof_records_once_and_rebuilds_its_slot() {
    startup_rebuilds_with_saved_revocations("record_runtime_event", true);
}

#[test]
fn exec_max_load_eof_rebuilds_its_leased_slot() {
    startup_rebuilds_with_saved_revocations("set_max_load", true);
}

#[test]
fn exec_start_record_eof_rebuilds_its_leased_slot() {
    startup_rebuilds_with_saved_revocations("record_queue_event", true);
}
