//! `dagq install`: a person's way to replace the fixed binary and the
//! queue's supervisor with it, without waiting for the sessions (ADR-0045
//! decisions 11–14). The new binary is built (or given), checked, its
//! queue service started as a replacement's, which applies the queue's
//! compatible migrations before it accepts (ADR-t3102-2 decision 2), and
//! put in place of the old one by a rename that keeps the old one as
//! `<name>.previous`. If install started the build's service and replaced
//! the binary, the service is started again from the binary in place before
//! any live supervisor is asked to exec it. Neither install nor the update
//! job stops the service from the handoff request through the end of the
//! watch. The old binary goes back when none of them took the handoff;
//! when some did, it stays and the ones that did not are named in a
//! [`KeptBinary`] error
//! (ADR-t632-1). A build
//! whose migrations would break the old binary goes through the drain
//! instead, and only when asked to. `--rollback` does the same with
//! `<name>.previous`.
//!
//! It reads the queue's schema through the new binary (`migrate --check`,
//! which asks the queue's service; with none that answers, the new build's
//! replacement start judges it) and applies no migration itself, before it opens the queue itself (apart from a best-effort read of the
//! e2e gate's history and write of its `install_e2e_finished`, whose
//! failure is ignored), so it works whether the queue is older or newer
//! than this binary.
//!
//! 未適用のmigrationがあるか使えるserviceが無ければ差し替えの前に新buildのserviceを起動し、
//! binaryを置き換えたときは、その後・引き継ぎの要求の前に配置したbinaryのものへ替える。
//! installと更新のjobは引き継ぎの要求から見張りの終わりまでserviceを止めない。
//! それ以外は引き継いだsupervisorが替える。
//!
//! The binaries are built, run and moved through [`Binaries`], the queue
//! is read through [`QueueOpener`]; [`crate::compose`] builds the adapters.

use super::{
    Clock, HostOpsQueue, ProcessControl, QueueOpener, RunFiles, SupervisorRegistry,
    lifecycle::{Handed, hand_off, handoff_failures},
    path_text,
};
use crate::domain::{HEARTBEAT_TIMEOUT_SECS, SupervisorRegistration, slot_limits::SettingSource};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// One migration a binary would apply to a queue (`migrate --check`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMigration {
    pub version: i64,
    pub compatible: bool,
}

/// What a binary reports about a queue's schema (`migrate --check`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaCheck {
    pub pending: Vec<PendingMigration>,
    /// Whether the binary opens the queue as it is.
    pub opens: bool,
}

/// Building, running and moving `dagq` binaries.
pub trait Binaries {
    /// Build the checkout for release; the binary built.
    fn build(&self, checkout: &Path) -> Result<PathBuf>;
    /// The build identifier `binary --version` reports.
    fn version(&self, binary: &Path) -> Result<String>;
    /// Start `binary` on a throwaway queue (`init`, and a read through its
    /// queue service).
    fn probe(&self, binary: &Path) -> Result<()>;
    /// Whether `binary` continues a supervisor after an exec (it knows
    /// `supervise --handoff-token`); a binary before ADR-0045 exits on it.
    fn takes_handoff(&self, binary: &Path) -> bool;
    /// Whether `binary`'s `install` sends its steps to the queue service
    /// (it knows `install --sends-steps`): one of a build before does not,
    /// and its service reads none of this build's steps.
    fn sends_install_steps(&self, binary: &Path) -> bool {
        let _ = binary;
        true
    }
    /// Whether the build accepts update-job principals. The credential
    /// argument and the service's support shipped together; probing clap
    /// avoids running a job or opening a queue (ADR-t4638-1).
    fn accepts_update_job(&self, binary: &Path) -> bool {
        let _ = binary;
        true
    }
    /// `binary --db <db> migrate --check`: the schema facts the queue's
    /// service read, judged by `binary`; none when no service that answers
    /// them runs on the queue, which nothing else reads.
    fn schema(&self, binary: &Path, db: &Path) -> Result<Option<SchemaCheck>>;
    /// Start `binary`'s service of the queue at `db` as a replacement's
    /// (ADR-t3102-2 decision 2), taking over from the one running whatever
    /// its build: it
    /// applies the migrations of its build before it accepts, the breaking
    /// ones too with `breaking` (after the drain, on a queue nothing uses),
    /// with `binary` let change the queue's schema though it is not yet the
    /// queue's binary (decision 3). What it applied, `migrate`'s report
    /// (`null` when nothing was pending); a service that refuses applies
    /// nothing, and leaves the running one as it was when it refused before
    /// it took over ([`super::queue_service::QueueServiceControl::replace`]).
    fn serve(&self, binary: &Path, db: &Path, breaking: bool) -> Result<Value>;
    /// Hold the queue's service at `db` against the supervisors' watch
    /// until the hold is dropped ([`super::queue_service::QueueServiceControl::hold`]).
    fn hold_service(&self, db: &Path) -> Result<super::queue_service::ServiceHold> {
        let _ = db;
        Ok(super::queue_service::ServiceHold::none())
    }
    /// Whether a service of the queue at `db` answers now.
    fn service_runs(&self, db: &Path) -> bool {
        let _ = db;
        false
    }
    /// Name `target` the binary that changes the schema of the queue at
    /// `db` (ADR-t3102-2 decision 3).
    fn adopt(&self, db: &Path, target: &Path) -> Result<()> {
        let _ = (db, target);
        Ok(())
    }
    /// Put a copy of `source` at `target` by a rename in `target`'s
    /// directory, keeping what was there as [`previous_path`] (ADR-0045
    /// decision 11).
    fn replace(&self, source: &Path, target: &Path) -> Result<()>;
    /// Put [`previous_path`] back at `target` after a failed handoff.
    fn restore(&self, target: &Path) -> Result<()>;
    /// Run `binary` with `arguments`; what it printed.
    fn run(&self, binary: &Path, arguments: &[String]) -> Result<Value>;
    /// Point the automatic update's `checkout` at `commit`: a detached
    /// worktree of the repository `repository` belongs to, added when
    /// missing (ADR-0045 decision 17).
    fn checkout(&self, repository: &Path, checkout: &Path, commit: &str) -> Result<()> {
        let _ = (repository, checkout, commit);
        bail!("these binaries cannot check a commit out")
    }
    /// Build `checkout` for release into `target_dir`, which the variable
    /// `target_env` points at (the Rust profile's), running `command` (a
    /// shell command) in place of `cargo build --release --locked -p
    /// dagq` when given, with what it prints appended to `log`; the binary
    /// built.
    fn build_into(
        &self,
        checkout: &Path,
        target_dir: &Path,
        target_env: &str,
        command: Option<&str>,
        log: &Path,
    ) -> Result<PathBuf> {
        let _ = (checkout, target_dir, target_env, command, log);
        bail!("these binaries cannot build a checkout")
    }
    /// Run the e2e of `checkout` (ADR-t963-1 decision 1) with
    /// `CARGO_TARGET_DIR` set to `target_dir` when given, and clean up what
    /// it left on disk; how it went. An error is an e2e that
    /// could not start (an unreadable `[run.env]`, the e2e lock).
    fn e2e(
        &self,
        checkout: &Path,
        target_dir: Option<&Path>,
        settings: &E2eSettings,
    ) -> Result<E2eOutcome> {
        let _ = (checkout, target_dir, settings);
        bail!("these binaries cannot run the e2e")
    }
}

/// The gate's e2e, unless [`E2eSettings::command`] gives another.
pub const E2E_COMMAND: &str = "cargo test --locked --test e2e -- --ignored";

/// How long the gate's e2e may run by default before it counts as failed
/// (ADR-t963-1 decision 1).
pub const E2E_TIMEOUT: Duration = Duration::from_secs(1800);

/// A repository's declared shell command and its time limit, ready to
/// carry to an executor without reading the repository again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct E2eCommand {
    pub command: String,
    pub timeout: Duration,
}

/// The e2e a build of dagq's source passes before it is put in place
/// (ADR-t963-1 decision 1): `cargo test --locked --test e2e -- --ignored`
/// in the checkout.
/// Gate cleanup lists processes with `ps`, stops only those pointing
/// inside the e2e's TMPDIR, then removes that directory.
#[derive(Debug, Clone)]
pub struct E2eSettings {
    /// A shell command in place of `cargo test --locked --test e2e --
    /// --ignored` (a repository declaration or a test override).
    pub command: Option<String>,
    /// How long it may run; past it the e2e is stopped and failed.
    pub timeout: Duration,
    /// The directory whose `dagq.toml` `[run.env]` the e2e runs with,
    /// expanded with `queue_dir`; `None` passes none.
    pub run_env_root: Option<PathBuf>,
    /// `${DAGQ_QUEUE_DIR}` of the `[run.env]`.
    pub queue_dir: Option<PathBuf>,
    /// The directory the e2e's fixtures are made in (its `TMPDIR`, one
    /// directory per e2e under it), removed with what they left.
    pub scratch: PathBuf,
    /// Where the e2e's output is appended.
    pub log: PathBuf,
    /// The host's offset from UTC in seconds, for the local date a mark's
    /// `until` is read against (ADR-t1165-1).
    pub utc_offset_secs: i64,
    /// The host's one e2e at a time (ADR-t1233-2 decision 4): the file the
    /// e2e holds a lock on while it runs, waiting for it first. The
    /// automatic update's gate, `install`'s and the runtime's e2e of the
    /// runs take the same one ([`e2e_lock_path`]); `None` takes none.
    pub lock: Option<PathBuf>,
    /// Variables the e2e runs with on top of its `[run.env]`: the guard of
    /// the git of a container run's landing worktree (ADR-t2179-1 decision
    /// 8); none otherwise.
    pub git_env: Vec<(String, String)>,
    /// Told of each command the e2e ran as it ends (its executor's steps,
    /// ADR-t1662-3 decision 1); `None` tells none.
    pub ended: Option<E2eEnded>,
}

