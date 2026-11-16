//! The administration and the settings (category G) on the queue service's
//! real use cases and the supervisor's real loop: an update job's install
//! and a command's `up` and `down` reach the audit as their principals'
//! records (once however often their caller sends them again), a `down`
//! takes its record before the drain may stop the service, the settings a pass observes are recorded `observed` only when
//! they change, and a change of the audit's table made outside the CLI
//! opens one attention at the next periodic check. What is recorded and
//! when is `domain::audit::administration`'s; these hold the wiring.

use serde_json::{Value, json};

use crate::application::queue_service::{Peer, QueueService};

use super::{
    agent::Script,
    harness::Wiring,
    update_job_token::{Test, control_step, job_step, send},
};
use crate::domain::{
    EventKind,
    audit::{Attestation, AuditAction, AuditRecord},
    queue_service::{API_VERSION, ControlCommand, ServiceRequest, UseCase},
};

fn records(wiring: &Wiring) -> Vec<AuditRecord> {
    super::harness::connect(&wiring.db)
        .prepare("SELECT record FROM audit_records ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
        .collect()
}

fn of(wiring: &Wiring, action: AuditAction) -> Vec<AuditRecord> {
    records(wiring)
        .into_iter()
        .filter(|r| r.action == action)
        .collect()
}

fn after(record: &AuditRecord) -> &Value {
    record.change.as_ref().unwrap().after.as_ref().unwrap()
}

#[test]
fn an_updates_install_and_up_and_down_reach_the_audit_as_their_principals() {
    let test = Test::new("audit_administration_install");
    let supervisor = test.sign_in(&test.lease);
    let job = test.issued(&supervisor, "update-1");
    let (job_pid, _) = test.wiring.processes().start();
    assert!(test.bind(&job, job_pid).ok);
    let installed = json!({
        "pid": job_pid, "version": "0.2.0",
        "digest": "d-new", "replaced_binary": {"version": "0.1.0", "digest": "d-old"}, "commit": "abc",
        "e2e": {"status": "passed"},
    });
    // The job's install steps go as the resending caller sends them, each
    // under its record: sent again (an answer lost), the event and its
    // audit are written once. One with no record goes the plain way.
    let with_record = |op: &str, args: Value, record: &str| {
        let mut params = job_step(op, args, job_pid);
        params["call"]["record"] = json!(record);
        params
    };
    for params in [
        with_record(
            "record_queue_event",
            json!(["update_installed", installed]),
            "update-installed-1",
        ),
        with_record(
            "record_queue_event",
            json!(["update_installed", installed]),
            "update-installed-1",
        ),
        job_step(
            "record_queue_event",
            json!(["update_installed", {"pid": job_pid, "version": "0.2.0", "plugin_only": true}]),
            job_pid,
        ),
    ] {
        let sent = send(&test.backend, &job, UseCase::Control, params, job_pid);
        assert!(sent.ok, "{sent:?}");
    }
    let installed_events: i64 = super::harness::connect(&test.wiring.db)
        .query_row(
            "SELECT count(*) FROM run_events WHERE kind='update_installed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        installed_events, 2,
        "the record's resend writes no second event"
    );
    let binaries = of(&test.wiring, AuditAction::BinaryInstalled);
    assert_eq!(
        binaries.len(),
        1,
        "written once with its event, and none for the plugin's update"
    );
    assert_eq!(binaries[0].sender_id, "updater:update");
    assert_eq!(binaries[0].principal.role, "updater");
    let change = binaries[0].change.as_ref().unwrap();
    assert_eq!(
        change.before,
        Some(json!({"version": "0.1.0", "digest": "d-old"}))
    );
    assert_eq!(after(&binaries[0])["version_after"], "0.2.0");
    assert_eq!(after(&binaries[0])["digest_after"], "d-new");

    // A person's `up` and `down`, each once, as the person: the service's
    // own OS user on its socket, with no token.
    let as_person = |command: ControlCommand, operation: &str| {
        let mut params = control_step(
            command,
            "audit_administration",
            json!([{"lifecycle": {"operation": operation}}]),
            test.pid,
        );
        // Sent as the command's resending caller sends it, under its
        // record's identity: sent again after a lost answer, it is one
        // record.
        params["call"]["record"] = json!(format!("audit-{operation}"));
        QueueService {
            backend: &*test.backend,
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
                use_case: UseCase::Control,
                params,
            },
            Peer::Owner,
            Some(test.pid),
        )
    };
    for (command, operation) in [
        (ControlCommand::Up, "up"),
        (ControlCommand::Up, "up"),
        (ControlCommand::Down, "down"),
    ] {
        let sent = as_person(command, operation);
        assert!(sent.ok, "{sent:?}");
    }
    // `up` sends no record of `down`.
    assert!(!as_person(ControlCommand::Up, "down").ok);
    let lifecycle: Vec<_> = of(&test.wiring, AuditAction::ServiceLifecycle)
        .into_iter()
        .filter(|r| r.sender_id == "queue_service:lifecycle")
        .collect();
    assert_eq!(
        lifecycle
            .iter()
            .map(|r| after(r)["operation"].clone())
            .collect::<Vec<_>>(),
        [json!("up"), json!("down")]
    );
    assert!(lifecycle.iter().all(|r| r.principal.role == "user"));
}

