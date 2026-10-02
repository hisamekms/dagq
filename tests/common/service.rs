//! The queue service a test's workers and jobs reach (goal 82's stage
//! (3)): their `dagq` runs in client mode with the service's socket and a
//! token, never the queue's path, so a test whose stub agent runs `dagq`
//! starts the queue's service first ([`serve`]) and stops it when done
//! ([`Served`], or [`unserve`]). A service also stops itself within seconds
//! of its queue's directory going away.

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

fn dagq(db: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
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

/// Stop the service [`serve`] started for the queue at `db`, if it did.
pub fn unserve(db: &Path) {
    if served().remove(db) {
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