/// A command of the e2e that ended: the failed tests it reran (none for
/// the e2e itself), when it started and ended, its exit code (none when it
/// was stopped or not waited for) and how many bytes it wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct E2eCommandEnded {
    pub rerun: Option<Vec<String>>,
    pub started: std::time::Instant,
    pub ended: std::time::Instant,
    pub exit_code: Option<i32>,
    pub output_bytes: usize,
}

/// What is told of each command of the e2e as it ends.
#[derive(Clone)]
pub struct E2eEnded(pub std::sync::Arc<dyn Fn(E2eCommandEnded) + Send + Sync>);

impl std::fmt::Debug for E2eEnded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("E2eEnded")
    }
}

/// The host's e2e lock of the queues under `queue_dir`'s parent, dagq's
/// data directory (`<data dir>/e2e.lock`): every queue of the host takes
/// the same, and an e2e's throwaway queues, made under a data directory of
/// their own, do not take the one of the e2e that runs them.
pub fn e2e_lock_path(queue_dir: &Path) -> Option<PathBuf> {
    queue_dir.parent().map(|data| data.join("e2e.lock"))
}

impl E2eSettings {
    /// Where the rerun of the failed tests is written (ADR-t1165-1): the
    /// log with `.rerun.log` for its `.log`.
    pub fn rerun_log(&self) -> PathBuf {
        let name = self
            .log
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stem = name.strip_suffix(".log").unwrap_or(&name);
        self.log.with_file_name(format!("{stem}.rerun.log"))
    }
}

/// How the gate's e2e went.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct E2eOutcome {
    pub passed: bool,
    /// It ran past its timeout and was stopped.
    pub timed_out: bool,
    /// The tests its output names as failed.
    pub failed_tests: Vec<String>,
    pub secs: u64,
    /// What was cleaned up after it (processes, the directory).
    pub cleanup: Value,
    /// The rerun of the failed tests by name (ADR-t1165-1); `None` when the
    /// e2e passed, ran past its timeout or named no failed test.
    pub rerun: Option<E2eRerun>,
    /// The marks the gate found in the checkout (ADR-t1165-1).
    pub quarantine: crate::domain::e2e_quarantine::QuarantineFile,
    /// How long it waited for the host's e2e lock before it started
    /// ([`E2eSettings::lock`], ADR-t1233-2 decision 4).
    pub lock_wait_secs: u64,
    /// The port and why when the e2e ran without `RUSTC_WRAPPER`: its
    /// `[run.env]` names sccache, and its server was not confirmed just
    /// before or its guard not made (ADR-t2086-1). Its caller records it as
    /// `sccache_wrapper_removed`.
    pub sccache_wrapper_removed: Option<(u16, String)>,
}

/// How the rerun of the tests the e2e failed went (ADR-t1165-1): the same
/// checkout, env and target, the tests by name (`--exact`), within the same
/// timeout and cleaned up the same way.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct E2eRerun {
    /// The tests rerun.
    pub tests: Vec<String>,
    /// The ones that failed again (all of them when the rerun ran past its
    /// timeout, could not start or failed naming none).
    pub failed: Vec<String>,
    pub timed_out: bool,
    pub secs: u64,
    pub cleanup: Value,
    /// Why it could not start.
    pub error: Option<String>,
}

impl E2eOutcome {
    /// Why it did not pass, for an error and a question.
    pub fn failure(&self, settings: &E2eSettings) -> String {
        let what = if self.timed_out {
            format!(
                "the e2e did not finish within {}s and was stopped",
                settings.timeout.as_secs()
            )
        } else if self.failed_tests.is_empty() {
            "the e2e failed".to_owned()
        } else {
            format!("the e2e failed: {}", self.failed_tests.join(", "))
        };
        format!("{what}; see {}", settings.log.display())
    }

    /// The `e2e` of a report: `passed` with its time and log. The gate's
    /// fields (`flaky`, `quarantined`, and `rerun` and `quarantine` when
    /// there are any) are added beside them as on the update's events.
    pub fn report(&self, settings: &E2eSettings) -> Value {
        json!({"status": "passed", "secs": self.secs, "log": settings.log})
    }
}

/// Whether `install` runs the e2e of a checkout it builds (ADR-t963-1
/// decision 1). A binary, a rollback and a release have no gate.
// Made once per `install`; boxing the settings buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum E2eGate {
    /// The source has none (not dagq's source, or a built binary): the
    /// report's `e2e` is `{"status": "not_applicable"}`.
    NotApplicable,
    /// A person skipped it (`--skip-e2e`): `{"status": "skipped"}`.
    Skip,
    /// Run it; the report's `e2e` is [`E2eOutcome::report`]'s. When there
    /// is a queue, the gate records `install_e2e_finished` whether it
    /// passes or fails, and counts a marked test's failures in a row from
    /// the queue's e2e gate events, its own of earlier installs among them
    /// ([`super::EventStore::e2e_gate_events`]). The report's `e2e` names
    /// that event (`event_id`) or why none was written (`event_error`).
    Run(E2eSettings),
}

/// Installs a dagq release from crates.io (ADR-t618-1 decision 5): `cargo
/// install --locked dagq@<version> --root <root> --target-dir
/// <target_dir>`, what it prints appended to `log`; the binary installed,
/// `<root>/bin/dagq`.
pub trait ReleaseInstaller {
    fn install(&self, version: &str, root: &Path, target_dir: &Path, log: &Path)
    -> Result<PathBuf>;
}

/// The binary of release `version` to put in place of `target` (ADR-t618-1
/// decision 5): `target` itself when it names that version already (the
/// answer of another queue of the host replaced it), else what `installer`
/// installs under `root`.
pub fn release_binary(
    binaries: &dyn Binaries,
    installer: &dyn ReleaseInstaller,
    version: &str,
    target: &Path,
    root: &Path,
    target_dir: &Path,
    log: &Path,
) -> Result<PathBuf> {
    if binaries.version(target).ok().as_deref() == Some(version) {
        return Ok(target.to_path_buf());
    }
    installer.install(version, root, target_dir, log)
}

/// Where the binary `install` puts in place comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Build this checkout.
    Checkout(PathBuf),
    /// A binary built already: given by a person, or built or installed by
    /// dagq itself (the automatic update's build, a release).
    Binary(PathBuf),
    /// The binary the last install replaced (`<target>.previous`). Going
    /// back to a binary that does not refuse an unknown `DAGQ_ROLE` and
    /// knows no newer role leaves a window: a job the newer binary started
    /// and still runs is not restricted by the binary its calls now reach.
    Rollback,
}

impl Source {
    /// The source of `install` without `--from`: `checkout`, the main
    /// checkout of the working directory's repository, built, when it is
    /// dagq's source (ADR-t614-1). Elsewhere building it would not give
    /// dagq, so it is an error that says how to update dagq instead.
    pub fn default_checkout(checkout: PathBuf, dagq_source: bool) -> Result<Self> {
        ensure!(
            dagq_source,
            "install without --from builds dagq from the repository's sources, and {} is not \
dagq's source (its Cargo.toml has no [package] named {}); update dagq with `cargo install dagq`, \
or pass --from with a built binary or a checkout of dagq",
            checkout.display(),
            crate::domain::source_repository::PACKAGE
        );
        Ok(Self::Checkout(checkout))
    }
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub source: Source,
    /// The fixed binary to replace.
    pub target: PathBuf,
    /// Go through the drain when the new binary's migrations would break
    /// the old one.
    pub allow_breaking: bool,
    /// Arguments of the `up` that starts the supervisor again after such a
    /// drain, beside `--db` and `--parallel` (taken from the drained
    /// supervisor): the agents' executables, `--plugin-dir` and the flags
    /// older command lines carry. The `up` starts
    /// it under launchd whatever mode the drained one had (ADR-t1433-4).
    pub restart: Vec<String>,
    pub handoff_timeout: Duration,
    pub poll: Duration,
    /// The e2e of a checkout's build before it is put in place.
    pub e2e: E2eGate,
}

