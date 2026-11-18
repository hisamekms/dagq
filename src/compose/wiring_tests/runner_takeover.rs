use crate::domain::queue_service;
use crate::infrastructure::sql_exec::{SqlWrite, TransactionEntries};
// In-process team control loop, real memdb selection/recovery, fake GitHub
// and runner work. No external process or runner clock enters lease judgment.
use crate::{
    application::{
        runner_enroll::{Credential, RunnerCredentialStore, RunnerExchange},
        runner_process::{RunnerPorts, run},
        supervise::{RunnerRefCleanup, runner_recovery_pass},
        *,
    },
    domain::{
        runner_assignment::{ClaimInputs, ClaimLimits, LeaseReport, Operation, Outcome, Poll},
        runner_lease::{LeaseState, Report, Verdict},
        *,
    },
    infrastructure::sqlite::SqliteQueue,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Clock(AtomicI64);
impl crate::application::Clock for Clock {
    fn system_time(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.0.load(Ordering::SeqCst) as u64)
    }
    fn monotonic(&self) -> Instant {
        panic!("no waits in the control loop")
    }
}
fn execute(q: &SqliteQueue, sql: &str) -> Result<()> {
    let tx = q.write_entry("runner_fixture")?;
    SqlWrite::execute(&tx, sql, &[])?;
    tx.commit()?;
    Ok(())
}
fn fixture() -> (SqliteQueue, Arc<Clock>) {
    let clock = Arc::new(Clock(AtomicI64::new(1_000_000)));
    let mut q = SqliteQueue::init(format!("file:/takeover-{}?vfs=memdb", uuid::Uuid::new_v4()))
        .unwrap()
        .with_generators(Generators {
            clock: clock.clone(),
            ids: Arc::new(crate::infrastructure::clock::UuidGenerator),
        });
    let task = q
        .add(NewTask {
            title: "t".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: vec![],
            required_evidence: vec![],
            paths: vec![],
            priority: None,
            change: None,
            dependencies: vec![],
            goal_dependencies: vec![],
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap();
    execute(
        &q,
        &format!(
            "UPDATE tasks SET status='ready', owner_issuer='i',owner_subject='s' WHERE id={}",
            task.id()
        ),
    )
    .unwrap();
    execute(
        &q,
        "INSERT INTO members(issuer,subject,role,on_call,created_at) VALUES ('i','s','admin',0,0)",
    )
    .unwrap();
    execute(
        &q,
        r#"INSERT INTO runners(id,owner_issuer,owner_subject,created_at,declaration) VALUES
        ('a','i','s',0,'{"os":"linux","arch":"aarch64","container":true,"credentials":[]}'),
        ('b','i','s',0,'{"os":"linux","arch":"aarch64","container":true,"credentials":[]}')"#,
    )
    .unwrap();
    execute(&q,"INSERT INTO runner_tokens(hash,runner_id,expires_at) VALUES ('a','a',99999999),('b','b',99999999)").unwrap();
    (q, clock)
}
fn poll(q: &SqliteQueue, who: &str) -> Outcome {
    q.runner_operation(
        who,
        &Operation::Poll(Poll {}),
        Some(&ClaimInputs {
            base: CommitSha::parse("a".repeat(40), "base").unwrap(),
            limits: ClaimLimits::default(),
        }),
        q.generators(),
    )
    .unwrap()
}
#[derive(Default)]
struct Refs {
    heads: RefCell<BTreeMap<i64, String>>,
    calls: RefCell<Vec<String>>,
    wrong: Cell<bool>,
    fail: Cell<Option<&'static str>>,
    failure_kind: Cell<Option<crate::infrastructure::github_refs::RefFailure>>,
    landing: RefCell<BTreeMap<i64, String>>,
}
impl Refs {
    fn call(&self, verb: &str, generation: i64) -> Result<()> {
        self.calls.borrow_mut().push(format!("{verb}{generation}"));
        if self.fail.get() == Some(verb) {
            return Err(self
                .failure_kind
                .get()
                .unwrap_or(crate::infrastructure::github_refs::RefFailure::Temporary)
                .into());
        }
        Ok(())
    }
}
impl RunnerRefs for Refs {
    fn delete_landing(&self, _run: &str, generation: i64) -> Result<()> {
        self.call("delete_landing", generation)?;
        self.landing.borrow_mut().remove(&generation);
        Ok(())
    }

    fn create(&self, _: &str, g: i64, head: &str) -> Result<()> {
        self.call("create", g)?;
        self.heads.borrow_mut().entry(g).or_insert(head.into());
        Ok(())
    }
    fn read(&self, _: &str, g: i64) -> Result<Option<String>> {
        self.call("read", g)?;
        Ok(if self.wrong.get() {
            Some("unaccepted".into())
        } else {
            self.heads.borrow().get(&g).cloned()
        })
    }
    fn delete(&self, _: &str, g: i64) -> Result<()> {
        self.call("delete", g)?;
        self.heads.borrow_mut().remove(&g);
        Ok(())
    }
    fn exists(&self, _: &str, g: i64) -> Result<bool> {
        Ok(self.heads.borrow().contains_key(&g))
    }
}
fn resume(q: &mut SqliteQueue, id: &RunId) {
    let token = LeaseToken::new("recovery");
    q.begin_triage(
        id,
        &token,
        None,
        &actor_model::ActorLaunch::default_of(actor_model::ModelRole::Recovery),
    )
    .unwrap()
    .unwrap();
    q.finish_triage(
        id,
        &token,
        &TriageAction::Resume {
            instruction: "continue accepted work".into(),
        },
        json!({}),
        vec![],
    )
    .unwrap();
    assert!(
        matches!(poll(q, "b"), Outcome::Assignment(None)),
        "recovery still owns its local coordination lease"
    );
    q.release_lease(id, &token).unwrap();
}

// Minimal runner-side fakes drive the production process through a real stale
// heartbeat refusal, so throwing away the work is not an assertion on a test's
// reimplementation of the cleanup rule.
struct Returning<'a> {
    queue: &'a SqliteQueue,
    first: RefCell<Option<Value>>,
    timer: Cell<u64>,
    stopped: Cell<bool>,
}
impl RunnerControl for Returning<'_> {
    fn call(
        &self,
        _: &str,
        _: &str,
        use_case: queue_service::UseCase,
        params: Value,
        _: Duration,
        _: &dyn Fn() -> bool,
    ) -> Result<queue_service::ServiceResponse> {
        if use_case == queue_service::UseCase::RunnerPoll {
            return Ok(queue_service::ServiceResponse::success(
                json!({"assignment":self.first.borrow_mut().take()}),
            ));
        }
        assert_eq!(use_case, queue_service::UseCase::RunnerHeartbeat);
        let report: LeaseReport = serde_json::from_value(params).unwrap();
        let outcome = self.queue.runner_operation(
            "a",
            &Operation::Heartbeat(report),
            None,
            self.queue.generators(),
        )?;
        assert!(matches!(outcome, Outcome::LeaseLost));
        self.stopped.set(true);
        Ok(queue_service::ServiceResponse::failure(
            queue_service::ServiceErrorCode::LeaseLost,
            "lease lost",
        ))
    }
}
impl RunnerTimer for Returning<'_> {
    fn now_ms(&self) -> u64 {
        self.timer.get()
    }
    fn sleep(&self, d: Duration) {
        self.timer.set(self.timer.get() + d.as_millis() as u64);
        assert!(self.timer.get() < 30_000);
    }
}
impl RunnerStopRequest for Returning<'_> {
    fn requested_at(&self) -> Option<u64> {
        self.stopped.get().then(|| self.timer.get())
    }
}
impl RunnerOutput for Returning<'_> {
    fn line(&self, _: &str) {}
}
impl RunnerExchange for Returning<'_> {
    fn exchange(
        &self,
        _: &str,
        _: queue_service::UseCase,
        _: Value,
    ) -> Result<queue_service::ServiceResponse> {
        Ok(queue_service::ServiceResponse::success(
            json!({"runner_id":"a", "refresh":"rotated", "token":"a", "expires_at":99999999}),
        ))
    }
}
impl RunnerCredentialStore for Returning<'_> {
    fn load(&self) -> Result<Credential> {
        Ok(Credential {
            control_url: "https://control.example".into(),
            runner_id: "a".into(),
            refresh: "old".into(),
        })
    }
    fn save(&self, _: &Credential) -> Result<()> {
        Ok(())
    }
}
#[derive(Default)]
struct Work {
    started: bool,
    stopped: bool,
    discarded: bool,
}
impl RunnerWork for Work {
    fn start(&mut self, _: &RunnerAssignment) -> Result<()> {
        self.started = true;
        Ok(())
    }
    fn turn_end(&mut self) -> Result<Option<RunnerTurnEnd>> {
        Ok(None)
    }
    fn stop(&mut self) {
        self.stopped = true;
    }
    fn discard(&mut self) {
        self.discarded = true;
    }
}

