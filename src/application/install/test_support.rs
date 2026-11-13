//! Memory ports shared by install and automatic-update unit tests.

use super::*;
use crate::domain::{LeaseToken, SupervisorMode};
use std::time::{SystemTime, UNIX_EPOCH};

/// The clock at `secs` after the epoch.
pub(crate) struct At(pub(crate) i64);

impl Clock for At {
    fn system_time(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.0.unsigned_abs())
    }

    fn monotonic(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

/// Pid 1 is alive, every other pid is dead.
pub(crate) struct OnlyOne;

impl ProcessControl for OnlyOne {
    fn alive(&self, pid: u32) -> bool {
        pid == 1
    }
    fn terminate(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn interrupt(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn kill(&self, _: u32) -> Result<()> {
        Ok(())
    }
}

pub(crate) fn registration(token: &str, pid: u32, heartbeat_at: i64) -> SupervisorRegistration {
    SupervisorRegistration {
        token: LeaseToken::new(token),
        pid,
        parallel: 2,
        started_at: 0,
        heartbeat_at,
        mode: Some(SupervisorMode::Launchd),
        workspace_id: None,
        handoff_accepted: true,
        handoff_binary: None,
        auto_update: false,
        max_waiting: None,
        parallel_source: None,
        max_waiting_source: None,
        runtime_planners: None,
        runtime_planners_source: None,
        claim_spacing: None,
        claim_spacing_source: None,
        max_spikes: None,
        max_spikes_source: None,
        class_aging: None,
        class_aging_source: None,
        max_load: None,
        max_load_source: None,
        providers: None,
        binary_version: Some("1.0.0".into()),
    }
}

// These fakes exercise install itself, including open's fallback;
// the literal call sequences do not share the install's ordering logic.
#[derive(Clone, Copy, Debug)]
pub(crate) enum OpenFailure {
    Unknown,
    Unreachable,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Handoff {
    Taken,
    Failed,
    Partial,
    Unsettled,
}

pub(crate) struct InstallSteps {
    pub(crate) calls: Arc<std::sync::Mutex<Vec<String>>>,
    pub(crate) registrations: std::sync::Mutex<Vec<SupervisorRegistration>>,
    pub(crate) first_open: std::sync::Mutex<Option<OpenFailure>>,
    pub(crate) schema: Option<SchemaCheck>,
    pub(crate) handoff: Handoff,
    pub(crate) restart_failures: std::sync::atomic::AtomicUsize,
    pub(crate) in_place: bool,
    pub(crate) unsupported_job: Option<PathBuf>,
    pub(crate) previous_version: Option<String>,
    pub(crate) job_checks: std::sync::Mutex<Vec<PathBuf>>,
    pub(crate) survives_failure: bool,
    pub(crate) service_running: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) hold_active: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) binary_restored: std::sync::atomic::AtomicBool,
    pub(crate) restored_start_fails: bool,
    pub(crate) watch_after_reads: std::sync::Mutex<Option<usize>>,
    pub(crate) events: std::sync::Mutex<Vec<(String, Value)>>,
    pub(crate) asks: std::sync::Mutex<Vec<crate::domain::Ask>>,
}

impl Default for InstallSteps {
    fn default() -> Self {
        Self {
            calls: Default::default(),
            registrations: std::sync::Mutex::new(vec![registration("first", 1, 10_000)]),
            first_open: std::sync::Mutex::new(None),
            schema: Some(SchemaCheck {
                pending: vec![PendingMigration {
                    version: 1,
                    compatible: true,
                }],
                opens: true,
            }),
            handoff: Handoff::Taken,
            restart_failures: std::sync::atomic::AtomicUsize::new(0),
            in_place: false,
            unsupported_job: None,
            previous_version: None,
            job_checks: Default::default(),
            survives_failure: false,
            service_running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            hold_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            binary_restored: std::sync::atomic::AtomicBool::new(false),
            restored_start_fails: false,
            watch_after_reads: std::sync::Mutex::new(None),
            events: Default::default(),
            asks: Default::default(),
        }
    }
}

/// Releasing a hold changes only the hold's state. A periodic watch does
/// not start a service at drop: its later pass is scripted separately.
struct HoldReleased(Arc<std::sync::atomic::AtomicBool>);

impl Drop for HoldReleased {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl InstallSteps {
    pub(crate) fn note(&self, step: impl Into<String>) {
        self.calls.lock().unwrap().push(step.into());
    }
}

impl Binaries for InstallSteps {
    fn build(&self, _: &Path) -> Result<PathBuf> {
        bail!("the binary is already built")
    }
    fn version(&self, binary: &Path) -> Result<String> {
        if binary == Path::new("/to.previous")
            && let Some(version) = &self.previous_version
        {
            return Ok(version.clone());
        }
        Ok(if binary == Path::new("/to") && !self.in_place {
            "old"
        } else {
            "new"
        }
        .into())
    }
    fn probe(&self, _: &Path) -> Result<()> {
        Ok(())
    }
    fn takes_handoff(&self, _: &Path) -> bool {
        true
    }
    fn accepts_update_job(&self, binary: &Path) -> bool {
        self.job_checks.lock().unwrap().push(binary.to_owned());
        self.unsupported_job.as_deref() != Some(binary)
    }
    fn schema(&self, _: &Path, _: &Path) -> Result<Option<SchemaCheck>> {
        Ok(self.schema.clone())
    }
    fn serve(&self, binary: &Path, _: &Path, breaking: bool) -> Result<Value> {
        assert!(!breaking);
        use std::sync::atomic::Ordering::SeqCst;
        if binary == Path::new("/to") && self.binary_restored.load(SeqCst) {
            assert!(!self.hold_active.load(SeqCst));
            self.note("serve_restored /to");
            if self.restored_start_fails {
                self.service_running.store(false, SeqCst);
                bail!("restored start failed")
            }
            self.service_running.store(true, SeqCst);
            return Ok(Value::Null);
        }
        self.note(format!("serve {}", binary.display()));
        if binary == Path::new("/to")
            && self
                .restart_failures
                .try_update(SeqCst, SeqCst, |left| left.checked_sub(1))
                .is_ok()
        {
            self.service_running.store(self.survives_failure, SeqCst);
            bail!("restart failed")
        }
        self.service_running.store(true, SeqCst);
        Ok(json!({"applied": 1}))
    }
    fn hold_service(&self, _: &Path) -> Result<crate::application::queue_service::ServiceHold> {
        self.note("hold");
        self.hold_active
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::application::queue_service::ServiceHold::new(
            Box::new(HoldReleased(self.hold_active.clone())),
        ))
    }
    fn service_runs(&self, _: &Path) -> bool {
        self.service_running
            .load(std::sync::atomic::Ordering::SeqCst)
    }
    fn replace(&self, _: &Path, _: &Path) -> Result<()> {
        self.note("replace");
        Ok(())
    }
    fn adopt(&self, _: &Path, _: &Path) -> Result<()> {
        self.note("adopt");
        Ok(())
    }
    fn restore(&self, _: &Path) -> Result<()> {
        self.note("restore");
        self.binary_restored
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn run(&self, _: &Path, _: &[String]) -> Result<Value> {
        bail!("the compatible install does not drain")
    }
}

impl crate::application::supervisor_service::ServiceCaller for InstallSteps {
    fn call(&self, _: crate::domain::queue_service::UseCase, params: Value) -> Result<Value> {
        let op = params["op"].as_str().unwrap();
        if op == "request_handoff" {
            self.note("hand_off");
        }
        use std::sync::atomic::Ordering::SeqCst;
        if op == "supervisors"
            && self.binary_restored.load(SeqCst)
            && !self.hold_active.load(SeqCst)
            && !self.service_runs(Path::new("/queue"))
        {
            let mut watch = self.watch_after_reads.lock().unwrap();
            if let Some(left) = watch.as_mut() {
                if *left == 0 {
                    self.service_running.store(true, SeqCst);
                    *watch = None;
                    self.note("watch_service");
                } else {
                    *left -= 1;
                }
            }
        }
        if !self.service_runs(Path::new("/queue")) {
            if op == "supervisors" && self.binary_restored.load(SeqCst) {
                self.note("read_unreachable");
                // A regression that omits the deadline must fail the test,
                // rather than spin forever with this deterministic clock.
                assert!(
                    self.calls
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|call| call.as_str() == "read_unreachable")
                        .count()
                        <= 8
                );
            }
            return Err(
                crate::application::supervisor_service::ServiceUnreachable::not_done("no service")
                    .into(),
            );
        }
        match op {
            "supervisors" => Ok(serde_json::to_value(&*self.registrations.lock().unwrap())?),
            "latest_events_of" => Ok(json!([])),
            "request_handoff" => {
                let token = params["args"][0].as_str().unwrap();
                match self.handoff {
                    Handoff::Unsettled => Err(anyhow::anyhow!("handoff answer lost")
                        .context(crate::application::lifecycle::HandoffUnsettled)),
                    Handoff::Failed => Ok(json!(false)),
                    Handoff::Partial if token == "second" => Ok(json!(false)),
                    _ => {
                        let mut registrations = self.registrations.lock().unwrap();
                        let registration = registrations
                            .iter_mut()
                            .find(|r| r.token.as_str() == token)
                            .unwrap();
                        registration.binary_version = Some("new".into());
                        Ok(json!(true))
                    }
                }
            }
            "cancel_handoff" => Ok(json!(true)),
            "open_update_ask" => {
                let mut asks = self.asks.lock().unwrap();
                let ask: crate::domain::Ask = serde_json::from_value(json!({
                    "id": asks.len() + 1,
                    "kind": params["args"][0],
                    "question": params["args"][1],
                    "options": params["args"][2],
                    "asked_by": params["args"][3],
                    "subject": params["args"][4],
                    "reason_category": "recovery_failed",
                    "created_at": 10_000,
                }))?;
                self.note(format!("ask {}", ask.kind));
                let value = serde_json::to_value(&ask)?;
                asks.push(ask);
                Ok(value)
            }
            "record_queue_event" => {
                let kind = params["args"][0].as_str().unwrap().to_owned();
                self.note(format!("record {kind}"));
                let mut events = self.events.lock().unwrap();
                events.push((kind, params["args"][1].clone()));
                Ok(json!(events.len()))
            }
            other => bail!("unexpected queue operation {other}"),
        }
    }
    fn with_token(
        &self,
        _: &Path,
    ) -> Result<Arc<dyn crate::application::supervisor_service::ServiceCaller>> {
        bail!("no landing in this install")
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn hand_over(&self) {}
    fn sign_out(&self) {}
    fn credential(&self) -> Option<PathBuf> {
        None
    }
}

pub(crate) struct InstallQueue(pub(crate) Arc<InstallSteps>);

impl QueueOpener for InstallQueue {
    fn open(&self) -> Result<Box<dyn crate::application::Queue + Send>> {
        use crate::application::supervisor_service::{
            CommandUnknown, ServiceUnreachable, SupervisorQueues,
        };
        self.0.note("open");
        match self.0.first_open.lock().unwrap().take() {
            Some(OpenFailure::Unknown) => {
                return Err(CommandUnknown("old service".into()).into());
            }
            Some(OpenFailure::Unreachable) => {
                return Err(ServiceUnreachable::not_done("no socket").into());
            }
            None => {}
        }
        ensure!(
            self.0.service_runs(Path::new("/queue")),
            ServiceUnreachable::not_done("no service")
        );
        Ok(Box::new(
            SupervisorQueues::new(self.0.clone(), crate::domain::ActorContext::user()).queue(),
        ))
    }
}