pub struct Ports<'a, P: ?Sized> {
    pub binaries: &'a dyn Binaries,
    pub files: &'a dyn RunFiles,
    pub processes: &'a dyn ProcessControl,
    pub clock: &'a dyn Clock,
    /// Opens the queue at a database path, as host運用's ports `P`.
    pub queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener<P>>,
    /// `down --wait` on the queue, for a breaking migration's drain.
    pub down: &'a dyn Fn() -> Result<Value>,
}

/// Where the binary a replacement kept goes: `<target>.previous`.
pub fn previous_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".previous");
    target.with_file_name(name)
}

/// Replace `options.target` with the binary of `options.source` and hand
/// the queue at `db`'s live supervisors over to it (see the module). `db`
/// is `None`, or names no file, outside a queue: then only the binary is
/// replaced.
///
/// If install started the source build's service and replaced the binary,
/// it starts the service from `options.target` before requesting any
/// supervisor's handoff. A start that leaves no service is tried once more;
/// if it still leaves none, install attempts to restore the binary and
/// reports the stopped service and restoration result without requesting
/// handoff.
///
/// 元のserviceを実際に起動してbinaryを置き換えた場合は、引き継ぎの要求前に配置したbinaryのserviceへ起動し直す。
/// serviceが残らなければ一度再試行し、それでも無ければ要求せずbinaryの戻しを試み、停止したserviceと戻しの結果を報告する。
pub fn install(
    ports: &Ports<'_, impl HostOpsQueue + ?Sized>,
    db: Option<&Path>,
    options: &InstallOptions,
) -> Result<Value> {
    let built = built(ports, db, options)?;
    install_built(ports, db, options, built)
}

/// The binary an install of `options.source` puts in place: a checkout's
/// build once its e2e gate passed, a binary as it is, or `.previous`.
pub struct Built {
    pub binary: PathBuf,
    /// The report's `e2e`.
    pub e2e: Value,
}

/// Build `options.source` and run its e2e gate, or find the binary it
/// names, for [`install_built`].
pub fn built(
    ports: &Ports<'_, impl HostOpsQueue + ?Sized>,
    db: Option<&Path>,
    options: &InstallOptions,
) -> Result<Built> {
    let previous = previous_path(&options.target);
    Ok(match &options.source {
        Source::Checkout(checkout) => {
            let binary = ports
                .binaries
                .build(checkout)
                .with_context(|| format!("build {}", checkout.display()))?;
            let e2e = match &options.e2e {
                E2eGate::NotApplicable => json!({"status": "not_applicable"}),
                E2eGate::Skip => json!({"status": "skipped"}),
                E2eGate::Run(settings) => e2e_gate(ports, db, checkout, &binary, settings)?,
            };
            Built { binary, e2e }
        }
        Source::Binary(binary) => Built {
            binary: binary.clone(),
            e2e: json!({"status": "not_applicable"}),
        },
        Source::Rollback => {
            ensure!(
                ports.files.is_file(&previous),
                "no previous binary at {} to roll back to",
                previous.display()
            );
            Built {
                binary: previous,
                e2e: json!({"status": "not_applicable"}),
            }
        }
    })
}

/// [`install`] of the binary [`built`] gave.
pub fn install_built(
    ports: &Ports<'_, impl HostOpsQueue + ?Sized>,
    db: Option<&Path>,
    options: &InstallOptions,
    built: Built,
) -> Result<Value> {
    install_built_as(ports, db, options, built, false)
}

/// Install by an update job: every build placed at target must keep its
/// credential usable, including the one restored after a failed handoff.
pub(super) fn install_by_update_job(
    ports: &Ports<'_, impl HostOpsQueue + ?Sized>,
    db: Option<&Path>,
    options: &InstallOptions,
) -> Result<Value> {
    install_built_as(ports, db, options, built(ports, db, options)?, true)
}

fn install_built_as(
    ports: &Ports<'_, impl HostOpsQueue + ?Sized>,
    db: Option<&Path>,
    options: &InstallOptions,
    built: Built,
    update_job: bool,
) -> Result<Value> {
    let Ports {
        binaries, files, ..
    } = *ports;
    let target = &options.target;
    let previous = previous_path(target);
    let Built {
        binary: source,
        e2e,
    } = built;
    check_update_job(binaries, &source, update_job)?;
    let version = binaries
        .version(&source)
        .with_context(|| format!("{} does not report its version", source.display()))?;
    binaries
        .probe(&source)
        .with_context(|| format!("{} does not start on a throwaway queue", source.display()))?;
    let replaced_version = if files.is_file(target) {
        binaries.version(target).ok()
    } else {
        None
    };
    // The binary in place already (a release another queue's answer
    // installed): only the handoff is left, and `.previous` stays the
    // build it replaced.
    let in_place = source == *target;
    let db = db.filter(|db| files.is_file(db));
    let Some(db) = db else {
        if !in_place {
            binaries.replace(&source, target)?;
        }
        return Ok(json!({
            "outcome": "installed",
            "target": target,
            "version": version,
            "previous": previous,
            "previous_version": replaced_version,
            "migrated": Value::Null,
            "supervisors": [],
            "e2e": e2e,
        }));
    };
    // With no service that answers for the queue's schema, nothing reads
    // it: the replacement start of the new build's service judges it, and
    // applies the compatible migrations before it accepts, or refuses the
    // queue (a breaking migration, a schema newer than it) and nothing is
    // replaced.
    let checked = binaries.schema(&source, db)?;
    let unread = checked.is_none();
    let schema = checked.unwrap_or(SchemaCheck {
        pending: Vec::new(),
        opens: true,
    });
    let start = unread || !schema.pending.is_empty();
    let breaking: Vec<String> = schema
        .pending
        .iter()
        .filter(|migration| !migration.compatible)
        .map(|migration| migration.version.to_string())
        .collect();
    if !breaking.is_empty() {
        ensure!(
            options.allow_breaking,
            "{version} brings breaking migration(s) {} the running supervisor and its runs' \
wrappers could not open the queue after: nothing was replaced. `install --allow-breaking` drains \
the supervisor (waits for its runs), migrates with a backup and starts it again",
            breaking.join(", ")
        );
        return install_breaking(ports, db, &source, &version, replaced_version, options).map(
            |mut report| {
                report["e2e"] = e2e;
                report
            },
        );
    }
    ensure!(
        schema.opens || !schema.pending.is_empty(),
        "the queue refuses {version}: a breaking migration after it was applied, so it cannot \
open the queue again; nothing was replaced. The queue as it was before that migration is in the \
backups/ directory next to it"
    );
    // The service the new build starts is held from the supervisors of the
    // build before until they are handed over and its binary is in place.
    let mut hold = if start {
        Some(binaries.hold_service(db)?)
    } else {
        None
    };
    // Before the queue is opened: this binary may be the new build, which
    // opens it only migrated.
    let migrated = if !start {
        Value::Null
    } else if unread {
        binaries.serve(&source, db, false).with_context(|| {
            format!(
                "no queue service answered for the queue's schema, and {version}'s could not \
start: nothing was replaced"
            )
        })?
    } else {
        binaries.serve(&source, db, false)?
    };
    // The install reaches the queue only through its service: with none
    // answering (nothing runs on the queue), or one of a build before that
    // reads none of its steps, it starts the new build's, as for a
    // migration, and the binary in place takes over after.
    let (queue, served) = match (ports.queues)(db).open() {
        Ok(queue) => (queue, start),
        // A service of a build before this command does not read its steps:
        // the install sends them to its build's, as to none.
        Err(error)
            if hold.is_none()
                && (super::supervisor_service::unreachable(&error)
                    || super::supervisor_service::command_unknown(&error)) =>
        {
            hold = Some(binaries.hold_service(db)?);
            binaries.serve(&source, db, false).with_context(|| {
                format!("no queue service answered, and {version}'s could not start: nothing was replaced")
            })?;
            ((ports.queues)(db).open()?, true)
        }
        Err(error) => return Err(error),
    };
    let _hold = hold;
    let live = live_supervisors(ports.clock, ports.processes, &*queue)?;
    let (takes, cannot): (Vec<_>, Vec<_>) = live
        .into_iter()
        .partition(|registration| registration.handoff_accepted);
    // An exec of a binary that does not know the handoff would end the
    // supervisor and orphan its sessions' leases.
    ensure!(
        takes.is_empty() || binaries.takes_handoff(&source),
        "{version} cannot continue a supervisor after an exec (it predates the handoff), so the \
running supervisor cannot be handed over to it; nothing was replaced. Stop the supervisor \
(`down --wait`), put the binary in place and run `up`"
    );
    if !in_place {
        binaries.replace(&source, target)?;
    }
    // Finish the service replacement before asking any supervisor to exec:
    // its first requests must not race with our stopping the service. The
    // replacement grant permits this before adopt records the binary.
    let undo = Undo {
        binaries,
        target,
        previous: &previous,
        version: &version,
        replaced_version: replaced_version.as_deref(),
        in_place,
        update_job,
    };
    let service = (served && !in_place).then(|| {
        let first = serve_in_place(binaries, target, db);
        if first["outcome"] == "stopped" {
            // There is no service to receive the handoff. Try once more
            // while no supervisor has been asked to exec the binary.
            serve_in_place(binaries, target, db)
        } else {
            first
        }
    });
    if let Some(service) = service.as_ref().filter(|s| s["outcome"] == "stopped") {
        let (error, restored) = undo.restore(
            anyhow::anyhow!(service["error"].as_str().unwrap_or_default().to_owned()),
            &format!("the queue service for {version} could not start before handoff"),
        );
        // No service can receive a handoff. A person's install returns
        // this report directly; an update job recovers its supervisor and
        // records the failure after the hold is dropped and service answers.
        return Err(super::update::InstallFailed {
            message: format!("{error:#}"),
            report: json!({
                "outcome": "failed",
                "stage": "install",
                "target": target,
                "version": version,
                "previous": previous,
                "previous_version": replaced_version,
                "migrated": migrated,
                "kept": false,
                "restored": restored,
                "e2e": e2e,
                "supervisors": [],
                "queue_service": service,
            }),
        }
        .into());
    }
    let handed = if takes.is_empty() {
        Vec::new()
    } else {
        let handed = hand_off(
            &*queue,
            ports.processes,
            ports.clock,
            &takes,
            target,
            &version,
            options.handoff_timeout,
            options.poll,
        );
        match handed {
            // The binary is one file for every supervisor: it goes back
            // only when none of them took it (ADR-t632-1).
            Ok(handed) if handed.iter().all(|h| h.error.is_some()) => {
                let error = handoff_failures(&handed)
                    .unwrap_or_else(|| anyhow::anyhow!("no supervisor took the handoff"));
                return Err(undo.error(error, Some(handed.iter().map(Handed::report).collect())));
            }
            Ok(handed) => handed,
            // One of them may run it: the binary stays.
            Err(error) if super::lifecycle::HandoffUnsettled::is(&error) => {
                return Err(error.context(format!(
                    "{version} stays at {}, as a supervisor asked to take it may run it; check `status`, then `down --force` and `up` to start every supervisor with it, or `install --rollback` to put the binary it replaced back for every supervisor",
                    target.display()
                )));
            }
            Err(error) => return Err(undo.error(error, None)),
        }
    };
    binaries.adopt(db, target)?;
    let failure = handoff_failures(&handed);
    let mut report = json!({
        "outcome": if failure.is_some() { "partially_handed_off" } else { "installed" },
        "target": target,
        "version": version,
        "previous": previous,
        "previous_version": replaced_version,
        "migrated": migrated,
        "kept": failure.is_some(),
        "e2e": e2e,
        "supervisors": handed.iter().map(Handed::report).collect::<Vec<_>>(),
        // A supervisor of a binary before ADR-0045 takes no handoff;
        // `up` drains and replaces it.
        "not_handed_off": cannot
            .iter()
            .map(|registration| json!({
                "token": registration.token,
                "pid": registration.pid,
                "version": registration.binary_version,
                "next": "run `up` to drain and replace it",
            }))
            .collect::<Vec<_>>(),
    });
    if let Some(service) = service {
        report["queue_service"] = service;
    }
    match failure {
        None => Ok(report),
        Some(error) => {
            let took = handed.iter().filter(|h| h.error.is_none()).count();
            Err(KeptBinary {
                message: format!(
                    "{:#}",
                    error.context(format!(
                        "{took} of the {} supervisors took the handoff to {version}, so it stays \
at {}; the ones that did not go on as the processes of the binary they had. Run `down --force` and \
`up` to start them with {version}, or `install --rollback` to put the binary it replaced back for \
every supervisor",
                        handed.len(),
                        target.display()
                    ))
                ),
                report,
            }
            .into())
        }
    }
}