#[test]
fn expiry_recovers_then_verified_successor_assigns_elsewhere_and_old_runner_discards() {
    let (mut q, clock) = fixture();
    let mut cleanup = RunnerRefCleanup::default();
    let Outcome::Assignment(Some(first)) = poll(&q, "a") else {
        panic!("first grant")
    };
    let id = first.run.id().clone();
    let refs = Refs::default();
    let h = "b".repeat(40);
    q.report_runner_lease(
        id.as_str(),
        Report {
            runner_id: "a",
            generation: 1,
        },
        Some(&h),
        1_000_000,
    )
    .unwrap();
    q.verify_runner_ref(id.as_str(), 1, &h, 1_000_000).unwrap();
    // The old ref may have advanced beyond the accepted H: never read its tip.
    refs.heads.borrow_mut().insert(1, "unaccepted".into());
    runner_recovery_pass(&mut q, &refs, 1_089_999, &mut cleanup).unwrap();
    assert_eq!(
        q.runner_lease(id.as_str()).unwrap().unwrap().state,
        LeaseState::Held
    );
    clock.0.store(1_090_000, Ordering::SeqCst);
    runner_recovery_pass(&mut q, &refs, 1_090_000, &mut cleanup).unwrap();
    assert_eq!(
        q.runner_lease(id.as_str()).unwrap().unwrap().state,
        LeaseState::Lost
    );
    assert_eq!(q.run(&id).unwrap().status(), RunStatus::Interrupted);
    assert!(
        q.run_events(&id)
            .unwrap()
            .iter()
            .any(|e| e.kind == EventKind::RunRecovered)
    );
    assert!(matches!(poll(&q, "b"), Outcome::Assignment(None)));
    resume(&mut q, &id);
    refs.wrong.set(true);
    runner_recovery_pass(&mut q, &refs, 1_090_001, &mut cleanup).unwrap();
    assert_eq!(q.verified_ref(id.as_str()).unwrap().unwrap().generation, 1);
    assert!(matches!(poll(&q, "b"), Outcome::Assignment(None)));
    assert!(refs.heads.borrow().contains_key(&1));
    refs.wrong.set(false);
    refs.calls.borrow_mut().clear();
    runner_recovery_pass(&mut q, &refs, 1_090_002, &mut cleanup).unwrap();
    assert_eq!(*refs.calls.borrow(), ["create2", "read2"]);
    assert_eq!(q.verified_ref(id.as_str()).unwrap().unwrap().generation, 2);
    assert!(refs.heads.borrow().contains_key(&1));
    let Outcome::Assignment(Some(next)) = poll(&q, "b") else {
        panic!("successor grant")
    };
    assert_eq!(next.generation, 2);
    assert_eq!(next.run.id(), &id);
    runner_recovery_pass(&mut q, &refs, 1_090_003, &mut cleanup).unwrap();
    assert_eq!(*refs.calls.borrow(), ["create2", "read2", "delete1"]);
    assert_eq!(refs.heads.borrow().get(&2), Some(&h));
    let returning = Returning {
        queue: &q,
        first: RefCell::new(Some(serde_json::to_value(&first).unwrap())),
        timer: Cell::new(0),
        stopped: Cell::new(false),
    };
    let mut work = Work::default();
    run(
        RunnerPorts {
            control: &returning,
            exchange: &returning,
            credentials: &returning,
            work: &mut work,
            timer: &returning,
            stop: &returning,
            output: &returning,
        },
        Duration::from_secs(1),
    );
    assert!(work.started && work.stopped && work.discarded);
}

