//! A supervisor of a test's queue run in its own thread, as the queue
//! service's and the queue broker's tests keep one: started with options,
//! waited for some passes, and drained as `down` drains it; and a copy of
//! the service's token files as an executor's turn finds them.

use crate::common;
use crate::common::cli::ok;
use crate::common::shell_path;
use dagq::infrastructure::{queue_service, sqlite::SqliteQueue};
use std::path::{Path, PathBuf};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// A supervisor of a queue, in its own thread until `stop`: with the
/// fixture's ready task cancelled, it claims nothing and only keeps the
/// service, every pass (`interval` zero).
pub struct Running {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    passes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    pub fn start(
        db: &Path,
        repo: &Path,
        executable: &Path,
        control: Option<std::sync::Arc<dyn dagq::application::queue_service::QueueServiceControl>>,
    ) -> Self {
        Self::with_options(
            db,
            repo,
            dagq::compose::SuperviseOptions {
                queue_service: Some(dagq::compose::QueueServiceOptions {
                    executable: executable.to_path_buf(),
                    interval: Duration::ZERO,
                    start_timeout: TIMEOUT,
                    control: control.map(dagq::compose::QueueServiceControlPort),
                }),
                ..crate::runtime_support::supervise_options(1, false)
            },
        )
    }

    /// A supervisor of the queue at `db` run with `options` (not `--once`).
    pub fn with_options(db: &Path, repo: &Path, options: dagq::compose::SuperviseOptions) -> Self {
        use crate::runtime_support::{TestWorkspace, VALID_AGENT};
        let (stop, passes) = (options.stop.clone(), options.passes.clone());
        let (db, repo) = (db.to_path_buf(), repo.to_path_buf());
        let thread = std::thread::spawn(move || {
            let backend = TestWorkspace::new(&db, false, VALID_AGENT);
            crate::runtime_support::supervise_with(&db, &repo, &backend, &options).unwrap();
        });
        Self {
            stop,
            passes,
            thread: Some(thread),
        }
    }