/// Run the e2e gate of `built`, the build of `checkout` (ADR-t963-1
/// decision 1, ADR-t1165-1): the report's `e2e` when it passes, else the
/// error that replaces nothing. Either way, when the queue at `db` opens,
/// one `install_e2e_finished` records it (ADR-t1165-1 decision 4) beside
/// the automatic update's gates; the report's `e2e` names the event
/// (`event_id`) or why none was written (`event_error`).
fn e2e_gate<Q: HostOpsQueue + ?Sized>(
    ports: &Ports<'_, Q>,
    db: Option<&Path>,
    checkout: &Path,
    built: &Path,
    settings: &E2eSettings,
) -> Result<Value> {
    // The build's own target (`<target>/release/dagq`): the e2e is not
    // given the shell's `CARGO_TARGET_DIR`, so it is named for it to build
    // no second time.
    let target_dir = built.parent().and_then(Path::parent);
    let ran = ports.binaries.e2e(checkout, target_dir, settings);
    // The gates before this one, for a marked test failing in a row
    // (ADR-t1165-1); a queue that cannot be read has none and gets no
    // event.
    let queue = match db.filter(|db| ports.files.is_file(db)) {
        None => Err(anyhow::anyhow!("no queue")),
        Some(db) => (ports.queues)(db).open(),
    };
    let mut event = json!({
        "gate": crate::domain::INSTALL_GATE,
        "checkout": checkout,
        "binary": built,
        "version": ports.binaries.version(built).ok(),
        "log": settings.log,
    });
    let record = |event: Value| -> std::result::Result<crate::domain::EventId, String> {
        let queue = queue.as_ref().map_err(|error| format!("{error:#}"))?;
        queue
            .record_queue_event(crate::domain::EventKind::InstallE2eFinished, event)
            .map_err(|error| format!("{error:#}"))
    };
    let warn = |recorded: std::result::Result<crate::domain::EventId, String>| {
        if let Err(error) = recorded {
            tracing::warn!("the install's e2e gate was not recorded: {error}");
        }
    };
    let outcome = match ran {
        Ok(outcome) => outcome,
        Err(error) => {
            let error = error.context(format!(
                "the e2e of {} could not start, so nothing was replaced (a person may pass \
--skip-e2e to install without it)",
                checkout.display()
            ));
            event["outcome"] = json!("failed");
            event["error"] = json!(format!("{error:#}"));
            warn(record(event));
            return Err(error);
        }
    };
    let history = queue
        .as_ref()
        .ok()
        .and_then(|queue| queue.e2e_gate_events(super::e2e_verdict::HISTORY).ok())
        .unwrap_or_default();
    let verdict = super::e2e_verdict::judge(&outcome, settings, &history, ports.clock.now());
    for (key, value) in [
        ("secs", json!(outcome.secs)),
        ("timed_out", json!(outcome.timed_out)),
        ("failed_tests", json!(outcome.failed_tests)),
        ("cleanup", outcome.cleanup.clone()),
    ] {
        event[key] = value;
    }
    if let (Some(event), Some(fields)) = (event.as_object_mut(), verdict.fields.as_object()) {
        event.extend(fields.clone());
    }
    if let Some(failure) = &verdict.failure {
        event["outcome"] = json!("failed");
        event["error"] = json!(failure);
        warn(record(event));
        bail!(
            "{failure}: nothing was replaced (a person may pass --skip-e2e to install without it)"
        );
    }
    event["outcome"] = json!("passed");
    let mut report = outcome.report(settings);
    if let (Some(report), Some(fields)) = (report.as_object_mut(), verdict.fields.as_object()) {
        report.extend(fields.clone());
    }
    match record(event) {
        Ok(id) => report["event_id"] = json!(id),
        Err(error) => report["event_error"] = json!(error),
    }
    Ok(report)
}

/// The error of an install that put the new binary in place and handed
/// some of the supervisors over to it, but not all (ADR-t632-1): the binary
/// stays, and `report` is what `install` reports, with `kept: true` and each
/// supervisor's outcome, for the command to print beside the error and for
/// the automatic update to read.
#[derive(Debug)]
pub struct KeptBinary {
    pub message: String,
    pub report: Value,
}

impl std::fmt::Display for KeptBinary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for KeptBinary {}