#[test]
fn revoked_or_disabled_runner_recovers_without_pids_and_headless_run_needs_no_ref() {
    for disable_owner in [false, true] {
        let (mut q, _) = fixture();
        let mut cleanup = RunnerRefCleanup::default();
        let Outcome::Assignment(Some(first)) = poll(&q, "a") else {
            panic!("grant")
        };
        execute(
            &q,
            if disable_owner {
                "UPDATE members SET disabled_at=1"
            } else {
                "UPDATE runners SET revoked_at=1 WHERE id='a'"
            },
        )
        .unwrap();
        let refs = Refs::default();
        runner_recovery_pass(&mut q, &refs, 1_000_001, &mut cleanup).unwrap();
        assert_eq!(
            q.run(first.run.id()).unwrap().status(),
            RunStatus::Interrupted
        );
        assert_eq!(
            q.runner_lease(first.run.id().as_str())
                .unwrap()
                .unwrap()
                .state,
            LeaseState::Lost
        );
        execute(&q, "UPDATE members SET disabled_at=NULL").unwrap();
        resume(&mut q, first.run.id());
        runner_recovery_pass(&mut q, &refs, 1_000_002, &mut cleanup).unwrap();
        assert!(refs.calls.borrow().is_empty());
        assert!(matches!(poll(&q, "b"), Outcome::Assignment(Some(_))));
    }
}

