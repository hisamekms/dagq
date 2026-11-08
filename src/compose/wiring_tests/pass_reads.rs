//! Read-count regression at the supervisor's service port. The ledger's
//! real builder is driven synchronously at pass boundaries, and the
//! measured pass waits for the startup cleanup job to be joined, so
//! neither catch-up races it; host ports remain the harness's fakes.
//! Counts belong to this caller and include the builder's and the
//! cleanup job's reads.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::Result;
use serde_json::{Value, json};

use super::{agent::Script, harness::Wiring};
use crate::{
    application::{
        Clock, TaskStore,
        supervise::{CleanupJobs, Retain, build_pass, start_builder},
        supervisor_service::{ServiceCaller, SupervisorQueues, request_reads_only},
    },
    domain::{
        ActorContext, ActorRole, EventKind, LeaseToken, ledger::builder::Builder,
        queue_service::UseCase,
    },
};

pub(super) struct AdvancedClock {
    base: Arc<dyn Clock>,
    seconds: AtomicU64,
}

impl Clock for AdvancedClock {
    fn system_time(&self) -> SystemTime {
        self.base.system_time() + Duration::from_secs(self.seconds.load(Ordering::SeqCst))
    }
    fn monotonic(&self) -> Instant {
        self.base.monotonic() + Duration::from_secs(self.seconds.load(Ordering::SeqCst))
    }
}

#[derive(Default)]
struct Counts {
    measuring: bool,
    reads: BTreeMap<String, usize>,
    initial_history_reads: usize,
    initial_sweeps: usize,
}

/// State of one wiring's pass probe, never shared with another test or DB.
/// Its synchronous ledger scheduling removes thread timing from the count,
/// while executing the production builder against the same service port.
pub(super) struct PassProbe {
    pub(super) clock: Arc<AdvancedClock>,
    /// The loop's cleanup jobs, which the measured pass waits for.
    pub(super) cleanup_jobs: Arc<CleanupJobs>,
    counts: Mutex<Counts>,
    builder: Mutex<Option<Builder>>,
    pass: AtomicU64,
    finished: AtomicBool,
}

impl PassProbe {
    fn new(clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            clock: Arc::new(AdvancedClock {
                base: clock,
                seconds: AtomicU64::new(0),
            }),
            cleanup_jobs: Arc::default(),
            counts: Mutex::default(),
            builder: Mutex::default(),
            pass: AtomicU64::new(0),
            finished: AtomicBool::new(false),
        })
    }

    pub(super) fn caller(
        self: &Arc<Self>,
        inner: Arc<dyn ServiceCaller>,
        stop: Arc<AtomicBool>,
    ) -> Arc<dyn ServiceCaller> {
        Arc::new(ProbedCaller {
            inner,
            probe: self.clone(),
            stop,
        })
    }
}

struct ProbedCaller {
    inner: Arc<dyn ServiceCaller>,
    probe: Arc<PassProbe>,
    stop: Arc<AtomicBool>,
}

impl ProbedCaller {
    fn count_read(&self, use_case: UseCase, params: &Value) {
        if request_reads_only(use_case, params) {
            let mut counts = self.probe.counts.lock().unwrap();
            let op = params["op"].as_str().unwrap();
            if counts.measuring {
                *counts.reads.entry(op.to_owned()).or_default() += 1;
            } else {
                if matches!(op, "last_turns_of" | "run_events") {
                    counts.initial_history_reads += 1;
                }
                if op == "ended_run_workspaces" {
                    counts.initial_sweeps += 1;
                }
            }
        }
    }

    fn build_ledger(&self) -> Result<()> {
        let caller = self.probe.caller(self.inner.clone(), self.stop.clone());
        let queue = SupervisorQueues::new(
            caller,
            ActorContext::instance(ActorRole::Supervisor, std::process::id()),
        )
        .queue();
        let now = self.probe.clock.now();
        let mut builder = self.probe.builder.lock().unwrap();
        if builder.is_none() {
            *builder = Some(start_builder(&queue, now * 1000)?);
        }
        // No retention is due: only the history, not unrelated daily work,
        // differs between the two queues.
        for _ in 0..100 {
            let built = build_pass(
                &queue,
                builder.as_mut().unwrap(),
                (now * 1000, &self.probe.clock.timestamp()),
                Retain::default(),
                None,
            );
            assert!(built.failure.is_none(), "{built:?}");
            if built.backlog == 0 {
                return Ok(());
            }
        }
        anyhow::bail!("ledger did not catch up within 100 bounded passes")
    }
}