#[test]
fn the_settings_a_pass_reads_are_recorded_observed_only_when_they_change() {
    let wiring = Wiring::new(
        "audit_administration_settings",
        Script {
            receipts: Vec::new(),
            verdicts: Vec::new(),
        },
    );
    wiring.verifier.set_config("[supervisor]\nparallel = 2\n");
    wiring.supervise_once();
    let first = of(&wiring, AuditAction::ConfigChanged);
    assert_eq!(
        first.len(),
        1,
        "the observation starts with its first record"
    );
    assert_eq!(first[0].change.as_ref().unwrap().before, None);
    wiring.supervise_once();
    assert_eq!(of(&wiring, AuditAction::ConfigChanged).len(), 1);
    wiring.verifier.set_config("[supervisor]\nparallel = 3\n");
    wiring.supervise_once();
    let changed = of(&wiring, AuditAction::ConfigChanged);
    assert_eq!(changed.len(), 2);
    let record = &changed[1];
    assert_eq!(record.attestation.level, Attestation::Observed);
    assert_eq!(record.sender_id, "supervisor:settings");
    assert_eq!(after(record)["paths"], json!(["dagq.toml"]));
    let before = &record.change.as_ref().unwrap().before.as_ref().unwrap()["hashes"];
    assert_eq!(before, &after(&first[0])["hashes"]);
    assert_ne!(before, &after(record)["hashes"]);
    wiring.supervise_once();
    assert_eq!(of(&wiring, AuditAction::ConfigChanged).len(), 2);
}

#[test]
fn a_row_of_the_audit_changed_outside_the_cli_opens_one_attention_at_the_next_check() {
    let wiring = Wiring::new(
        "audit_administration_chain",
        Script {
            receipts: Vec::new(),
            verdicts: Vec::new(),
        },
    );
    wiring.supervise_once();
    let sql = super::harness::connect(&wiring.db);
    assert!(!records(&wiring).is_empty());
    // A writer that goes around the queue service and its trigger.
    sql.execute_batch("DROP TRIGGER audit_records_no_update")
        .unwrap();
    let text: String = sql
        .query_row("SELECT record FROM audit_records WHERE seq=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let mut record: AuditRecord = serde_json::from_str(&text).unwrap();
    record.principal.id = "intruder".into();
    sql.execute(
        "UPDATE audit_records SET record=?1 WHERE seq=1",
        [serde_json::to_string(&record).unwrap()],
    )
    .unwrap();
    let mismatches = || -> i64 {
        sql.query_row(
            "SELECT count(*) FROM run_events WHERE kind=?1",
            [EventKind::SupervisorAuditMismatch.as_str()],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(mismatches(), 0);
    wiring.supervise_once();
    assert_eq!(mismatches(), 1);
    let standing: Vec<_> = wiring.status()["attention"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a["kind"] == "supervisor_audit_mismatch")
        .collect();
    assert_eq!(standing.len(), 1, "{standing:?}");
    assert_eq!(standing[0]["next"], "dagq audit verify");
    // The next check finds the same record changed: no second attention.
    wiring.supervise_once();
    assert_eq!(mismatches(), 1);
}

#[test]
fn a_persons_recover_by_hand_is_one_privileged_record() {
    let wiring = Wiring::new(
        "audit_administration_recover",
        Script {
            receipts: Vec::new(),
            verdicts: Vec::new(),
        },
    );
    let recovers = || {
        of(&wiring, AuditAction::AuthzPrivileged)
            .into_iter()
            .filter(|r| r.authority.capability.as_deref() == Some("run.recover"))
            .collect::<Vec<_>>()
    };
    assert!(recovers().is_empty());
    // A person's `dagq recover` through the queue service: the grant is
    // recorded once the policy allows it, whatever the recovery then finds.
    let backend = wiring.service_backend();
    let _ = QueueService {
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
            use_case: UseCase::Recover,
            params: json!({"run": "00000000-0000-4000-8000-000000000001"}),
        },
        Peer::Owner,
        Some(std::process::id()),
    );
    let recorded = recovers();
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].principal.role, "user");
}

