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