impl ServiceCaller for ProbedCaller {
    fn call(&self, use_case: UseCase, params: Value) -> Result<Value> {
        self.count_read(use_case, &params);
        self.inner.call(use_case, params)
    }
    fn call_now(&self, use_case: UseCase, params: Value) -> Result<Value> {
        self.count_read(use_case, &params);
        // The underlying Resending still decides which records are kept.
        self.inner.call_now(use_case, params)
    }
    fn with_token(&self, file: &Path) -> Result<Arc<dyn ServiceCaller>> {
        Ok(self
            .probe
            .caller(self.inner.with_token(file)?, self.stop.clone()))
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
    fn holds_for_busy_queue(&self) -> bool {
        self.inner.holds_for_busy_queue()
    }
    fn keep(&self, use_case: UseCase, params: Value) -> Result<()> {
        self.inner.keep(use_case, params)
    }
    fn send_held(&self) -> Result<bool> {
        let sent = self.inner.send_held()?;
        // Production calls this at the start of each pass, before reads.
        self.probe.pass.fetch_add(1, Ordering::SeqCst);
        if self.probe.counts.lock().unwrap().measuring {
            self.probe.counts.lock().unwrap().measuring = false;
            self.probe.finished.store(true, Ordering::SeqCst);
            self.stop.store(true, Ordering::SeqCst);
        } else if !self.probe.finished.load(Ordering::SeqCst) {
            // Fold startup and the preceding pass before taking the count.
            self.build_ledger()?;
            if self.probe.counts.lock().unwrap().initial_sweeps > 0
                && self.probe.cleanup_jobs.all_joined()
            {
                // The first sweep returned before this pass boundary, and
                // the cleanup it asked for was joined: what that job
                // settled is cached, and none of its rechecks is left to
                // fall in the measured pass. Until then the clock stays,
                // so no sweep is due in the passes between.
                self.probe.clock.seconds.store(61, Ordering::SeqCst);
                self.probe.counts.lock().unwrap().measuring = true;
                self.build_ledger()?;
            }
        }
        Ok(sent)
    }
}

fn steady_pass_reads(history: usize) -> BTreeMap<String, usize> {
    let wiring = Wiring::new(
        "steady-pass-reads",
        Script {
            receipts: Vec::new(),
            verdicts: Vec::new(),
        },
    );
    let probe = PassProbe::new(wiring.generators().clock);
    let wiring = wiring.with_pass_probe(probe.clone());
    let mut queue = wiring.queue();
    let token = LeaseToken::new("history");
    for n in 0..history {
        let task = wiring.ready_task(&[]);
        let crate::domain::ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor(
                &crate::domain::CommitSha::try_from("a".repeat(40)).unwrap(),
                &token,
            )
            .unwrap()
        else {
            panic!("history task must be claimed")
        };
        let handle = format!("background:{}:gone", 3_000_000_000_u32 + n as u32);
        queue
            .plan_run(
                run.id(),
                &token,
                &crate::domain::RunPlan {
                    repo_path: "/wiring/repo".into(),
                    run_dir: format!("/wiring/history/{n}"),
                    branch: format!("history/{n}"),
                    worktree_path: format!("/wiring/worktree/{n}"),
                    receipt_path: format!("/wiring/history/{n}/receipt.json"),
                    log_path: format!("/wiring/history/{n}/log"),
                },
            )
            .unwrap();
        queue.workspace_created(run.id(), &token, &handle).unwrap();
        queue
            .record_runtime_event(
                run.id(),
                EventKind::WrapperLaunched,
                json!({"workspace_id": handle}),
            )
            .unwrap();
        queue
            .record_runtime_event(
                run.id(),
                EventKind::TurnStarted,
                json!({"pid": 2_000_000_000_u32 + n as u32, "start": "gone"}),
            )
            .unwrap();
        queue.release_lease(run.id(), &token).unwrap();
        queue.recover_run(run.id(), 0, json!({})).unwrap();
        queue
            .transition(task, crate::domain::TaskAction::Cancel)
            .unwrap();
    }
    assert_eq!(queue.ended_run_workspaces().unwrap().len(), history);
    drop(queue);
    wiring.supervise_once();
    assert!(probe.pass.load(Ordering::SeqCst) >= 3);
    let counts = probe.counts.lock().unwrap();
    assert!(
        counts.initial_history_reads > 0,
        "startup must read the history"
    );
    let reads = counts.reads.clone();
    assert_eq!(
        reads.get("ended_run_workspaces"),
        Some(&1),
        "the measured pass must sweep: {reads:?}"
    );
    assert!(
        !reads.contains_key("last_turns_of"),
        "the first sweep must already have checked the dead sessions: {reads:?}"
    );
    assert!(
        reads.contains_key("run_resources_after") && reads.contains_key("steps_after"),
        "include the caught-up ledger's reads: {reads:?}"
    );
    assert_eq!(
        reads.get("ended_run_worktrees"),
        Some(&1),
        "the measured sweep must list the ended runs' worktrees for cleanup: {reads:?}"
    );
    assert!(
        !reads.contains_key("ended_run_worktree"),
        "the startup cleanup must have settled every ended run: {reads:?}"
    );
    reads
}

/// Increasing ended history must not add queue reads to a caught-up pass,
/// including its due sweep and the cleanup it asks for.
#[test]
fn ended_history_does_not_increase_a_caught_up_sweep_pass_reads() {
    let small = steady_pass_reads(20);
    let large = steady_pass_reads(200);
    assert_eq!(
        small.values().sum::<usize>(),
        large.values().sum::<usize>(),
        "20 runs: {small:?}; 200 runs: {large:?}"
    );
    assert_eq!(
        small, large,
        "compare each read too, so changes cannot cancel out"
    );
}