/// A `down` that leaves the service's stop to the supervisors' drain takes
/// its record before it asks for that stop: from then on the last
/// supervisor's drain may stop the service, which takes no record after.
/// Here the service takes none once the supervisors were told to drain,
/// and `down`'s one record is there all the same.
fn down_through_control(
    fail: bool,
    reject_retry: bool,
) -> (Wiring, anyhow::Result<Value>, Vec<String>) {
    let (wiring, result, sent, _) = down_stop_case(fail, reject_retry, None, None);
    (wiring, result, sent)
}

/// `service_only`: no supervisor registered, with the audit's insert
/// refused (`Some(false)`) or taken (`Some(true)`). `install_drain`:
/// install's `down`, which keeps the service, with the audit's insert
/// refused (`Some(true)`) or taken (`Some(false)`).
fn down_stop_case(
    fail: bool,
    reject_retry: bool,
    service_only: Option<bool>,
    install_drain: Option<bool>,
) -> (Wiring, anyhow::Result<Value>, Vec<String>, usize) {
    use crate::application::lifecycle::{self, DownOptions, LifecycleQueue, QueuePaths};
    use crate::application::queue_broker::{BrokerProbe, QueueBrokerControl};
    use crate::application::queue_service::{QueueServiceControl, Replaced, ServiceProbe};
    use crate::application::{AgentState, DefinedAgent, LaunchAgent, QueueOpener};
    use crate::domain::queue_service::ServiceState;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;
    struct Launchd(std::path::PathBuf, bool, bool, Option<bool>);
    impl LaunchAgent for Launchd {
        fn install(&self, _: &str, _: &Path, _: &str) -> anyhow::Result<()> {
            anyhow::bail!("down installs nothing")
        }
        // The bootout signals the supervisors, which drain and stop the
        // service: by then `down`'s record and its stop request are in,
        // and the service takes no record after.
        fn uninstall(&self, _: &str, _: &Path) -> anyhow::Result<AgentState> {
            if self.1 {
                anyhow::bail!("child stderr: access_token=must-not-be-audited");
            }
            if self.2 {
                return Ok(AgentState {
                    loaded: false,
                    pid: None,
                });
            }
            assert_ne!(
                self.3,
                Some(true),
                "no supervisor drains after a record not taken"
            );
            let db = super::harness::connect(&self.0);
            let count = |sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).unwrap() };
            assert_eq!(
                count(
                    "SELECT count(*) FROM audit_records WHERE sender_id='queue_service:lifecycle'"
                ),
                1
            );
            // Install's drain keeps the service: it asks for no stop.
            assert_eq!(
                count("SELECT count(*) FROM run_events WHERE kind='queue_service_stop_requested'"),
                if self.3.is_some() { 0 } else { 1 }
            );
            db.execute_batch(
                "CREATE TRIGGER service_stopped BEFORE INSERT ON audit_records
                 BEGIN SELECT RAISE(ABORT, 'the queue service stopped'); END;",
            )
            .unwrap();
            Ok(AgentState {
                loaded: false,
                pid: None,
            })
        }
        fn state(&self, _: &str) -> anyhow::Result<AgentState> {
            Ok(AgentState {
                loaded: false,
                pid: None,
            })
        }
        fn defined(&self, _: &Path) -> anyhow::Result<Vec<DefinedAgent>> {
            Ok(Vec::new())
        }
    }
    struct Service(ServiceState, Arc<std::sync::atomic::AtomicUsize>);
    impl QueueServiceControl for Service {
        fn probe(&self) -> ServiceProbe {
            ServiceProbe {
                state: self.0,
                socket: "/service.sock".into(),
                pid: Some(9),
                build: None,
                api_version: None,
                min_api_version: None,
                build_matches: None,
                started_at: None,
                error: None,
            }
        }
        fn start(&self, _: Duration) -> anyhow::Result<ServiceProbe> {
            anyhow::bail!("down starts no service")
        }
        fn stop(&self, _: Duration) -> anyhow::Result<Option<u32>> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(None)
        }
        fn replace(&self, _: bool, _: bool, _: Duration) -> anyhow::Result<Replaced> {
            anyhow::bail!("down replaces no service")
        }
    }
    struct Broker;
    impl QueueBrokerControl for Broker {
        fn probe(&self) -> BrokerProbe {
            BrokerProbe {
                state: ServiceState::Stopped,
                address: None,
                pid: None,
                build: None,
                build_matches: None,
                started_at: None,
                error: None,
            }
        }
        fn start(&self, _: Duration) -> anyhow::Result<BrokerProbe> {
            anyhow::bail!("down starts no broker")
        }
        fn stop(&self, _: Duration) -> anyhow::Result<Option<u32>> {
            Ok(None)
        }
    }
    let wiring = Wiring::new(
        "audit_administration_down_order",
        Script {
            receipts: Vec::new(),
            verdicts: Vec::new(),
        },
    );
    let token = crate::domain::LeaseToken::new("draining");
    if !fail && service_only.is_none() {
        wiring.register_at(&token, wiring.generators().clock.system_time());
    }
    /// The test's queue as the person's command opens it.
    use crate::application::supervisor_service::SupervisorQueues;
    use crate::infrastructure::queue_service::ControlCaller;
    use std::sync::Mutex;
    struct Opener(SupervisorQueues);
    impl QueueOpener<dyn LifecycleQueue + Send> for Opener {
        fn open(&self) -> anyhow::Result<Box<dyn LifecycleQueue + Send>> {
            Ok(Box::new(self.0.queue()))
        }
    }
    let backend = wiring.service_backend();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let calls = sent.clone();
    let caller = ControlCaller::new(
        control_client(),
        crate::domain::queue_service::ControlCall {
            command: ControlCommand::Down,
            task: None,
            yes: false,
            pid: std::process::id(),
        },
    )
    .unwrap()
    .exchanging_with(Arc::new(move |request| {
        // The committed fact survives even when the next service refuses
        // its retry: down must return that error without another fact.
        if reject_retry
            && request.params["call"]["op"] == "audit_administration"
            && calls.lock().unwrap().len() == 1
        {
            calls.lock().unwrap().push(
                request.params["call"]["record"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
            return Ok(crate::domain::queue_service::ServiceResponse::failure(
                crate::domain::queue_service::ServiceErrorCode::Failed,
                "the retry was refused after the first commit",
            ));
        }
        let answer = QueueService {
            backend: &*backend,
            build: "wiring",
            pid: 1,
        }
        .handle_from_process(request, Peer::Owner, Some(std::process::id()));
        if request.params["call"]["op"] == "audit_administration" {
            let mut calls = calls.lock().unwrap();
            calls.push(
                request.params["call"]["record"]
                    .as_str()
                    .expect("the real caller assigns an identity")
                    .to_owned(),
            );
            assert!(
                calls.len() <= 2,
                "the answer must settle on the second send"
            );
            if calls.len() == 1 && answer.ok {
                // The service committed; only its answer was lost.
                anyhow::bail!("lost after commit");
            }
        }
        Ok(answer)
    }));
    let caller = crate::compose::wiring_lifecycle_caller(Arc::new(caller), wiring.generators())
        .pausing_with(Arc::new(|_| {}));
    let queues = crate::compose::wiring_lifecycle_queues(Arc::new(caller)).unwrap();
    if service_only == Some(false) || install_drain == Some(true) {
        super::harness::connect(&wiring.db).execute_batch(
            "CREATE TRIGGER refuse_admin_audit BEFORE INSERT ON audit_records BEGIN SELECT RAISE(ABORT, 'audit INSERT failed'); END;"
        ).unwrap();
    }
    let opener = |_: &Path| -> Arc<dyn QueueOpener<dyn LifecycleQueue + Send>> {
        Arc::new(Opener(queues.clone()))
    };
    let launchd = Launchd(
        wiring.db.clone(),
        fail,
        service_only.is_some(),
        install_drain,
    );
    let stopped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let files = crate::application::memory_files::MemoryFiles::default();
    let clock = wiring.generators().clock;
    let stop_counter = stopped.clone();
    let ports = lifecycle::Ports {
        launchd: &launchd,
        processes: wiring.processes(),
        files: &files,
        clock: &*clock,
        queues: &opener,
        inspect_repository: &|_| anyhow::bail!("down inspects no repository"),
        trusts_repository: &|_, _| anyhow::bail!("down trusts no repository"),
        run_env_programs: &|_, _, _| anyhow::bail!("down checks no program"),
        ci_watch_preflight: &|_, _| Ok(None),
        resolve_language: &|_, _| Ok(None),
        queue_service: &move |_, _| Box::new(Service(ServiceState::Running, stop_counter.clone())),
        queue_broker: &|_, _| Box::new(Broker),
        revoke_ended: &|_, _| Ok(false),
    };
    let paths = QueuePaths {
        db: wiring.db.clone(),
        hash: "hash".into(),
        label: "dagq.test".into(),
        launch_agent: "/agent.plist".into(),
        log_dir: "/logs".into(),
    };
    let report = lifecycle::down(
        &ports,
        &paths,
        &DownOptions {
            keep_service: install_drain.is_some(),
            wait: false,
            force: false,
            poll: Duration::from_millis(1),
        },
    );
    let identities = sent.lock().unwrap().clone();
    (
        wiring,
        report,
        identities,
        stopped.load(std::sync::atomic::Ordering::SeqCst),
    )
}

#[test]
fn down_takes_its_record_before_the_drain_may_stop_the_service() {
    let (wiring, report, sent) = down_through_control(false, false);
    let report = report.unwrap();
    assert_eq!(report["outcome"], "draining", "{report}");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);
    let downs: Vec<AuditRecord> = of(&wiring, AuditAction::ServiceLifecycle)
        .into_iter()
        .filter(|r| r.sender_id == "queue_service:lifecycle")
        .collect();
    assert_eq!(downs.len(), 1, "{downs:?}");
    assert_eq!(after(&downs[0])["operation"], "down");
    assert_eq!(downs[0].principal.role, "user");
}