    /// Wait until `passes` more passes ran.
    pub fn passes(&self, passes: u64) {
        use std::sync::atomic::Ordering;
        let until = self.passes.load(Ordering::SeqCst) + passes;
        let _waiting = common::within(TIMEOUT * 3, format!("{passes} passes"));
        while self.passes.load(Ordering::SeqCst) < until {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Ask it to drain, as `down` does, and wait for it to end.
    pub fn drain(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _waiting = common::within(TIMEOUT * 3, "the supervisor to drain");
            thread.join().unwrap();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The fixture's queue with its ready task cancelled: a supervisor of it
/// claims nothing.
pub fn idle_queue() -> (crate::runtime_support::Fixture, PathBuf, PathBuf) {
    let (fixture, repo, db) = crate::runtime_support::fixture();
    for task in ok(&db, &["list"])["tasks"].as_array().unwrap() {
        ok(
            &db,
            &["cancel", &task["id"].to_string(), "--reason", "stale"],
        );
    }
    (fixture, repo, db)
}

pub fn supervisor_token(db: &Path) -> String {
    let _waiting = common::within(TIMEOUT * 3, "the supervisor to register");
    loop {
        if let Some(registration) = SqliteQueue::open(db)
            .unwrap()
            .supervisors()
            .unwrap()
            .first()
        {
            return registration.token.to_string();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Shell lines that copy the queue's token records and token files to
/// `<snapshots>/<name>/service/`, as the control side holds them while the
/// executor runs, to a place of the test's outside the run. A temporary
/// file that goes while it is copied is no matter: the test reads the
/// token files and the records it needs.
pub fn token_snapshot(queue_dir: &Path, snapshots: &Path, name: &str) -> String {
    let service = queue_service::service_dir(queue_dir);
    let to = snapshots.join(name).join("service");
    format!(
        "mkdir -p {to} || exit 75; cp -R {tokens} {credentials} {to}/ 2>/dev/null; ",
        to = shell_path(&to),
        tokens = shell_path(service.join(queue_service::TOKENS_DIR)),
        credentials = shell_path(service.join(queue_service::CREDENTIALS_DIR)),
    )
}

/// A supervisor of the queue at `db` signed in to its running service,
/// and the queue a job command it started as its own reaches through the
/// service with its token ([`queue_service::SocketCaller::adopt`]): the
/// supervisor signs out when dropped.
pub struct JobOfSupervisor {
    pub queues: dagq::application::supervisor_service::SupervisorQueues,
    supervisor: queue_service::SocketCaller,
}

impl JobOfSupervisor {
    pub fn sign_in(db: &Path) -> Self {
        use dagq::application::supervisor_service::ServiceCaller;
        let pid = std::process::id();
        let registration = dagq::domain::LeaseToken::new("supervisor-of-the-job");
        let supervisor = queue_service::SocketCaller::new(db);
        supervisor.sign_in(&registration, pid).unwrap();
        let adopted = queue_service::SocketCaller::adopt(db, &registration, pid).unwrap();
        Self {
            queues: dagq::application::supervisor_service::SupervisorQueues::new(
                std::sync::Arc::new(adopted),
                dagq::domain::ActorContext::instance(dagq::domain::ActorRole::Supervisor, pid),
            ),
            supervisor,
        }
    }
}

impl Drop for JobOfSupervisor {
    fn drop(&mut self) {
        use dagq::application::supervisor_service::ServiceCaller;
        self.supervisor.sign_out();
    }
}

/// The supervisor's way to its service as the route gives it, but for the
/// operation `op` (ADR-t3102-3 decision 5): its first request finds the
/// service out of reach and is not sent, and its second is done and its
/// answer lost on the way back; the later ones go through.
pub struct DropsThenLoses {
    inner: std::sync::Arc<dyn dagq::application::supervisor_service::ServiceCaller>,
    op: &'static str,
    sent: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl DropsThenLoses {
    /// The layer of the supervisor's in-process route in this double for
    /// `op`, and how many requests of `op` it was given.
    pub fn wrap(
        op: &'static str,
    ) -> (
        dagq::compose::CallerLayer,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = sent.clone();
        let wrap = dagq::compose::CallerLayer(std::sync::Arc::new(move |inner| {
            std::sync::Arc::new(Self {
                inner,
                op,
                sent: counted.clone(),
            })
        }));
        (wrap, sent)
    }
}

impl dagq::application::supervisor_service::ServiceCaller for DropsThenLoses {
    fn call(
        &self,
        use_case: dagq::domain::queue_service::UseCase,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        use dagq::application::supervisor_service::ServiceUnreachable;
        if params["op"] != self.op {
            return self.inner.call(use_case, params);
        }
        match self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => Err(ServiceUnreachable::not_done("the service is down").into()),
            1 => {
                self.inner.call(use_case, params)?;
                Err(ServiceUnreachable::maybe_done("the answer was lost").into())
            }
            _ => self.inner.call(use_case, params),
        }
    }
    fn with_token(
        &self,
        file: &Path,
    ) -> anyhow::Result<std::sync::Arc<dyn dagq::application::supervisor_service::ServiceCaller>>
    {
        self.inner.with_token(file)
    }
    fn sign_in(&self, registration: &dagq::domain::LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.sign_in(registration, pid)
    }
    fn take_back(&self, registration: &dagq::domain::LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.take_back(registration, pid)
    }
    fn hand_over(&self) {
        self.inner.hand_over();
    }
    fn sign_out(&self) {
        self.inner.sign_out();
    }
    fn credential(&self) -> Option<std::path::PathBuf> {
        self.inner.credential()
    }
    fn send_held(&self) -> anyhow::Result<bool> {
        self.inner.send_held()
    }
}

/// The supervisor's caller that never gets its ops through: every request
/// of one finds the service out of reach, however long the supervisor sends
/// it again, as a service down for longer than the supervisor runs.
pub struct KeepsDown {
    inner: std::sync::Arc<dyn dagq::application::supervisor_service::ServiceCaller>,
    ops: Vec<&'static str>,
    sent: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl KeepsDown {
    /// The layer of the supervisor's in-process route in this double for
    /// `op`, and how many requests of `op` it was given.
    pub fn wrap(
        op: &'static str,
    ) -> (
        dagq::compose::CallerLayer,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        Self::wrap_all(&[op])
    }

    /// [`Self::wrap`] of each of `ops`, the requests of all counted.
    pub fn wrap_all(
        ops: &[&'static str],
    ) -> (
        dagq::compose::CallerLayer,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = sent.clone();
        let ops = ops.to_vec();
        let wrap = dagq::compose::CallerLayer(std::sync::Arc::new(move |inner| {
            std::sync::Arc::new(Self {
                inner,
                ops: ops.clone(),
                sent: counted.clone(),
            })
        }));
        (wrap, sent)
    }
}

impl dagq::application::supervisor_service::ServiceCaller for KeepsDown {
    fn call(
        &self,
        use_case: dagq::domain::queue_service::UseCase,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        if !self.ops.iter().any(|op| params["op"] == *op) {
            return self.inner.call(use_case, params);
        }
        self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(
            dagq::application::supervisor_service::ServiceUnreachable::not_done(
                "the service is down",
            )
            .into(),
        )
    }
    fn with_token(
        &self,
        file: &Path,
    ) -> anyhow::Result<std::sync::Arc<dyn dagq::application::supervisor_service::ServiceCaller>>
    {
        self.inner.with_token(file)
    }
    fn sign_in(&self, registration: &dagq::domain::LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.sign_in(registration, pid)
    }
    fn take_back(&self, registration: &dagq::domain::LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.take_back(registration, pid)
    }
    fn hand_over(&self) {
        self.inner.hand_over();
    }
    fn sign_out(&self) {
        self.inner.sign_out();
    }
    fn credential(&self) -> Option<std::path::PathBuf> {
        self.inner.credential()
    }
}