impl KeptBinary {
    /// The [`KeptBinary`] `error` is or wraps.
    pub fn of(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

/// A job cannot replace the target with a build whose service refuses its
/// principal, even after launchd or reach_at_start starts that service.
/// Human install and rollback send as their caller and bypass this gate.
pub(super) fn check_update_job(
    binaries: &dyn Binaries,
    binary: &Path,
    update_job: bool,
) -> Result<()> {
    ensure!(
        !update_job || binaries.accepts_update_job(binary),
        "{} does not accept the update job's principal",
        binary.display()
    );
    Ok(())
}

/// A refused previous build needs a person's rollback. Keep that advice
/// out of candidate checks, where the installed binary was never replaced.
pub(super) fn check_update_job_restore(
    binaries: &dyn Binaries,
    binary: &Path,
    update_job: bool,
) -> Result<()> {
    check_update_job(binaries, binary, update_job).context(
        "the previous build was left unrestored; the installed binary stays in place; a person can run `install --rollback`",
    )
}

/// What an install undoes when no supervisor took the new binary, including
/// a service start that failed before any handoff request.
struct Undo<'a> {
    binaries: &'a dyn Binaries,
    target: &'a Path,
    previous: &'a Path,
    version: &'a str,
    replaced_version: Option<&'a str>,
    in_place: bool,
    update_job: bool,
}

impl Undo<'_> {
    /// Put the binary it replaced back (unless it was this build already)
    /// and say so around `error`; with each supervisor's outcome, as a
    /// [`HandoffFailed`].
    fn error(&self, error: anyhow::Error, supervisors: Option<Vec<Value>>) -> anyhow::Error {
        let (error, restored) =
            self.restore(error, &format!("the handoff to {} failed", self.version));
        match supervisors {
            Some(supervisors) => HandoffFailed {
                message: format!("{error:#}"),
                supervisors,
                restored,
            }
            .into(),
            None => error,
        }
    }

    /// Restore the binary before returning a failed handoff or a service
    /// start that left no receiver for its requests, with the restore's report.
    /// `failure` names the stage that actually failed: a service start before
    /// any handoff request must not be described as a failed handoff.
    fn restore(&self, error: anyhow::Error, failure: &str) -> (anyhow::Error, Value) {
        let Self {
            target,
            previous,
            version,
            ..
        } = *self;
        if self.in_place {
            let context = format!(
                "{failure}; {} was {version} already and stays",
                target.display()
            );
            (
                error.context(context.clone()),
                json!({"restored": false, "reason": context}),
            )
        } else if let Err(reason) =
            check_update_job_restore(self.binaries, previous, self.update_job)
        {
            let reason = format!("{reason:#}");
            (
                error.context(reason.clone()),
                // Recovery may start the binary still in place to record
                // the failure; this never starts the refused previous build.
                json!({"restored": false, "reason": reason, "kept_for_job": true}),
            )
        } else {
            match self.binaries.restore(target) {
                Ok(()) => (
                    error.context(format!(
                        "{failure}; the binary it replaced is back at {}",
                        target.display()
                    )),
                    json!({
                        "restored": true,
                        "version": self.replaced_version,
                    }),
                ),
                Err(restore) => {
                    let reason = format!("{restore:#}");
                    (
                        error.context(format!(
                            "{failure}, and the binary it replaced could \
not be put back at {} ({reason}); it is at {}",
                            target.display(),
                            previous.display()
                        )),
                        json!({"restored": false, "reason": reason}),
                    )
                }
            }
        }
    }
}

/// The error of an install whose handoff every supervisor failed: the
/// binary it replaced went back (or `restored` says why not), and
/// `supervisors` is each one's outcome as [`Handed::report`] gives it, for
/// the automatic update to bring each of them back.
#[derive(Debug)]
pub struct HandoffFailed {
    pub message: String,
    pub supervisors: Vec<Value>,
    /// `restored` and `version`, or `reason` when the binary did not go back.
    pub restored: Value,
}

impl std::fmt::Display for HandoffFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HandoffFailed {}