#[test]
fn failures_keep_the_old_ref_and_retry_readback_before_assignment() {
    for failure in ["create", "read", "mark", "delete"] {
        let (mut q, clock) = fixture();
        let mut cleanup = RunnerRefCleanup::default();
        let Outcome::Assignment(Some(first)) = poll(&q, "a") else {
            panic!("grant")
        };
        let id = first.run.id().clone();
        let h = "b".repeat(40);
        q.report_runner_lease(
            id.as_str(),
            Report {
                runner_id: "a",
                generation: 1,
            },
            Some(&h),
            1_000_000,
        )
        .unwrap();
        q.verify_runner_ref(id.as_str(), 1, &h, 1_000_000).unwrap();
        let refs = Refs::default();
        refs.heads.borrow_mut().insert(1, h.clone());
        clock.0.store(1_090_000, Ordering::SeqCst);
        runner_recovery_pass(&mut q, &refs, 1_090_000, &mut cleanup).unwrap();
        resume(&mut q, &id);
        if failure == "mark" {
            execute(&q, "CREATE TRIGGER fail_mark BEFORE INSERT ON runner_lease_records WHEN NEW.generation=2 BEGIN SELECT RAISE(ABORT,'injected mark failure'); END").unwrap();
        } else if failure != "delete" {
            refs.fail.set(Some(failure));
        }
        if failure != "delete" {
            assert!(runner_recovery_pass(&mut q, &refs, 1_090_001, &mut cleanup).is_err());
            assert_eq!(q.verified_ref(id.as_str()).unwrap().unwrap().generation, 1);
            assert!(matches!(poll(&q, "b"), Outcome::Assignment(None)));
            assert!(refs.heads.borrow().contains_key(&1));
            refs.fail.set(None);
            if failure == "mark" {
                execute(&q, "DROP TRIGGER fail_mark").unwrap();
            }
        }
        runner_recovery_pass(&mut q, &refs, 1_090_002, &mut cleanup).unwrap();
        let Outcome::Assignment(Some(next)) = poll(&q, "b") else {
            panic!("grant after durable mark")
        };
        assert_eq!(next.generation, 2);
        if failure == "delete" {
            refs.fail.set(Some("delete"));
            assert!(runner_recovery_pass(&mut q, &refs, 1_090_003, &mut cleanup).is_err());
            assert_eq!(refs.heads.borrow().get(&2), Some(&h));
            refs.fail.set(None);
        }
        runner_recovery_pass(&mut q, &refs, 1_090_004, &mut cleanup).unwrap();
        assert_eq!(*refs.heads.borrow(), BTreeMap::from([(2, h)]));
        assert!(!refs.calls.borrow().iter().any(|c| c == "read1"));
    }
}

#[test]
fn loss_rechecks_a_renewal_and_recovery_rechecks_the_generation() {
    let (mut q, _) = fixture();
    let Outcome::Assignment(Some(first)) = poll(&q, "a") else {
        panic!("grant")
    };
    let id = first.run.id().as_str();
    let old = q.runner_lease(id).unwrap().unwrap();
    assert!(matches!(
        q.report_runner_lease(
            id,
            Report {
                runner_id: "a",
                generation: 1
            },
            None,
            1_089_999
        )
        .unwrap(),
        Verdict::Accept { .. }
    ));
    assert_eq!(
        q.lose_runner_if_due(id, old.generation, 1_090_000).unwrap(),
        None
    );
    // Host-style recovery must not mistake this pid-less run for an orphan.
    assert!(q.recover_run(first.run.id(), 0, json!({})).is_err());
    q.lose_runner_lease(id, 1).unwrap();
    let lost = q.runner_lease(id).unwrap().unwrap();
    q.grant_runner_lease(id, "b", 1_090_001).unwrap();
    assert!(q.recover_runner(&lost).is_err());
    assert_eq!(q.runner_lease(id).unwrap().unwrap().generation, 2);
}