/// A failing down sends a fixed reason code even when its child error
/// contains a credential. This uses ControlCaller and its actual conversion.
#[test]
fn a_failed_down_audits_a_fixed_reason_without_its_childs_secret() {
    let (wiring, report, sent) = down_through_control(true, false);
    assert!(format!("{:#}", report.unwrap_err()).contains("must-not-be-audited"));
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);
    let downs: Vec<_> = of(&wiring, AuditAction::ServiceLifecycle)
        .into_iter()
        .filter(|r| r.sender_id == "queue_service:lifecycle")
        .collect();
    assert_eq!(downs.len(), 1);
    assert_eq!(
        downs[0].outcome.status,
        crate::domain::audit::OutcomeStatus::Failed
    );
    assert_eq!(downs[0].outcome.reason.as_deref(), Some("operation_failed"));
    assert!(
        !serde_json::to_string(&records(&wiring))
            .unwrap()
            .contains("must-not-be-audited")
    );
}

#[test]
fn a_down_whose_committed_audit_retry_fails_creates_no_second_fact() {
    let (wiring, result, sent) = down_through_control(false, true);
    assert!(format!("{:#}", result.unwrap_err()).contains("the retry was refused"));
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);
    let downs: Vec<_> = of(&wiring, AuditAction::ServiceLifecycle)
        .into_iter()
        .filter(|r| r.sender_id == "queue_service:lifecycle")
        .collect();
    assert_eq!(downs.len(), 1);
    assert_eq!(
        downs[0].outcome.status,
        crate::domain::audit::OutcomeStatus::Succeeded
    );
}