impl HandoffFailed {
    /// The [`HandoffFailed`] `error` is or wraps.
    pub fn of(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

/// The drain of ADR-0045 decision 14: stop the live supervisors and wait
/// for their runs (`down --wait`), start the new binary's service as a
/// replacement's that may apply breaking migrations (ADR-t3102-2 decision
/// 2: it judges the queue unused, backs it up and migrates it in one
/// transaction before it accepts, and a queue still in use is refused and
/// nothing replaced; restore the old service and drained supervisor on
/// refusal), replace the binary, start the service again from it,
/// and start the supervisor again with the new binary's `up`, in the mode
/// and with the parallelism the drained one had.
///
/// The started supervisor's `up` carries over the drained ones' values, so
/// no second `up` is needed after the drain: `--auto-update` when any of
/// them had it, and the parallelism, wait limit, limit of the runtime's
/// planners and load threshold each took from a flag, as the same flags. A value one took from
/// `dagq.toml` or the default is left for the started one to resolve again.
/// A registration that records no source for its values (from a binary
/// older than the source columns) carries them over as flag-sourced ones
/// do, except `--max-load`: it is carried over only from a registration
/// that records the flag as its source
/// ([`SupervisorRegistration::flag_max_load`]), 0 when the flag disabled
/// the hold. A flag the restart's arguments hold already is not added
/// again.
fn install_breaking(
    ports: &Ports<'_, impl HostOpsQueue + ?Sized>,
    db: &Path,
    source: &Path,
    version: &str,
    replaced_version: Option<String>,
    options: &InstallOptions,
) -> Result<Value> {
    let binaries = ports.binaries;
    // With no service answering nothing runs on the queue to drain: the
    // breaking build's service judges the queue unused itself before it
    // migrates.
    let live = match (ports.queues)(db).open() {
        Ok(queue) => live_supervisors(ports.clock, ports.processes, &*queue)?,
        Err(error) if super::supervisor_service::unreachable(&error) => Vec::new(),
        Err(error) if super::supervisor_service::command_unknown(&error) => {
            return Err(error.context(
                "the queue's service is of a build before this install and reads none of its \
steps, so the supervisors to drain are unknown: nothing was replaced. Run `up` with this build \
first (it replaces the service), then this install again",
            ));
        }
        Err(error) => return Err(error),
    };
    let drained = if live.is_empty() {
        Value::Null
    } else {
        (ports.down)()?
    };
    let migrated = binaries.serve(source, db, true).map_err(|cause| {
        restore_before_replace(
            ports,
            db,
            &live,
            options,
            cause.context(format!(
                "{version} could not migrate the queue; nothing was replaced"
            )),
        )
    })?;
    binaries.replace(source, &options.target)?;
    binaries.adopt(db, &options.target)?;
    let service = (source != options.target).then(|| serve_in_place(binaries, &options.target, db));
    let started = match live.first() {
        None => Value::Null,
        Some(drained) => {
            let arguments = restart_arguments(path_text(db)?, drained, &live, &options.restart);
            binaries.run(&options.target, &arguments).with_context(|| {
                format!(
                    "{version} is installed and the queue migrated, but its `up` failed; run it \
again"
                )
            })?
        }
    };
    let mut report = json!({
        "outcome": "installed",
        "target": options.target,
        "version": version,
        "previous": previous_path(&options.target),
        "previous_version": replaced_version,
        "migrated": migrated,
        "drained": drained,
        "up": started,
    });
    if let Some(service) = service {
        report["queue_service"] = service;
    }
    Ok(report)
}

/// Recover from a refusal before replacing the binary. `target` still
/// names the old binary. Restore its service before any `up` needs the
/// queue; reuse an answering service when a caller left it running.
/// The caller's cause and both recovery outcomes survive in the error.
fn restore_before_replace(
    ports: &Ports<'_, impl HostOpsQueue + ?Sized>,
    db: &Path,
    live: &[SupervisorRegistration],
    options: &InstallOptions,
    cause: anyhow::Error,
) -> anyhow::Error {
    let service = if ports.binaries.service_runs(db) {
        Recovery::AlreadyRunning
    } else {
        match ports.binaries.serve(&options.target, db, false) {
            Ok(_) => Recovery::Started,
            Err(error) => Recovery::Failed(format!("{error:#}")),
        }
    };
    let supervisor = match recovery_supervisor(&service, !live.is_empty()) {
        SupervisorRecovery::Start => {
            let started = path_text(db).and_then(|db| {
                let arguments = restart_arguments(db, &live[0], live, &options.restart);
                ports.binaries.run(&options.target, &arguments)
            });
            match started {
                Ok(_) => Recovery::Started,
                Err(error) => Recovery::Failed(format!("{error:#}")),
            }
        }
        SupervisorRecovery::NotDrained => Recovery::NotDrained,
        SupervisorRecovery::ServiceFailed => {
            Recovery::Failed("not attempted because the queue service failed".into())
        }
    };
    let message = format!("{cause:#}; {}", recovery_message(&service, &supervisor));
    cause.context(message)
}

#[derive(Debug, PartialEq, Eq)]
enum Recovery {
    Started,
    AlreadyRunning,
    Failed(String),
    NotDrained,
}

#[derive(Debug, PartialEq, Eq)]
enum SupervisorRecovery {
    Start,
    NotDrained,
    ServiceFailed,
}

/// A supervisor needs an answering service, and only drained registrations
/// are restarted. With no drained registration, restore just the service.
fn recovery_supervisor(service: &Recovery, drained: bool) -> SupervisorRecovery {
    if !drained {
        SupervisorRecovery::NotDrained
    } else if matches!(service, Recovery::Failed(_)) {
        SupervisorRecovery::ServiceFailed
    } else {
        SupervisorRecovery::Start
    }
}

/// Keep recovery results separate from the original error's cause chain.
fn recovery_message(service: &Recovery, supervisor: &Recovery) -> String {
    let outcome = |result: &Recovery| match result {
        Recovery::Started => "started".to_owned(),
        Recovery::AlreadyRunning => "already_running".to_owned(),
        Recovery::Failed(reason) => format!("failed: {reason}"),
        Recovery::NotDrained => "not_drained".to_owned(),
    };
    format!(
        "old binary recovery: service: {}; supervisor: {}",
        outcome(service),
        outcome(supervisor)
    )
}

/// Start the service of the binary put in place at `target` for the queue
/// at `db`, taking over from the one the build migrated with: what the
/// report says of it ([`in_place_report`]). It fails nothing, as the binary
/// is in place and the queue migrated.
fn serve_in_place(binaries: &dyn Binaries, target: &Path, db: &Path) -> Value {
    match binaries.serve(target, db, false) {
        Ok(_) => in_place_report(None, true),
        Err(error) => in_place_report(Some(format!("{error:#}")), binaries.service_runs(db)),
    }
}

/// What the report says of the start of the service of the binary put in
/// place: `failed`, why it failed (`None` when it started), and whether a
/// service of the queue answers after (`runs`). A start may fail after it
/// stopped the service there, leaving none.
fn in_place_report(failed: Option<String>, runs: bool) -> Value {
    match failed {
        None => json!({"outcome": "replaced"}),
        Some(error) if runs => json!({
            "outcome": "kept",
            "error": error,
            "next": "a service of the queue still answers; `up` starts the one in place",
        }),
        Some(error) => json!({
            "outcome": "stopped",
            "error": error,
            "next": "no service of the queue answers: a supervisor's watch starts it again, else run `up`",
        }),
    }
}

/// The arguments of the `up` that starts the supervisors in `live` again
/// after the drain, on the queue at `db`: what [`install_breaking`] says it
/// carries over, the parallelism from `drained` (the first of them), then
/// `restart`.
fn restart_arguments(
    db: String,
    drained: &SupervisorRegistration,
    live: &[SupervisorRegistration],
    restart: &[String],
) -> Vec<String> {
    let flagged =
        |source: Option<SettingSource>| source.is_none_or(|source| source == SettingSource::Flag);
    let mut arguments = vec!["--db".to_owned(), db, "up".to_owned()];
    if live
        .iter()
        .any(|registration| registration.claude_disabled())
        && !restart.iter().any(|argument| argument == "--no-claude")
    {
        arguments.push("--no-claude".into());
    }
    if flagged(drained.parallel_source) {
        arguments.extend(["--parallel".to_owned(), drained.parallel.to_string()]);
    }
    // `--flag value` and `--flag=value` alike.
    let restarts = |flag: &str| {
        restart.iter().any(|argument| {
            argument
                .strip_prefix(flag)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
        })
    };
    if live.iter().any(|registration| registration.auto_update) && !restarts("--auto-update") {
        arguments.push("--auto-update".to_owned());
    }
    if let Some(max_waiting) = live
        .iter()
        .filter(|registration| flagged(registration.max_waiting_source))
        .find_map(|registration| registration.max_waiting)
        && !restarts("--max-waiting")
    {
        arguments.extend(["--max-waiting".to_owned(), max_waiting.to_string()]);
    }
    if let Some(runtime_planners) = live
        .iter()
        .filter(|registration| flagged(registration.runtime_planners_source))
        .find_map(|registration| registration.runtime_planners)
        && !restarts("--runtime-planners")
    {
        arguments.extend([
            "--runtime-planners".to_owned(),
            runtime_planners.to_string(),
        ]);
    }
    if let Some(max_load) = live.iter().find_map(SupervisorRegistration::flag_max_load)
        && !restarts("--max-load")
    {
        arguments.extend(["--max-load".to_owned(), max_load.to_string()]);
    }
    arguments.extend(restart.iter().cloned());
    arguments
}

/// The registrations whose process lives and heartbeats: the ones a
/// replacement asks (a silent one cannot pick the request up, ADR-0045
/// decision 16).
fn live_supervisors(
    clock: &dyn Clock,
    processes: &dyn ProcessControl,
    queue: &(impl SupervisorRegistry + ?Sized),
) -> Result<Vec<SupervisorRegistration>> {
    let now = clock.now();
    Ok(queue
        .supervisors()?
        .into_iter()
        .filter(|registration| {
            processes.alive(registration.pid)
                && now - registration.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
        })
        .collect())
}

/// The version `dagq --version` printed: its last word.
pub fn parse_version(output: &str) -> Result<String> {
    match output.split_whitespace().last() {
        Some(version) => Ok(version.to_owned()),
        None => bail!("no version in {output:?}"),
    }
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::application::port_fakes::SupervisorsAndEvents;
    use std::time::UNIX_EPOCH;

    /// Decision cases have literal expectations, independent of the wiring.
    #[test]
    fn recovery_requires_a_service_and_a_drained_registration() {
        for (service, drained, expected) in [
            (Recovery::Started, true, SupervisorRecovery::Start),
            (Recovery::AlreadyRunning, true, SupervisorRecovery::Start),
            (
                Recovery::Failed("no socket".into()),
                true,
                SupervisorRecovery::ServiceFailed,
            ),
            (Recovery::Started, false, SupervisorRecovery::NotDrained),
            (
                Recovery::Failed("no socket".into()),
                false,
                SupervisorRecovery::NotDrained,
            ),
        ] {
            assert_eq!(recovery_supervisor(&service, drained), expected);
        }
        for (service, supervisor, expected) in [
            (
                Recovery::Started,
                Recovery::Started,
                "old binary recovery: service: started; supervisor: started",
            ),
            (
                Recovery::AlreadyRunning,
                Recovery::Failed("up failed".into()),
                "old binary recovery: service: already_running; supervisor: failed: up failed",
            ),
            (
                Recovery::Failed("no socket".into()),
                Recovery::NotDrained,
                "old binary recovery: service: failed: no socket; supervisor: not_drained",
            ),
        ] {
            assert_eq!(recovery_message(&service, &supervisor), expected);
        }
    }

    /// The install entry drains a service-aware queue, then restores the old
    /// service before up. Direct recovery also supports a service left alive.
    #[test]
    fn a_refused_breaking_install_restores_the_old_service_before_up() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        for (drain, service_fails, up_fails, already_running, refuses) in [
            (true, false, false, false, true),
            (true, true, false, false, true),
            (true, false, true, false, true),
            (false, false, false, false, true),
            (true, false, false, true, true),
            // Post-replacement failures keep the installed binary.
            (true, false, true, false, false),
        ] {
            let live = if drain {
                vec![SupervisorRegistration {
                    auto_update: true,
                    max_waiting: Some(3),
                    runtime_planners: Some(4),
                    max_load: Some(6.0),
                    max_load_source: Some(SettingSource::Flag),
                    ..registration("first", 1, 10_000)
                }]
            } else {
                vec![]
            };
            let steps = Arc::new(InstallSteps {
                registrations: std::sync::Mutex::new(live.clone()),
                schema: Some(SchemaCheck {
                    pending: vec![PendingMigration {
                        version: 117,
                        compatible: false,
                    }],
                    opens: true,
                }),
                breaking_failure: refuses.then(|| "queue is in use by run active".into()),
                restart_failures: AtomicUsize::new(usize::from(service_fails)),
                up_fails,
                ..Default::default()
            });
            let files = crate::application::memory_files::MemoryFiles::default();
            files.put(Path::new("/queue"), UNIX_EPOCH, "queue");
            files.put(Path::new("/to"), UNIX_EPOCH, "old");
            let queues =
                |_: &Path| -> Arc<dyn QueueOpener> { Arc::new(InstallQueue(steps.clone())) };
            let down = || {
                steps.note("down");
                steps.service_running.store(false, SeqCst);
                // This queue really becomes unreachable after the drain.
                let error = match queues(Path::new("/queue")).open() {
                    Ok(_) => panic!("drain must stop the service"),
                    Err(error) => error,
                };
                assert!(super::super::supervisor_service::unreachable(&error));
                Ok(json!({"drained": true}))
            };
            let ports = Ports {
                binaries: &*steps,
                files: &files,
                processes: &OnlyOne,
                clock: &At(10_000),
                queues: &queues,
                down: &down,
            };
            let options = InstallOptions {
                source: Source::Binary("/source".into()),
                target: "/to".into(),
                allow_breaking: true,
                restart: vec!["--max-waiting=9".into()],
                handoff_timeout: Duration::ZERO,
                poll: Duration::ZERO,
                e2e: E2eGate::NotApplicable,
            };
            let error = if already_running {
                restore_before_replace(
                    &ports,
                    Path::new("/queue"),
                    &live,
                    &options,
                    anyhow::anyhow!("queue is in use by run active"),
                )
            } else {
                // No live supervisor and no answering service: no down is needed.
                if !drain {
                    steps.service_running.store(false, SeqCst);
                }
                install(&ports, Some(Path::new("/queue")), &options).unwrap_err()
            };
            let text = error.to_string();
            let mut expected = if already_running {
                vec![]
            } else if drain {
                vec!["open", "down", "open", "serve_breaking /source"]
            } else {
                vec!["open", "serve_breaking /source"]
            };
            if !already_running {
                if !refuses {
                    expected.extend(["replace", "adopt"]);
                }
                expected.push("serve /to");
            }
            if drain && !service_fails {
                expected.push("run /to --db /queue up --parallel 2 --auto-update --runtime-planners 4 --max-load 6 --max-waiting=9");
            }
            assert_eq!(*steps.calls.lock().unwrap(), expected);
            if !refuses {
                assert!(
                    text.contains("is installed and the queue migrated"),
                    "{text}"
                );
                assert!(!text.contains("old binary recovery"), "{text}");
                continue;
            }
            assert!(text.contains("queue is in use by run active"), "{text}");
            let service = if already_running {
                "already_running"
            } else if service_fails {
                "failed: restart failed"
            } else {
                "started"
            };
            let supervisor = if !drain {
                "not_drained"
            } else if service_fails {
                "failed: not attempted because the queue service failed"
            } else if up_fails {
                "failed: up failed"
            } else {
                "started"
            };
            assert!(
                text.contains(&format!("service: {service}; supervisor: {supervisor}")),
                "{text}"
            );
        }
    }

    /// Every queue of the host takes the one e2e lock in dagq's data
    /// directory, the parent of the queue's directory (ADR-t1233-2
    /// decision 4); a queue directory with no parent takes none.
    #[test]
    fn the_e2e_lock_is_the_data_directorys_for_every_queue() {
        assert_eq!(
            e2e_lock_path(Path::new("/data/dagq/abc")),
            Some(PathBuf::from("/data/dagq/e2e.lock"))
        );
        assert_eq!(
            e2e_lock_path(Path::new("/data/dagq/def")),
            e2e_lock_path(Path::new("/data/dagq/abc"))
        );
        assert_eq!(e2e_lock_path(Path::new("/")), None);
    }

    /// A replacement asks only the registrations whose process lives and
    /// whose heartbeat is at most the timeout old: one heartbeating just
    /// at the timeout is live, one a second past it is silent, and a dead
    /// pid is never live, read from the registrations' port alone.
    #[test]
    fn live_supervisors_are_the_alive_ones_heartbeating_within_the_timeout() {
        let now = 10_000;
        let queue = SupervisorsAndEvents {
            registrations: vec![
                registration("at-timeout", 1, now - HEARTBEAT_TIMEOUT_SECS),
                registration("past-timeout", 1, now - HEARTBEAT_TIMEOUT_SECS - 1),
                registration("dead", 2, now),
            ],
            ..SupervisorsAndEvents::default()
        };
        let live = live_supervisors(&At(now), &OnlyOne, &queue).unwrap();
        let tokens: Vec<&str> = live
            .iter()
            .map(|registration| registration.token.as_str())
            .collect();
        assert_eq!(tokens, ["at-timeout"]);
    }

    /// The `up` after the drain carries `--max-load` over only from a
    /// registration that took it from the flag, as `0` when the flag
    /// disabled the hold, and leaves it to the restart's arguments when
    /// they name it, as `--max-load N` or `--max-load=N`.
    #[test]
    fn the_restart_carries_over_only_a_flag_max_load() {
        use crate::domain::slot_limits::SettingSource;
        let loaded = |max_load, max_load_source| SupervisorRegistration {
            parallel_source: Some(SettingSource::Default),
            max_load,
            max_load_source,
            ..registration("live", 1, 0)
        };
        let up = |live: &[SupervisorRegistration], restart: &[&str]| {
            let restart: Vec<String> = restart.iter().map(|&a| a.to_owned()).collect();
            restart_arguments("/q.db".into(), &live[0], live, &restart)
        };
        let base = ["--db", "/q.db", "up"];
        assert_eq!(
            up(&[loaded(Some(6.0), Some(SettingSource::Flag))], &[]),
            [&base[..], &["--max-load", "6"]].concat()
        );
        assert_eq!(
            up(&[loaded(Some(2.5), Some(SettingSource::Flag))], &[]),
            [&base[..], &["--max-load", "2.5"]].concat()
        );
        assert_eq!(
            up(&[loaded(None, Some(SettingSource::Flag))], &[]),
            [&base[..], &["--max-load", "0"]].concat()
        );
        assert_eq!(
            up(&[loaded(Some(16.0), Some(SettingSource::Default))], &[]),
            base
        );
        assert_eq!(up(&[loaded(Some(16.0), None)], &[]), base);
        // The restart's value wins and is not doubled.
        assert_eq!(
            up(
                &[loaded(Some(6.0), Some(SettingSource::Flag))],
                &["--max-load", "9"]
            ),
            [&base[..], &["--max-load", "9"]].concat()
        );
        assert_eq!(
            up(
                &[loaded(Some(6.0), Some(SettingSource::Flag))],
                &["--max-load=9"]
            ),
            [&base[..], &["--max-load=9"]].concat()
        );
        // A longer flag that starts with the same text is not the flag.
        assert_eq!(
            up(
                &[loaded(Some(6.0), Some(SettingSource::Flag))],
                &["--max-loader"]
            ),
            [&base[..], &["--max-load", "6", "--max-loader"]].concat()
        );
        // Any drained registration's flag value, not only the first's.
        assert_eq!(
            up(
                &[
                    loaded(Some(16.0), Some(SettingSource::Default)),
                    loaded(Some(4.0), Some(SettingSource::Flag))
                ],
                &[]
            ),
            [&base[..], &["--max-load", "4"]].concat()
        );
    }

    /// The report of the service of the binary put in place: started, or
    /// failed with a service still answering (kept) or with none left
    /// (stopped, its start having stopped the one there).
    #[test]
    fn the_report_says_whether_a_service_answers_after_the_one_in_place_failed() {
        assert_eq!(in_place_report(None, false), json!({"outcome": "replaced"}));
        let kept = in_place_report(Some("no answer".into()), true);
        assert_eq!(kept["outcome"], "kept");
        assert_eq!(kept["error"], "no answer");
        let stopped = in_place_report(Some("no answer".into()), false);
        assert_eq!(stopped["outcome"], "stopped");
        assert!(
            stopped["next"].as_str().unwrap().contains("run `up`"),
            "{stopped}"
        );
    }

    #[test]
    fn without_from_only_dagqs_source_is_built() {
        let checkout = PathBuf::from("/repo");
        assert!(matches!(
            Source::default_checkout(checkout.clone(), true).unwrap(),
            Source::Checkout(path) if path == checkout
        ));
        let error = Source::default_checkout(checkout, false)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("/repo is not dagq's source")
                && error.contains("cargo install dagq")
                && error.contains("--from"),
            "{error}"
        );
    }
    /// Exercise the real install entry points, including Undo after every
    /// handoff failed. Human rollback must never consult the job gate.
    #[test]
    fn only_job_installs_gate_the_source_and_the_previous_build() {
        for (job, rollback, unsupported, restored) in [
            (true, false, "/source", false),
            (true, false, "/to.previous", false),
            (true, false, "/unrelated", true),
            (false, false, "/source", true),
            (false, true, "/to.previous", true),
        ] {
            let steps = Arc::new(InstallSteps {
                handoff: Handoff::Failed,
                unsupported_job: Some(unsupported.into()),
                ..Default::default()
            });
            let files = crate::application::memory_files::MemoryFiles::default();
            files.put(Path::new("/queue"), UNIX_EPOCH, "queue");
            files.put(Path::new("/to"), UNIX_EPOCH, "old");
            files.put(Path::new("/to.previous"), UNIX_EPOCH, "previous");
            let queues =
                |_: &Path| -> Arc<dyn QueueOpener> { Arc::new(InstallQueue(steps.clone())) };
            let ports = Ports {
                binaries: &*steps,
                files: &files,
                processes: &OnlyOne,
                clock: &At(10_000),
                queues: &queues,
                down: &|| bail!("no drain"),
            };
            let options = InstallOptions {
                source: if rollback {
                    Source::Rollback
                } else {
                    Source::Binary("/source".into())
                },
                target: "/to".into(),
                allow_breaking: false,
                restart: vec![],
                handoff_timeout: Duration::ZERO,
                poll: Duration::ZERO,
                e2e: E2eGate::NotApplicable,
            };
            let error = if job {
                install_by_update_job(&ports, Some(Path::new("/queue")), &options)
            } else {
                install(&ports, Some(Path::new("/queue")), &options)
            }
            .unwrap_err();
            let calls = steps.calls.lock().unwrap();
            assert_eq!(calls.iter().any(|c| c == "restore"), restored, "{calls:?}");
            if job && unsupported == "/source" {
                assert!(
                    calls.is_empty(),
                    "source refused before any side effects: {calls:?}"
                );
                assert!(
                    error
                        .to_string()
                        .contains("does not accept the update job's principal")
                );
            } else {
                let report = &HandoffFailed::of(&error).expect("handoff failed").restored;
                assert_eq!(report["restored"], restored);
                if !restored {
                    assert!(
                        report["reason"]
                            .as_str()
                            .unwrap()
                            .contains("does not accept")
                    );
                    assert!(
                        report["reason"]
                            .as_str()
                            .unwrap()
                            .contains("the previous build was left unrestored")
                    );
                    assert!(
                        report["reason"]
                            .as_str()
                            .unwrap()
                            .contains("install --rollback")
                    );
                }
            }
            if !job {
                assert!(steps.job_checks.lock().unwrap().is_empty());
            }
        }
    }

