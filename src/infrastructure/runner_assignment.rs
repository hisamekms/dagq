//! SQLite composition of selection, claim, run transitions and fenced runner
//! leases. One write entry owns all of them, including authentication recheck.
//! Postgres composition is supplied separately; no local supervisor calls here.
use super::{
    runner_leases, runtime_store,
    sql_exec::{SqlRead, TransactionEntries, sql_params},
    sqlite::SqliteQueue,
    task_store,
};
use crate::{
    application::Generators,
    domain::{
        ClaimOutcome, RunId, Task, TaskRun,
        provider_switch::WorkerRoute,
        runner_assignment::{self as decision, Assignment, ClaimInputs, Operation, Outcome},
        runner_lease::{self, Grant, Verdict},
        worker::Worker,
        worker_model::WorkerTrial,
    },
};
use anyhow::{Context, Result, ensure};

impl SqliteQueue {
    /// `claim` comes from the service backend's main-head and rules ports,
    /// outside the transaction, never from runner params. Rechecking the token
    /// here closes revocation races. A failed claim/grant/report drops this one transaction without any partial write.
    pub(crate) fn runner_operation(
        &self,
        hash: &str,
        operation: &Operation,
        claim: Option<&ClaimInputs>,
        generators: &Generators,
    ) -> Result<Outcome> {
        let tx = self.write_entry("runner_assignment")?;
        let at = generators.clock.system_time();
        let now = i64::try_from(at.duration_since(std::time::UNIX_EPOCH)?.as_millis())?;
        let Some(runner) = super::runner_registration::authenticate(&tx, hash, now / 1000)? else {
            return Ok(Outcome::Unauthenticated);
        };
        let result = match operation {
            Operation::Poll(_) => {
                // Ready conditions, task holds, class/investigation holds and
                // provider routes stay in claim_task_filtered, including fallback.
                let claim = claim.context("main head and claim rules are required for a pull")?;
                // Runner leases are the remote worker slots. A result already
                // handed to the landing queue no longer occupies a worker slot.
                let running_spikes: i64 = tx.query_row(
                    "SELECT count(*) FROM runner_leases l JOIN task_runs r ON r.id=l.run_id \
                     JOIN tasks t ON t.id=r.task_id WHERE l.state='held' AND l.expires_at_ms > ?1 \
                     AND t.execution_class='spike' AND r.status IN ('claimed','starting','running','validating')",
                    sql_params![now], |r| r.get(0))?;
                let held = claim.limits.holds(usize::try_from(running_spikes)?);
                let outcome = task_store::claim_task_filtered(
                    &tx,
                    &self.runs_dir,
                    generators.ids.as_ref(),
                    at,
                    &claim.base,
                    &[], // Only a build-aware supervisor may supply an order containing wait_for_build.
                    &held,
                    None,
                    &WorkerTrial::default(),
                    &WorkerRoute::direct(&Worker::ALL),
                    |task| {
                        decision::select(
                            &runner,
                            std::slice::from_ref(task),
                            |t| t.owner().person().map(|o| o.identity()),
                            |_| true,
                        )
                        .is_some()
                    },
                )?;
                let run = match outcome {
                    ClaimOutcome::Claimed { run } => Some(*run),
                    ClaimOutcome::NoReadyTask => returned_candidate(&tx, &runner, &held, now)?,
                };
                let Some(run) = run else {
                    // No work: do not commit incidental investigation refreshes.
                    return Ok(Outcome::Assignment(None));
                };
                let Grant::Grant {
                    generation,
                    expires_at_ms,
                } = runner_leases::grant_lease(&tx, run.id().as_str(), &runner.id, now)?
                else {
                    anyhow::bail!("selected run is already leased");
                };
                let from = run.status();
                let run = decision::start(run, generation)?;
                ensure!(
                    runtime_store::save_run(&tx, &run, from, None)?,
                    "assigned run changed"
                );
                Outcome::Assignment(Some(Box::new(Assignment {
                    run,
                    generation,
                    remaining_seconds: decision::remaining_seconds(expires_at_ms, now),
                })))
            }
            Operation::Heartbeat(report) | Operation::Release(report) => {
                let current = runner_leases::lease(&tx, &report.run_id)?;
                match decision::check_report(&runner.id, current.as_ref(), report.generation, now) {
                    Ok(_) => {}
                    Err(decision::ReportRefusal::AuthorizationDenied) => {
                        return Ok(Outcome::Denied);
                    }
                    Err(decision::ReportRefusal::LeaseLost) => return Ok(Outcome::LeaseLost),
                }
                let evidence = runner_lease::Report {
                    runner_id: &runner.id,
                    generation: report.generation,
                };
                let verdict = if matches!(operation, Operation::Release(_)) {
                    runner_leases::release_lease(&tx, &report.run_id, evidence, now)?
                } else {
                    runner_leases::report_lease(&tx, &report.run_id, evidence, None, now)?
                };
                let Verdict::Accept { expires_at_ms } = verdict else {
                    return Ok(Outcome::LeaseLost);
                };
                if matches!(operation, Operation::Release(_)) {
                    Outcome::Released
                } else {
                    Outcome::Renewed {
                        remaining_seconds: decision::remaining_seconds(expires_at_ms, now),
                    }
                }
            }
            Operation::Report(report) => {
                let current = runner_leases::lease(&tx, &report.run_id)?;
                match decision::check_report(&runner.id, current.as_ref(), report.generation, now) {
                    Ok(_) => {}
                    Err(decision::ReportRefusal::AuthorizationDenied) => {
                        return Ok(Outcome::Denied);
                    }
                    Err(decision::ReportRefusal::LeaseLost) => return Ok(Outcome::LeaseLost),
                }
                let evidence = runner_lease::Report {
                    runner_id: &runner.id,
                    generation: report.generation,
                };
                if runner_lease::judge(current.as_ref(), evidence, now) == Verdict::LeaseLost {
                    return Ok(Outcome::LeaseLost);
                }
                let id = RunId::new(report.run_id.clone())?;
                let run = runtime_store::stored_run(&tx, &id)?.context("run is missing")?;
                let from = run.status();
                let (run, events) = decision::finish(run, report)?;
                ensure!(
                    runtime_store::save_run(&tx, &run, from, None)?,
                    "run changed during report"
                );
                runtime_store::record_events(&tx, &id, events)?;
                let Verdict::Accept { expires_at_ms } = runner_leases::report_lease(
                    &tx,
                    &report.run_id,
                    evidence,
                    Some(&report.head),
                    now,
                )?
                else {
                    anyhow::bail!("lease changed during result report");
                };
                Outcome::Renewed {
                    remaining_seconds: decision::remaining_seconds(expires_at_ms, now),
                }
            }
        };
        tx.commit()?;
        Ok(result)
    }
}