/// A socket client that names no token, role, queue or binding, as the
/// one the tests' exchange answers in place of the service.
fn control_client() -> crate::infrastructure::queue_service::Client {
    crate::infrastructure::queue_service::Client::broker(std::path::Path::new("/wiring"))
}

#[test]
fn up_audit_uses_the_real_lifecycle_caller_and_resends_one_identity_after_commit() {
    use crate::application::EventStore;
    use crate::domain::audit::administration::{AdministrationFact, LifecycleOperation};
    use crate::infrastructure::queue_service::ControlCaller;
    use std::sync::{Arc, Mutex};
    let wiring = Wiring::new(
        "audit_up_caller",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let backend = wiring.service_backend();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let requests = sent.clone();
    let caller = ControlCaller::new(
        control_client(),
        crate::domain::queue_service::ControlCall {
            command: ControlCommand::Up,
            task: None,
            yes: false,
            pid: std::process::id(),
        },
    )
    .unwrap()
    .exchanging_with(Arc::new(move |request| {
        let response = QueueService {
            backend: &*backend,
            build: "wiring",
            pid: 1,
        }
        .handle_from_process(request, Peer::Owner, Some(std::process::id()));
        if request.params["call"]["op"] == "audit_administration" {
            let mut sent = requests.lock().unwrap();
            sent.push(request.params["call"]["record"].clone());
            assert!(sent.len() <= 2);
            if sent.len() == 1 && response.ok {
                anyhow::bail!("lost after commit");
            }
        }
        Ok(response)
    }));
    let caller = crate::compose::wiring_lifecycle_caller(Arc::new(caller), wiring.generators())
        .pausing_with(Arc::new(|_| {}));
    let queue = crate::compose::wiring_lifecycle_queues(Arc::new(caller))
        .unwrap()
        .queue();
    queue
        .audit_administration(&AdministrationFact::Lifecycle {
            operation: LifecycleOperation::Up,
            error: None,
        })
        .unwrap();
    let sent = sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert!(sent[0].is_string());
    assert_eq!(sent[0], sent[1]);
    let up: Vec<_> = of(&wiring, AuditAction::ServiceLifecycle)
        .into_iter()
        .filter(|r| r.sender_id == "queue_service:lifecycle")
        .collect();
    assert_eq!(up.len(), 1);
    assert_eq!(after(&up[0])["operation"], "up");
}

#[test]
fn rollback_to_a_service_without_audit_steps_keeps_a_successful_manual_install() {
    use crate::application::install::{
        self,
        test_support::{At, InstallSteps, OnlyOne},
    };
    use crate::application::supervisor_service::{ServiceCaller, SupervisorQueues};
    use crate::application::update::{self, JobPorts};
    use crate::application::{QueueOpener, RunFiles, memory_files::MemoryFiles};
    use crate::infrastructure::queue_service::ControlCaller;
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::SeqCst},
        },
        time::Duration,
    };
    struct Opener(SupervisorQueues);
    impl QueueOpener for Opener {
        fn open(&self) -> anyhow::Result<Box<dyn crate::application::Queue + Send>> {
            Ok(Box::new(self.0.queue()))
        }
    }
    let wiring = Wiring::new(
        "audit_rollback_compatibility",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let steps = Arc::new(InstallSteps {
        in_place: true,
        heartbeats: true,
        ..Default::default()
    });
    let old = steps.clone();
    let audits = Arc::new(AtomicUsize::new(0));
    let attempted = audits.clone();
    let caller = ControlCaller::new(
        control_client(),
        crate::domain::queue_service::ControlCall {
            command: ControlCommand::Install,
            task: None,
            yes: false,
            pid: std::process::id(),
        },
    )
    .unwrap()
    .exchanging_with(Arc::new(move |request| {
        use crate::domain::queue_service::{ServiceErrorCode, ServiceResponse};
        let params = request.params["call"].clone();
        if params["op"] == crate::application::supervisor_service::control::CONTROL_QUEUE {
            return Ok(ServiceResponse::success(
                json!({"role": "user", "actor_id": "person"}),
            ));
        }
        if params["op"] == "audit_administration" {
            attempted.fetch_add(1, SeqCst);
            return Ok(ServiceResponse::failure(
                ServiceErrorCode::BadRequest,
                "audit_administration is not an operation of this build",
            ));
        }
        old.call(UseCase::Supervisor, params)
            .map(ServiceResponse::success)
    }));
    let caller = crate::application::supervisor_service::control::resending(
        Arc::new(caller),
        wiring.generators(),
    )
    .pausing_with(Arc::new(|_| {}));
    let queues = crate::compose::wiring_lifecycle_queues(Arc::new(caller)).unwrap();
    let opener = |_: &Path| -> Arc<dyn QueueOpener> { Arc::new(Opener(queues.clone())) };
    let files = MemoryFiles::default();
    files.write(Path::new("/queue"), b"queue").unwrap();
    files.write(Path::new("/to"), b"new").unwrap();
    files.write(Path::new("/to.previous"), b"old").unwrap();
    let report = update::install_watched(
        &JobPorts {
            binaries: &*steps,
            files: &files,
            processes: &OnlyOne,
            clock: &At::new(10_000),
            queues: &opener,
            restart: &|_| anyhow::bail!("no restart"),
            is_ancestor: &|_, _| anyhow::bail!("no checkout"),
        },
        &|| anyhow::bail!("no drain"),
        None,
        Some(Path::new("/queue")),
        &install::InstallOptions {
            source: install::Source::Rollback,
            target: "/to".into(),
            allow_breaking: false,
            restart: vec![],
            handoff_timeout: Duration::ZERO,
            poll: Duration::ZERO,
            e2e: install::E2eGate::NotApplicable,
        },
        Duration::from_secs(30),
    )
    .unwrap();
    assert_eq!(report["outcome"], "installed");
    assert_eq!(report["watch"][0]["heartbeat"], true);
    assert_eq!(audits.load(SeqCst), 1);
}

