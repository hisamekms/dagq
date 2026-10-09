//! The queue service a test's workers and jobs reach (goal 82's stage
//! (3)): their `dagq` runs in client mode with the service's socket and a
//! token, never the queue's path, so a test whose stub agent runs `dagq`
//! starts the queue's service first ([`serve`]), or the stub starts it on
//! its first `dagq` ([`started_by_a_stub`]), and the test stops it when
//! done ([`Served`], or [`unserve`]). A service also stops itself within seconds
//! of its queue's directory going away, and one a test started within
//! seconds of the test's process going ([`OwnedByTest`]): a timeout's
//! `process::exit` or a SIGKILL skips the drops and leaves the directory
//! (task 1352).

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Command,
    sync::{LazyLock, Mutex, MutexGuard, PoisonError},
};

use crate::common::{Bounded, WithoutActor};

/// The queues whose service a test started and has not stopped.
static SERVED: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

fn served() -> MutexGuard<'static, HashSet<PathBuf>> {
    SERVED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Has the queue service the command starts, itself or through a
/// supervisor it runs, stop once this test process is gone
/// ([`dagq::domain::queue_service::OWNER_PID_ENV`]).
pub trait OwnedByTest {
    fn owned_by_test(&mut self) -> &mut Self;
}

impl OwnedByTest for Command {
    fn owned_by_test(&mut self) -> &mut Self {
        self.env(
            dagq::domain::queue_service::OWNER_PID_ENV,
            std::process::id().to_string(),
        )
    }
}

/// A `dagq` in `dir` that runs the tests' binary [`OwnedByTest`], for a
/// supervisor the test runs in its own process to start the service with:
/// the service would inherit no owner from the test's environment.
pub fn owned_executable(dir: &Path) -> PathBuf {
    let wrapper = dir.join("owned-dagq");
    let owner = std::process::id().to_string();
    crate::common::template::script_env(
        &wrapper,
        "#!/bin/sh\nexport DAGQ_SERVICE_OWNER_PID\nexec \"$OWNED_DAGQ\" \"$@\"\n",
        &[
            (dagq::domain::queue_service::OWNER_PID_ENV, &owner),
            ("OWNED_DAGQ", env!("CARGO_BIN_EXE_dagq")),
        ],
    );
    wrapper
}

fn dagq(db: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .owned_by_test()
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap()
}

/// Start the service of the queue at `db` unless this test process did
/// already, with `cmux` as the cmux it notifies the inbox through (a
/// stub's: the host's is never reached).
pub fn serve_with(db: &Path, cmux: &Path) {
    let mut served = served();
    if served.contains(db) {
        return;
    }
    let output = dagq(db, &["service", "start", "--cmux", cmux.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "the queue service of {} did not start: {}",
        db.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    served.insert(db.to_owned());
}

/// [`serve_with`] a stub cmux beside the queue that only records its calls
/// (`calls` beside it), shared like the tests' other stubs.
pub fn serve(db: &Path) {
    let stub = db.parent().unwrap().join("service-cmux").join("cmux");
    if !stub.exists() {
        std::fs::create_dir_all(stub.parent().unwrap()).unwrap();
        crate::common::template::script(
            &stub,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0%/*}/calls\"\n",
        );
    }
    serve_with(db, &stub);
}

/// The file a test's stub writes beside the queue at `db` once it started
/// the queue's service itself (the runtime tests' headless stubs start it on
/// their first client-mode `dagq`).
pub fn started_by_a_stub(db: &Path) -> PathBuf {
    db.with_file_name("queue-service-started-by-a-stub")
}

/// Stop the service [`serve`] or a stub ([`started_by_a_stub`]) started for
/// the queue at `db`, if one did.
pub fn unserve(db: &Path) {
    let by_a_stub = std::fs::remove_file(started_by_a_stub(db)).is_ok();
    if served().remove(db) || by_a_stub {
        let _ = dagq(db, &["service", "stop"]);
    }
}

/// The queue's service while it is held: started now, stopped when it
/// goes (the test returned or panicked).
pub struct Served(PathBuf);

impl Served {
    pub fn start(db: &Path) -> Self {
        serve(db);
        Self(db.to_owned())
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        unserve(&self.0);
    }
}