#[test]
fn terminal_ref_cleanup_retries_and_keeps_unfinished_history() {
    for canceled in [false, true] {
        let (mut q, _) = fixture();
        let mut cleanup = RunnerRefCleanup::default();
        let Outcome::Assignment(Some(first)) = poll(&q, "a") else {
            panic!("grant")
        };
        let h = "b".repeat(40);
        q.report_runner_lease(
            first.run.id().as_str(),
            Report {
                runner_id: "a",
                generation: 1,
            },
            Some(&h),
            1_000_000,
        )
        .unwrap();
        q.verify_runner_ref(first.run.id().as_str(), 1, &h, 1_000_000)
            .unwrap();
        let refs = Refs::default();
        refs.heads.borrow_mut().extend([(1, h.clone()), (2, h)]);
        refs.landing.borrow_mut().insert(1, "M".into());
        runner_recovery_pass(&mut q, &refs, 1_000_000, &mut cleanup).unwrap();
        assert_eq!(refs.heads.borrow().len(), 2);
        assert_eq!(refs.landing.borrow().len(), 1);
        assert!(refs.calls.borrow().is_empty());
        // Persist the terminal fact (not a worker's success receipt). Generation
        // 2 has no durable verification mark: it represents a crash after POST.
        if canceled {
            execute(&q, "UPDATE tasks SET status='canceled'").unwrap();
        } else {
            execute(
                &q,
                &format!(
                    "UPDATE task_runs SET status='integrated', result_commit='{}'",
                    "b".repeat(40)
                ),
            )
            .unwrap();
        }
        refs.fail
            .set(Some(if canceled { "delete_landing" } else { "delete" }));
        assert!(runner_recovery_pass(&mut q, &refs, 1_000_001, &mut cleanup).is_err());
        if canceled {
            assert_eq!(refs.landing.borrow().len(), 1);
        } else {
            assert_eq!(refs.heads.borrow().len(), 2);
        }
        refs.fail.set(None);
        runner_recovery_pass(&mut q, &refs, 1_000_002, &mut cleanup).unwrap();
        assert!(refs.heads.borrow().is_empty());
        assert!(refs.landing.borrow().is_empty());
        let calls_after_cleanup = refs.calls.borrow().len();
        runner_recovery_pass(&mut q, &refs, 1_000_003, &mut cleanup).unwrap();
        assert_eq!(refs.calls.borrow().len(), calls_after_cleanup);
        assert!(refs.heads.borrow().is_empty());
        assert_eq!(
            q.verified_ref(first.run.id().as_str())
                .unwrap()
                .unwrap()
                .generation,
            1
        );
    }
}

fn assert_ref_failure_blocks_assignment(failure: crate::infrastructure::github_refs::RefFailure) {
    let (mut q, clock) = fixture();
    let mut cleanup = RunnerRefCleanup::default();
    let Outcome::Assignment(Some(first)) = poll(&q, "a") else {
        panic!("grant")
    };
    let id = first.run.id().as_str();
    let h = "b".repeat(40);
    q.report_runner_lease(
        id,
        Report {
            runner_id: "a",
            generation: 1,
        },
        Some(&h),
        1_000_000,
    )
    .unwrap();
    q.verify_runner_ref(id, 1, &h, 1_000_000).unwrap();
    let refs = Refs::default();
    refs.heads.borrow_mut().insert(1, h);
    clock.0.store(1_090_000, Ordering::SeqCst);
    runner_recovery_pass(&mut q, &refs, 1_090_000, &mut cleanup).unwrap();
    resume(&mut q, first.run.id());
    // Reuse one recovered run: after a failed POST, the next attempt creates
    // the ref but its GET fails. Neither interruption makes it assignable.
    for verb in ["create", "read"] {
        refs.fail.set(Some(verb));
        refs.failure_kind.set(Some(failure));
        let error = runner_recovery_pass(&mut q, &refs, 1_090_001, &mut cleanup).unwrap_err();
        assert_eq!(
            error.downcast_ref::<crate::infrastructure::github_refs::RefFailure>(),
            Some(&failure)
        );
        assert!(matches!(poll(&q, "b"), Outcome::Assignment(None)));
        assert!(refs.heads.borrow().contains_key(&1));
        assert!(!refs.calls.borrow().iter().any(|c| c.starts_with("delete")));
    }
}
#[test]
fn permission_ref_failures_block_assignment_and_old_deletion() {
    assert_ref_failure_blocks_assignment(
        crate::infrastructure::github_refs::RefFailure::Permission,
    );
}
#[test]
fn temporary_ref_failures_block_assignment_and_old_deletion() {
    assert_ref_failure_blocks_assignment(crate::infrastructure::github_refs::RefFailure::Temporary);
}