    /// Start includes an unread schema as well as pending migrations. Open
    /// can still require a replacement with start=false. Failures must not
    /// introduce a service restart after handoff, nor change adopt/restore.
    #[test]
    fn the_service_in_place_starts_before_handoff_only_when_install_served() {
        for (pending, unread, fallback, in_place, failures, runs, handoff) in [
            (true, false, None, false, 0, true, Handoff::Taken),
            (false, true, None, false, 0, true, Handoff::Taken),
            (
                false,
                false,
                Some(OpenFailure::Unknown),
                false,
                0,
                true,
                Handoff::Taken,
            ),
            (
                false,
                false,
                Some(OpenFailure::Unreachable),
                false,
                0,
                true,
                Handoff::Taken,
            ),
            (true, false, None, false, 1, true, Handoff::Taken),
            (true, false, None, false, 1, false, Handoff::Taken),
            (true, false, None, false, 2, false, Handoff::Taken),
            (true, false, None, false, 0, true, Handoff::Failed),
            (true, false, None, false, 0, true, Handoff::Partial),
            (true, false, None, false, 0, true, Handoff::Unsettled),
            (false, false, None, false, 0, true, Handoff::Taken),
            (true, false, None, true, 0, true, Handoff::Taken),
        ] {
            let steps = Arc::new(InstallSteps {
                calls: Default::default(),
                registrations: std::sync::Mutex::new(if matches!(handoff, Handoff::Partial) {
                    vec![
                        registration("first", 1, 10_000),
                        registration("second", 1, 10_000),
                    ]
                } else {
                    vec![registration("first", 1, 10_000)]
                }),
                first_open: std::sync::Mutex::new(fallback),
                schema: (!unread).then(|| SchemaCheck {
                    pending: if pending {
                        vec![PendingMigration {
                            version: 1,
                            compatible: true,
                        }]
                    } else {
                        vec![]
                    },
                    opens: true,
                }),
                handoff,
                restart_failures: std::sync::atomic::AtomicUsize::new(failures),
                in_place,
                survives_failure: runs,
                service_running: Arc::new(std::sync::atomic::AtomicBool::new(!matches!(
                    fallback,
                    Some(OpenFailure::Unreachable)
                ))),
                ..InstallSteps::default()
            });
            let files = crate::application::memory_files::MemoryFiles::default();
            files.put(Path::new("/queue"), UNIX_EPOCH, "queue");
            files.put(Path::new("/to"), UNIX_EPOCH, "old");
            let queues =
                |_: &Path| -> Arc<dyn QueueOpener> { Arc::new(InstallQueue(steps.clone())) };
            let options = InstallOptions {
                source: Source::Binary(PathBuf::from(if in_place { "/to" } else { "/source" })),
                target: PathBuf::from("/to"),
                allow_breaking: false,
                restart: vec![],
                handoff_timeout: Duration::ZERO,
                poll: Duration::ZERO,
                e2e: E2eGate::NotApplicable,
            };
            let result = install(
                &Ports {
                    binaries: &*steps,
                    files: &files,
                    processes: &OnlyOne,
                    clock: &At(10_000),
                    queues: &queues,
                    down: &|| bail!("no drain"),
                },
                Some(Path::new("/queue")),
                &options,
            );
            let served = pending || unread || fallback.is_some();
            let mut expected = if pending || unread {
                vec![
                    "hold",
                    if in_place {
                        "serve /to"
                    } else {
                        "serve /source"
                    },
                    "open",
                ]
            } else if fallback.is_some() {
                vec!["open", "hold", "serve /source", "open"]
            } else {
                vec!["open"]
            };
            if !in_place {
                expected.push("replace");
            }
            if served && !in_place {
                expected.push("serve /to");
            }
            let stopped = failures == 2 && !runs;
            if failures > 0 && !runs {
                expected.push("serve /to");
            }
            if stopped {
                expected.push("restore");
            } else {
                expected.push("hand_off");
                match handoff {
                    Handoff::Taken => expected.push("adopt"),
                    Handoff::Failed => expected.push("restore"),
                    Handoff::Partial => expected.extend(["hand_off", "adopt"]),
                    Handoff::Unsettled => {}
                }
            }
            assert_eq!(
                *steps.calls.lock().unwrap(),
                expected,
                "{pending} {unread} {fallback:?} {in_place} {failures} {runs} {handoff:?}"
            );
            if stopped {
                let error = result.unwrap_err();
                assert!(error.to_string().contains("could not start before handoff"));
                assert!(!error.to_string().contains("the handoff to"));
                let report = &super::super::update::InstallFailed::of(&error)
                    .unwrap()
                    .report;
                assert_eq!(
                    report["queue_service"],
                    json!({
                        "outcome": "stopped",
                        "error": "restart failed",
                        "next": "no service of the queue answers: a supervisor's watch starts it again, else run `up`",
                    })
                );
                assert_eq!(
                    report["restored"],
                    json!({"restored": true, "version": "old"})
                );
                assert_eq!(report["supervisors"], json!([]));
                assert_eq!(report["outcome"], "failed");
                continue;
            }
            let report = match handoff {
                Handoff::Taken => result.unwrap(),
                Handoff::Partial => KeptBinary::of(&result.unwrap_err()).unwrap().report.clone(),
                Handoff::Failed => {
                    assert_eq!(
                        HandoffFailed::of(&result.unwrap_err()).unwrap().restored["restored"],
                        true
                    );
                    continue;
                }
                Handoff::Unsettled => {
                    assert!(super::super::lifecycle::HandoffUnsettled::is(
                        &result.unwrap_err()
                    ));
                    continue;
                }
            };
            if served && !in_place {
                assert_eq!(
                    report["queue_service"]["outcome"],
                    if failures > 0 && runs {
                        "kept"
                    } else {
                        "replaced"
                    }
                );
                if failures > 0 && runs {
                    assert_eq!(report["queue_service"]["error"], "restart failed");
                }
            } else {
                assert!(report.get("queue_service").is_none());
            }
        }
    }