/// A row whose metadata a writer around the CLI made unreadable (here a
/// negative sender counter; the other unreadable forms are the adapter's
/// `verify` cases) reaches the inbox as the one attention of the periodic
/// check, as a changed row does.
#[test]
fn malformed_audit_metadata_opens_attention_through_the_periodic_verifier() {
    let wiring = Wiring::new(
        "audit_negative_counter",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    wiring.supervise_once();
    let sql = super::harness::connect(&wiring.db);
    sql.execute_batch(
        "DROP TRIGGER audit_records_no_update; UPDATE audit_records SET sender_seq=-1 WHERE seq=(SELECT max(seq) FROM audit_records);",
    )
    .unwrap();
    for _ in 0..2 {
        wiring.supervise_once();
    }
    let count: i64 = sql
        .query_row(
            "SELECT count(*) FROM run_events WHERE kind='supervisor_audit_mismatch'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let status = wiring.status();
    let attention: Vec<_> = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "supervisor_audit_mismatch")
        .collect();
    assert_eq!(attention.len(), 1);
    assert_eq!(attention[0]["next"], "dagq audit verify");
}

/// The actual loop reaches another service step while verification is
/// blocked on its own thread. One deadline and all joins are the wiring's.
#[test]
fn a_blocked_audit_verification_does_not_hold_the_supervisor_pass() {
    use crate::application::supervisor_service::ServiceCaller;
    use std::{
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering::SeqCst},
            mpsc,
        },
    };
    struct Probe {
        inner: Arc<dyn ServiceCaller>,
        loop_thread: std::thread::ThreadId,
        seen_settings: AtomicBool,
        armed: AtomicBool,
        entered: mpsc::Sender<()>,
        began: Mutex<mpsc::Receiver<()>>,
        release: mpsc::Sender<()>,
        released: Mutex<mpsc::Receiver<()>>,
        progressed: Arc<AtomicBool>,
    }
    impl ServiceCaller for Probe {
        fn call(&self, case: UseCase, params: Value) -> anyhow::Result<Value> {
            let op = params["op"].as_str().unwrap_or_default().to_owned();
            let on_loop = std::thread::current().id() == self.loop_thread;
            if op == "verify_audit_chain" {
                assert!(!on_loop, "verification must be off the pass");
                self.entered.send(()).unwrap();
                self.released
                    .lock()
                    .unwrap()
                    .recv_timeout(super::harness::LIMIT)
                    .unwrap();
            } else if on_loop && self.seen_settings.swap(false, SeqCst) {
                self.began
                    .lock()
                    .unwrap()
                    .recv_timeout(super::harness::LIMIT)
                    .unwrap();
                self.progressed.store(true, SeqCst);
                self.release.send(()).unwrap();
            }
            let result = self.inner.call(case, params);
            if op == "observe_settings" && on_loop && self.armed.swap(false, SeqCst) {
                self.seen_settings.store(true, SeqCst);
            }
            result
        }
        fn with_token(&self, p: &Path) -> anyhow::Result<Arc<dyn ServiceCaller>> {
            self.inner.with_token(p)
        }
        fn sign_in(&self, t: &crate::domain::LeaseToken, p: u32) -> anyhow::Result<()> {
            self.inner.sign_in(t, p)
        }
        fn take_back(&self, t: &crate::domain::LeaseToken, p: u32) -> anyhow::Result<()> {
            self.inner.take_back(t, p)
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
    let progressed = Arc::new(AtomicBool::new(false));
    let progress = progressed.clone();
    let wiring = Wiring::new(
        "audit_off_pass",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_caller_wrap(Arc::new(move |inner| {
        let (entered, began) = mpsc::channel();
        let (release, released) = mpsc::channel();
        Arc::new(Probe {
            inner,
            loop_thread: std::thread::current().id(),
            seen_settings: AtomicBool::new(false),
            armed: AtomicBool::new(true),
            entered,
            began: Mutex::new(began),
            release,
            released: Mutex::new(released),
            progressed: progress.clone(),
        })
    }));
    wiring.supervise_once();
    assert!(progressed.load(SeqCst));
}

/// Install's drain keeps the service and takes `down`'s one record before
/// the supervisors are told to drain: sent again after a lost answer, it
/// is one record, and none is sent after.
#[test]
fn an_installs_drain_takes_its_one_record_before_the_supervisors_drain() {
    let (wiring, result, sent, stops) = down_stop_case(false, false, None, Some(false));
    let report = result.unwrap();
    assert_eq!(report["outcome"], "draining", "{report}");
    assert_eq!(report["queue_service"]["outcome"], "kept", "{report}");
    assert_eq!(stops, 0);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);
    let downs: Vec<_> = of(&wiring, AuditAction::ServiceLifecycle)
        .into_iter()
        .filter(|r| r.sender_id == "queue_service:lifecycle")
        .collect();
    assert_eq!(downs.len(), 1, "{downs:?}");
    assert_eq!(after(&downs[0])["operation"], "down");
    assert_eq!(
        downs[0].outcome.status,
        crate::domain::audit::OutcomeStatus::Succeeded
    );
}

/// An install's drain whose record is not taken fails before any
/// supervisor is told to drain, so the install fails with every
/// supervisor still at work.
#[test]
fn an_installs_drain_whose_record_is_not_taken_drains_no_supervisor() {
    let (wiring, result, sent, stops) = down_stop_case(false, false, None, Some(true));
    assert!(format!("{:#}", result.unwrap_err()).contains("audit INSERT failed"));
    assert_eq!(sent.len(), 1);
    assert_eq!(stops, 0);
    assert!(
        !of(&wiring, AuditAction::ServiceLifecycle)
            .iter()
            .any(|r| r.sender_id == "queue_service:lifecycle")
    );
}

#[test]
fn a_service_only_down_returns_audit_insert_failure_and_does_not_stop() {
    let (wiring, result, sent, stops) = down_stop_case(false, false, Some(false), None);
    assert!(format!("{:#}", result.unwrap_err()).contains("audit INSERT failed"));
    assert_eq!(sent.len(), 1);
    assert_eq!(stops, 0);
    assert!(
        !of(&wiring, AuditAction::ServiceLifecycle)
            .iter()
            .any(|r| r.sender_id == "queue_service:lifecycle")
    );
}
#[test]
fn a_service_only_down_retries_its_committed_audit_before_stopping() {
    let (wiring, result, sent, stops) = down_stop_case(false, false, Some(true), None);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stops, 1);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);
    assert_eq!(
        of(&wiring, AuditAction::ServiceLifecycle)
            .iter()
            .filter(|r| r.sender_id == "queue_service:lifecycle")
            .count(),
        1
    );
}
