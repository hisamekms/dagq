//! The ruleset gate wired from GitHub's read to the queue (ADR-t4332-1):
//! the queue service's backend answers a runner's pull from the gate it is
//! given, and the service's watch step records the hold's event and its
//! own ask on the queue, which `status` shows until the rulesets verify.
//! The gate's judgment, its periods and the hold's changes are the unit
//! tests of `domain::ruleset_gate` and `application::queue_service`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use super::{agent::Script, harness::Wiring};
use crate::application::Clock;
use crate::application::queue_service::{
    RulesetGate, RulesetSource, ServiceBackend, watch_rulesets,
};
use crate::domain::ruleset_gate::test_support::{APPS, reference_read};
use crate::domain::ruleset_gate::{Apps, Read, UNVERIFIED_SECS, Unverified};
use crate::infrastructure::queue_service::{SqliteServiceBackend, SqliteStorage};

/// GitHub's answer as the test sets it.
struct Source(Arc<Mutex<Read>>);

impl RulesetSource for Source {
    fn read(&self, _: i64) -> Read {
        self.0.lock().unwrap().clone()
    }
    fn apps(&self) -> Apps {
        APPS
    }
}

/// The gate's clock, which the test moves on past a read's period.
struct Moving {
    secs: AtomicI64,
    origin: Instant,
}

impl Clock for Moving {
    fn system_time(&self) -> SystemTime {
        let secs = u64::try_from(self.secs.load(Ordering::SeqCst)).unwrap();
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }
    fn monotonic(&self) -> Instant {
        self.origin
    }
}

/// The queue service's backend on the test's queue with `gate`.
fn backend(wiring: &Wiring, gate: Option<Arc<RulesetGate>>) -> SqliteServiceBackend {
    let (reads, writes, detail, commands) = super::super::wiring_service_use_cases();
    SqliteServiceBackend {
        storage: SqliteStorage,
        oidc_configured: false,
        db: wiring.db.clone(),
        queue_dir: PathBuf::from("/wiring/ruleset-gate/queue"),
        generators: wiring.generators(),
        reads,
        writes,
        detail,
        commands,
        at_stop: None,
        stands_for: None,
        doctor: None,
        write_line: None,
        heavy_reads: None,
        ruleset_gate: gate,
    }
}

/// The rows of `status`'s attention for the rulesets' hold.
fn held_rows(wiring: &Wiring) -> Vec<Value> {
    let status = wiring.status();
    status["attention"]
        .as_array()
        .expect("the status has its attention")
        .iter()
        .filter(|entry| entry["kind"] == "runner_rulesets_held")
        .cloned()
        .collect()
}

/// A backend with no gate gives runners no work and records nothing; with
/// the gate, an unreadable answer holds assignment, records the hold and
/// opens its ask, and `status` shows the reason and the next step, still
/// after the ask is answered; once GitHub's rulesets verify, the backend
/// answers verified, the hold is resumed, its ask closed and the row gone.
#[test]
fn the_gate_reaches_the_backends_answer_the_queues_hold_and_status() {
    let wiring = Wiring::new(
        "ruleset-gate",
        Script {
            receipts: Vec::new(),
            verdicts: Vec::new(),
        },
    );
    let queue = wiring.queue();
    let none = backend(&wiring, None);
    assert_eq!(none.runner_rulesets(), Err(Unverified::NotConfigured));

    let answer = Arc::new(Mutex::new(Read::Unreadable));
    let clock = Arc::new(Moving {
        secs: AtomicI64::new(1_000),
        origin: Instant::now(),
    });
    let gate = Arc::new(RulesetGate::new(
        Box::new(Source(answer.clone())),
        clock.clone(),
    ));
    let with = backend(&wiring, Some(gate.clone()));
    assert_eq!(
        with.runner_rulesets(),
        Err(Unverified::Stale),
        "not read yet"
    );
    // One step of the service's watch.
    let watch = || {
        watch_rulesets(&gate, |now| {
            with.record_runner_rulesets(now.as_ref().map(|_| ()))
        })
        .unwrap();
    };
    watch();
    assert_eq!(with.runner_rulesets(), Err(Unverified::Unreadable));
    let held = queue
        .latest_queue_event(&["runner_rulesets_held"])
        .unwrap()
        .expect("the hold is recorded");
    assert_eq!(held.payload["reason"], "ruleset_unreadable");
    let ask = crate::domain::AskId::new(held.payload["ask_id"].as_i64().unwrap());
    let rows = held_rows(&wiring);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["status"], "held");
    assert_eq!(
        rows[0]["last_error"],
        json!(Unverified::Unreadable.message())
    );
    assert_eq!(rows[0]["next"], "fix the repository's rulesets");
    // The ask is answered while the rulesets still do not verify.
    wiring.queue().answer(ask, "done").unwrap();
    watch();
    assert_eq!(held_rows(&wiring), rows, "the row stays while held");

    // Fixed on GitHub: the next read after the period verifies.
    *answer.lock().unwrap() = reference_read();
    clock.secs.fetch_add(UNVERIFIED_SECS, Ordering::SeqCst);
    watch();
    assert_eq!(with.runner_rulesets(), Ok(()));
    let resumed = queue
        .latest_queue_event(&["runner_rulesets_resumed"])
        .unwrap()
        .expect("the resume is recorded");
    assert_eq!(resumed.payload["ask_id"], json!(ask));
    assert!(
        queue.read_ask(ask).unwrap().closed_at.is_some(),
        "the hold's ask is closed on resume"
    );
    assert!(held_rows(&wiring).is_empty());
}