    /// The watched entry must return the stopped-service report directly:
    /// there is no receiver for a handoff, recovery, or a failure record.
    #[test]
    fn a_stopped_service_report_leaves_install_watched_without_more_queue_calls() {
        use super::super::supervisor_service::{ServiceCaller, undone, unreachable};
        use super::super::update::{self, InstallFailed, JobPorts};
        let steps = Arc::new(InstallSteps {
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
            restart_failures: std::sync::atomic::AtomicUsize::new(2),
            in_place: false,
            survives_failure: false,
            service_running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ..InstallSteps::default()
        });
        let files = crate::application::memory_files::MemoryFiles::default();
        files.put(Path::new("/queue"), UNIX_EPOCH, "queue");
        files.put(Path::new("/to"), UNIX_EPOCH, "old");
        let queues = |_: &Path| -> Arc<dyn QueueOpener> { Arc::new(InstallQueue(steps.clone())) };
        let options = InstallOptions {
            source: Source::Binary(PathBuf::from("/source")),
            target: PathBuf::from("/to"),
            allow_breaking: false,
            restart: vec![],
            handoff_timeout: Duration::ZERO,
            poll: Duration::ZERO,
            e2e: E2eGate::NotApplicable,
        };
        let error = update::install_watched(
            &JobPorts {
                binaries: &*steps,
                files: &files,
                processes: &OnlyOne,
                clock: &At(10_000),
                queues: &queues,
                restart: &|_| bail!("no supervisor was handed over"),
                is_ancestor: &|_, _| bail!("no checkout is built"),
            },
            &|| bail!("no drain"),
            Some(Path::new("/queue")),
            &options,
            Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(
            *steps.calls.lock().unwrap(),
            [
                "open",
                "hold",
                "serve /source",
                "open",
                "replace",
                "serve /to",
                "serve /to",
                "restore",
            ]
        );
        let report = &InstallFailed::of(&error).unwrap().report;
        assert_eq!(report["queue_service"]["outcome"], "stopped");
        assert_eq!(report["queue_service"]["error"], "restart failed");
        assert_eq!(report["restored"]["restored"], true);
        // The same fake would reject a handoff if install sent one while
        // stopped, rather than granting it as though the service existed.
        let unanswered = steps
            .call(
                crate::domain::queue_service::UseCase::Supervisor,
                json!({"op": "request_handoff", "args": ["first", "/to"]}),
            )
            .unwrap_err();
        assert!(unreachable(&unanswered));
        assert!(undone(&unanswered));
    }
}