fn returned_candidate(
    tx: &impl super::sql_exec::SqlWrite,
    runner: &crate::domain::runner_registration::Runner,
    held: &crate::domain::claim_order::ClaimHolds,
    now: i64,
) -> Result<Option<TaskRun>> {
    let ids: Vec<String> = tx.query_map(
        "SELECT l.run_id FROM runner_leases l JOIN task_runs r ON r.id=l.run_id \
         WHERE (l.state <> 'held' OR l.expires_at_ms <= ?1) AND r.status IN ('claimed','starting','running') \
         AND NOT EXISTS (SELECT 1 FROM task_holds h WHERE h.task_id=r.task_id AND h.released_at IS NULL) \
         ORDER BY r.created_at, r.id", sql_params![now], |r| r.get(0))?;
    let investigations = if held.investigations.max.is_some() {
        // Reuse the same live investigation projection as fresh claims.
        super::investigations::refresh(tx, now / 1000)?
    } else {
        Default::default()
    };
    for id in ids {
        let run =
            runtime_store::stored_run(tx, &RunId::new(id.clone())?)?.context("run is missing")?;
        let task: Task = super::store_rows::read_task(tx, run.task_id())?;
        if !decision::eligible(runner, task.owner().person().map(|o| o.identity())) {
            continue;
        }
        if task.wait_for_build() || held.classes.contains(&task.execution_class()) {
            continue;
        }

        if investigations
            .limit(
                task.id(),
                task.execution_class(),
                task.goal_id(),
                held.investigations,
                now / 1000,
            )
            .is_some()
        {
            continue;
        }
        // A reported head must remain reachable from a verified successor ref.
        // The hand-over adapter creates that ref before a new generation grants.
        let current = runner_leases::lease(tx, &id)?.context("lease is missing")?;
        if !decision::reassignable(&current, &runner_leases::records(tx, &id)?, now) {
            continue;
        }
        return Ok(Some(run));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::super::sql_exec::SqlWrite;
    use super::*;
    use crate::{
        application::{RunnerLeaseStore, TaskStore},
        domain::{
            CommitSha, NewTask,
            runner_assignment::{ClaimLimits, LeaseReport, Poll},
            runner_lease::LeaseState,
        },
    };
    use std::sync::mpsc;
    use std::time::Duration;

    struct FixedClock;
    impl crate::application::Clock for FixedClock {
        fn system_time(&self) -> std::time::SystemTime {
            std::time::UNIX_EPOCH + Duration::from_secs(1_000_000)
        }
        fn monotonic(&self) -> std::time::Instant {
            unreachable!("assignment never uses the runner's clock or waits")
        }
    }

    fn new_task() -> NewTask {
        NewTask {
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
        }
    }

    fn setup() -> (String, SqliteQueue) {
        let uri = format!("file:/runner-assignment-{}?vfs=memdb", uuid::Uuid::new_v4());
        let mut queue = SqliteQueue::init(&uri).unwrap();
        queue.generators.clock = std::sync::Arc::new(FixedClock);
        let task = queue.add(new_task()).unwrap();
        let tx = queue.write_entry("assignment_fixture").unwrap();
        SqlWrite::execute(
            &tx,
            "UPDATE tasks SET status='ready',owner_issuer='i',owner_subject='s' WHERE id=?1",
            sql_params![task.id()],
        )
        .unwrap();
        SqlWrite::execute(&tx,"INSERT INTO members(issuer,subject,role,on_call,created_at) VALUES ('i','s','admin',0,0)", sql_params![]).unwrap();
        for id in ["a", "b"] {
            SqlWrite::execute(&tx,"INSERT INTO runners(id,owner_issuer,owner_subject,created_at,declaration) VALUES (?1,'i','s',0,?2)",
                sql_params![id, r#"{"os":"linux","arch":"aarch64","container":true,"credentials":[]}"#]).unwrap();
            SqlWrite::execute(&tx,"INSERT INTO runner_tokens(hash,runner_id,expires_at) VALUES (?1,?1,9223372036854775807)", sql_params![id]).unwrap();
        }
        tx.commit().unwrap();
        (uri, queue)
    }
    fn base() -> CommitSha {
        CommitSha::parse("a".repeat(40), "base").unwrap()
    }
    fn poll(queue: &SqliteQueue, token: &str) -> Result<Outcome> {
        poll_limited(queue, token, ClaimLimits::default())
    }
    fn poll_limited(queue: &SqliteQueue, token: &str, limits: ClaimLimits) -> Result<Outcome> {
        queue.runner_operation(
            token,
            &Operation::Poll(Poll {}),
            Some(&ClaimInputs {
                base: base(),
                limits,
            }),
            &queue.generators,
        )
    }
    fn assert_no_assignment(queue: &SqliteQueue) {
        for token in ["a", "b"] {
            assert!(
                matches!(poll(queue, token).unwrap(), Outcome::Assignment(None)),
                "{token} received held work"
            );
        }
    }

    /// Check the real pull's SQL candidate composition, rather than a predicate
    /// invented by the test. Both shared and private runners are otherwise
    /// eligible; removing the final restriction must allow the assignment.
    #[test]
    fn pull_defers_build_wait_task_holds_and_unlanded_dependencies() {
        let (_, mut queue) = setup();
        queue.conn.execute_batch("UPDATE runners SET shared=1 WHERE id='a'; UPDATE tasks SET wait_for_build=1 WHERE id=1").unwrap();
        assert_no_assignment(&queue);
        queue.conn.execute_batch("UPDATE tasks SET wait_for_build=0 WHERE id=1; INSERT INTO task_holds(task_id,reason,by_role,by_id,created_at) VALUES (1,'hold','user','user',0)").unwrap();
        assert_no_assignment(&queue);
        queue
            .conn
            .execute_batch("UPDATE task_holds SET released_at=1")
            .unwrap();
        let predecessor = queue.add(new_task()).unwrap();
        let tx = queue.write_entry("block_claim_dependency").unwrap();
        SqlWrite::execute(
            &tx,
            "INSERT INTO task_dependencies(task_id,predecessor_id) VALUES (1,?1)",
            sql_params![predecessor.id()],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_no_assignment(&queue);
        queue
            .conn
            .execute_batch("DELETE FROM task_dependencies WHERE task_id=1")
            .unwrap();
        assert!(matches!(
            poll(&queue, "a").unwrap(),
            Outcome::Assignment(Some(_))
        ));
    }

    /// Live lease counts and investigation cycles are read under the same write
    /// entry as the second claim. Neither configured limit may be discarded.
    #[test]
    fn pull_applies_spike_and_investigation_limits_to_live_queue_facts() {
        let (_, mut queue) = setup();
        let second = queue.add(new_task()).unwrap();
        let tx = queue.write_entry("ready_spikes").unwrap();
        SqlWrite::execute(&tx, "UPDATE tasks SET status='ready', execution_class='spike', owner_issuer='i', owner_subject='s'", sql_params![]).unwrap();
        tx.commit().unwrap();
        let class_limit = ClaimLimits {
            max_spikes: Some(1),
            ..Default::default()
        };
        let Outcome::Assignment(Some(first)) = poll_limited(&queue, "a", class_limit).unwrap()
        else {
            panic!("first Spike must fit")
        };
        assert_ne!(first.run.task_id(), second.id());
        for token in ["a", "b"] {
            assert!(matches!(
                poll_limited(&queue, token, class_limit).unwrap(),
                Outcome::Assignment(None)
            ));
        }
        let investigation_limit = ClaimLimits {
            max_spikes: None,
            investigations: crate::domain::investigation::InvestigationLimit {
                max: Some(1),
                grace_secs: 60,
            },
        };
        for token in ["a", "b"] {
            assert!(matches!(
                poll_limited(&queue, token, investigation_limit).unwrap(),
                Outcome::Assignment(None)
            ));
        }
        let Outcome::Assignment(Some(next)) = poll(&queue, "b").unwrap() else {
            panic!("unset limits allow the second Spike")
        };
        assert_eq!(next.run.task_id(), second.id());
    }

    #[test]
    fn concurrent_pulls_never_duplicate_and_failed_grant_rolls_claim_back() {
        let (uri, queue) = setup();
        // Abort after claim, at the lease write: both run and task roll back.
        queue.conn.execute_batch("CREATE TRIGGER fail_grant BEFORE INSERT ON runner_leases BEGIN SELECT RAISE(ABORT,'injected'); END").unwrap();
        assert!(poll(&queue, "a").is_err());
        let count: i64 = queue
            .conn
            .query_row("SELECT count(*) FROM task_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        queue.conn.execute_batch("DROP TRIGGER fail_grant").unwrap();
        let (send, receive) = mpsc::channel();
        let mut handles = Vec::new();
        for token in ["a", "b"] {
            let uri = uri.clone();
            let send = send.clone();
            handles.push(std::thread::spawn(move || {
                let mut q = SqliteQueue::open(uri).unwrap();
                q.generators.clock = std::sync::Arc::new(FixedClock);
                let result = poll(&q, token).map(|o| match o {
                    Outcome::Assignment(Some(work)) => Some(work.run.id().to_string()),
                    Outcome::Assignment(None) => None,
                    _ => panic!("unexpected answer"),
                });
                send.send(result).unwrap();
            }));
        }
        let first = receive
            .recv_timeout(Duration::from_secs(30))
            .unwrap()
            .unwrap();
        let second = receive
            .recv_timeout(Duration::from_secs(30))
            .unwrap()
            .unwrap();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            usize::from(first.is_some()) + usize::from(second.is_some()),
            1
        );
        let id = first.or(second).unwrap();
        let lease = queue.runner_lease(&id).unwrap().unwrap();
        assert_eq!(lease.generation, 1);
        // A returned assignment is available before its original deadline.
        let released = queue
            .runner_operation(
                &lease.runner_id,
                &Operation::Release(LeaseReport {
                    run_id: id.clone(),
                    generation: 1,
                }),
                None,
                &queue.generators,
            )
            .unwrap();
        assert!(matches!(released, Outcome::Released));
        assert_eq!(
            queue.runner_lease(&id).unwrap().unwrap().state,
            LeaseState::Released
        );
        let Outcome::Assignment(Some(next)) = poll(&queue, "a").unwrap() else {
            panic!("returned run must be available")
        };
        assert_eq!(next.run.id().as_str(), id);
        assert_eq!(next.generation, 2);
        let report = Operation::Report(
            serde_json::from_value(serde_json::json!({
                "run_id": id, "generation":2, "head":"b".repeat(40),
                "state":{"head":"b".repeat(40),"branch":next.run.branch()},
                "receipt":{"result":"succeeded"}, "exit_code":0,
            }))
            .unwrap(),
        );
        assert!(matches!(
            queue
                .runner_operation("a", &report, None, &queue.generators)
                .unwrap(),
            Outcome::Renewed { .. }
        ));
        let accepted = queue.accepted_head(&id).unwrap().unwrap();
        assert_eq!(accepted.head, "b".repeat(40));
        assert_eq!(accepted.generation, 2);
        let stored =
            runtime_store::stored_run(&queue.read_entry("reported_run").unwrap(), next.run.id())
                .unwrap()
                .unwrap();
        assert_eq!(stored.status(), crate::domain::RunStatus::Validating);
    }
}
