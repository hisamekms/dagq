//! `dagq install`: a person's way to replace the fixed binary and the
//! queue's supervisor with it, without waiting for the sessions (ADR-0045
//! decisions 11–14). The new binary is built (or given), checked, the
//! queue's compatible migrations applied with it, and put in place of the
//! old one by a rename that keeps the old one as `<name>.previous`; then
//! every live supervisor that takes a handoff is asked to exec it. The old
//! binary goes back only when none of them took it; when some did, it stays
//! and the ones that did not are named in a [`KeptBinary`] error
//! (ADR-t632-1). A build
//! whose migrations would break the old binary goes through the drain
//! instead, and only when asked to. `--rollback` does the same with
//! `<name>.previous`.
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
    /// Start `binary` on a throwaway queue (`init` and a read).
    fn probe(&self, binary: &Path) -> Result<()>;
    /// Whether `binary` continues a supervisor after an exec (it knows
    /// `supervise --handoff-token`); a binary before ADR-0045 exits on it.
    fn takes_handoff(&self, binary: &Path) -> bool;
    /// `binary --db <db> migrate --check`.
    fn schema(&self, binary: &Path, db: &Path) -> Result<SchemaCheck>;
    /// `binary --db <db> migrate`, its report.
    fn migrate(&self, binary: &Path, db: &Path) -> Result<Value>;
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
    /// Build `checkout` for release into `target_dir`, running `command`
    /// (a shell command) in place of `cargo build --release --locked -p
    /// dagq` when given, with what it prints appended to `log`; the binary
    /// built.
    fn build_into(
        &self,
        checkout: &Path,
        target_dir: &Path,
        command: Option<&str>,
        log: &Path,
    ) -> Result<PathBuf> {
        let _ = (checkout, target_dir, command, log);
        bail!("these binaries cannot build a checkout")
    }
    /// Run the e2e of `checkout` (ADR-t963-1 decision 1) with
    /// `CARGO_TARGET_DIR` set to `target_dir` when given, and clean up what
    /// it left in cmux and on disk; how it went. An error is an e2e that
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

/// How long the gate's e2e may run by default before it counts as failed
/// (ADR-t963-1 decision 1).
pub const E2E_TIMEOUT: Duration = Duration::from_secs(1800);

/// The e2e a build of dagq's source passes before it is put in place
/// (ADR-t963-1 decision 1): `cargo test --locked --test e2e -- --ignored`
/// in the checkout.
#[derive(Debug, Clone)]
pub struct E2eSettings {
    /// A shell command in place of `cargo test --locked --test e2e --
    /// --ignored` (tests).
    pub command: Option<String>,
    /// How long it may run; past it the e2e is stopped and failed.
    pub timeout: Duration,
    /// The cmux the e2e drives (`DAGQ_E2E_CMUX`): pinged before it starts,
    /// and the groups the e2e left in it are deleted after. When it does
    /// not answer, the tests that need it ([`CMUX_E2E`]) are not run and
    /// its cleanup is left to a later gate (ADR-t2105-1).
    pub cmux: Option<PathBuf>,
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

/// The e2e tests that need a running cmux, the `up` / `down` ones that open
/// the inbox's workspace (`fixture_with_cmux` of `tests/e2e.rs`): the
/// `--skip` filters of the tests not run when cmux does not answer `ping`
/// (ADR-t2105-1). The others need no cmux (ADR-t1433-1).
pub const CMUX_E2E: &[&str] =
    &["up_starts_a_launchd_supervisor_that_status_lists_and_down_wait_stops_it"];

/// The e2e tests the gate did not run, and why: those that need cmux when
/// it does not answer (ADR-t2105-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct E2eSkip {
    /// The `--skip` filters of the tests not run.
    pub tests: Vec<String>,
    /// Why (`cmux did not answer: ...`).
    pub reason: String,
}

impl E2eSkip {
    /// The cmux tests ([`CMUX_E2E`]), not run because cmux did not answer
    /// `ping` for `error`.
    pub fn cmux(error: &str) -> Self {
        Self {
            tests: CMUX_E2E.iter().map(|test| (*test).to_owned()).collect(),
            reason: format!("cmux did not answer: {error}"),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"tests": self.tests, "reason": self.reason})
    }

    /// A sentence for a person: which tests were not run and why.
    pub fn sentence(&self) -> String {
        format!(
            "the e2e did not run {} because {}",
            self.tests.join(", "),
            self.reason
        )
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
    /// What was cleaned up after it (groups, processes, the directory).
    pub cleanup: Value,
    /// The tests not run because cmux did not answer (ADR-t2105-1).
    pub skipped: Option<E2eSkip>,
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

    /// The `e2e` of a report: `passed` with its time, and the tests it did
    /// not run (`skipped`) when there are any.
    pub fn report(&self, settings: &E2eSettings) -> Value {
        let mut report = json!({"status": "passed", "secs": self.secs, "log": settings.log});
        if let Some(skipped) = &self.skipped {
            report["skipped"] = skipped.to_json();
        }
        report
    }
}

/// Whether `install` runs the e2e of a checkout it builds (ADR-t963-1
/// decision 1). A binary, a rollback and a release have no gate.
// Made once per `install`; boxing the settings buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum E2eGate {
    /// The source has none (not dagq's source, or a built binary).
    NotApplicable,
    /// A person skipped it (`--skip-e2e`).
    Skip,
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
    /// The binary the last install replaced (`<target>.previous`).
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
    /// supervisor): `--cmux`, `--claude`, `--plugin-dir`. The `up` starts
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
pub fn install(
    ports: &Ports<'_, impl HostOpsQueue + ?Sized>,
    db: Option<&Path>,
    options: &InstallOptions,
) -> Result<Value> {
    let Ports {
        binaries, files, ..
    } = *ports;
    let target = &options.target;
    let previous = previous_path(target);
    let mut e2e = json!({"status": "not_applicable"});
    let source = match &options.source {
        Source::Checkout(checkout) => {
            let built = binaries
                .build(checkout)
                .with_context(|| format!("build {}", checkout.display()))?;
            e2e = match &options.e2e {
                E2eGate::NotApplicable => json!({"status": "not_applicable"}),
                E2eGate::Skip => json!({"status": "skipped"}),
                E2eGate::Run(settings) => {
                    // The build's own target (`<target>/release/dagq`): the
                    // e2e is not given the shell's `CARGO_TARGET_DIR`, so it
                    // is named for it to build no second time.
                    let target_dir = built.parent().and_then(Path::parent);
                    let outcome = binaries.e2e(checkout, target_dir, settings).with_context(
                        || {
                            format!(
                                "the e2e of {} could not start, so nothing was replaced (a person \
may pass --skip-e2e to install without it)",
                                checkout.display()
                            )
                        },
                    )?;
                    // The automatic update's gates before this one, for a
                    // marked test failing in a row (ADR-t1165-1); a queue
                    // that cannot be read has none.
                    let history = db
                        .filter(|db| files.is_file(db))
                        .and_then(|db| (ports.queues)(db).open().ok())
                        .and_then(|queue| queue.e2e_gate_events(super::e2e_verdict::HISTORY).ok())
                        .unwrap_or_default();
                    let verdict =
                        super::e2e_verdict::judge(&outcome, settings, &history, ports.clock.now());
                    if let Some(failure) = &verdict.failure {
                        bail!(
                            "{failure}: nothing was replaced (a person may pass --skip-e2e to \
install without it)"
                        );
                    }
                    let mut report = outcome.report(settings);
                    if let (Some(report), Some(fields)) =
                        (report.as_object_mut(), verdict.fields.as_object())
                    {
                        report.extend(fields.clone());
                    }
                    report
                }
            };
            built
        }
        Source::Binary(binary) => binary.clone(),
        Source::Rollback => {
            ensure!(
                files.is_file(&previous),
                "no previous binary at {} to roll back to",
                previous.display()
            );
            previous.clone()
        }
    };
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
    let schema = binaries.schema(&source, db)?;
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
    let migrated = if schema.pending.is_empty() {
        Value::Null
    } else {
        binaries.migrate(&source, db)?
    };
    let queue = (ports.queues)(db).open()?;
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
        let undo = |error, supervisors| {
            Undo {
                binaries,
                target,
                previous: &previous,
                version: &version,
                replaced_version: replaced_version.as_deref(),
                in_place,
            }
            .error(error, supervisors)
        };
        match handed {
            // The binary is one file for every supervisor: it goes back
            // only when none of them took it (ADR-t632-1).
            Ok(handed) if handed.iter().all(|h| h.error.is_some()) => {
                let error = handoff_failures(&handed)
                    .unwrap_or_else(|| anyhow::anyhow!("no supervisor took the handoff"));
                return Err(undo(
                    error,
                    Some(handed.iter().map(Handed::report).collect()),
                ));
            }
            Ok(handed) => handed,
            Err(error) => return Err(undo(error, None)),
        }
    };
    let failure = handoff_failures(&handed);
    let report = json!({
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

/// What an install undoes when its handoff failed for every supervisor.
struct Undo<'a> {
    binaries: &'a dyn Binaries,
    target: &'a Path,
    previous: &'a Path,
    version: &'a str,
    replaced_version: Option<&'a str>,
    in_place: bool,
}

impl Undo<'_> {
    /// Put the binary it replaced back (unless it was this build already)
    /// and say so around `error`; with each supervisor's outcome, as a
    /// [`HandoffFailed`].
    fn error(&self, error: anyhow::Error, supervisors: Option<Vec<Value>>) -> anyhow::Error {
        let Self {
            target,
            previous,
            version,
            ..
        } = *self;
        let (error, restored) = if self.in_place {
            let context = format!(
                "the handoff to {version} failed; {} was {version} already and stays",
                target.display()
            );
            (
                error.context(context.clone()),
                json!({"restored": false, "reason": context}),
            )
        } else {
            match self.binaries.restore(target) {
                Ok(()) => (
                    error.context(format!(
                        "the handoff to {version} failed; the binary it replaced is back at {}",
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
                            "the handoff to {version} failed, and the binary it replaced could \
not be put back at {} ({reason}); it is at {}",
                            target.display(),
                            previous.display()
                        )),
                        json!({"restored": false, "reason": reason}),
                    )
                }
            }
        };
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
/// for their runs (`down --wait`), migrate with the new binary (which
/// backs the queue up first), replace the binary, and start the supervisor
/// again with the new binary's `up`, in the mode and with the parallelism
/// the drained one had.
fn install_breaking(
    ports: &Ports<'_, impl HostOpsQueue + ?Sized>,
    db: &Path,
    source: &Path,
    version: &str,
    replaced_version: Option<String>,
    options: &InstallOptions,
) -> Result<Value> {
    let binaries = ports.binaries;
    let live = {
        let queue = (ports.queues)(db).open()?;
        live_supervisors(ports.clock, ports.processes, &*queue)?
    };
    let drained = if live.is_empty() {
        Value::Null
    } else {
        (ports.down)()?
    };
    let migrated = binaries.migrate(source, db).with_context(|| {
        format!(
            "the supervisor was drained but {version} could not migrate the queue; nothing was \
replaced, and `up` starts the old binary again"
        )
    })?;
    binaries.replace(source, &options.target)?;
    let started = match live.first() {
        None => Value::Null,
        Some(drained) => {
            // A value the drained supervisor took from a flag carries over
            // as that flag; one it took from `dagq.toml` or the default is
            // left for the started one to resolve again (task 698). A
            // registration of an older binary says nothing, and was given
            // its values.
            let flagged = |source: Option<SettingSource>| {
                source.is_none_or(|source| source == SettingSource::Flag)
            };
            let mut arguments = vec!["--db".to_owned(), path_text(db)?, "up".to_owned()];
            if live
                .iter()
                .any(|registration| registration.claude_disabled())
                && !options
                    .restart
                    .iter()
                    .any(|argument| argument == "--no-claude")
            {
                arguments.push("--no-claude".into());
            }
            if flagged(drained.parallel_source) {
                arguments.extend(["--parallel".to_owned(), drained.parallel.to_string()]);
            }
            // The drained supervisor's automatic update, wait limit and
            // limit of the runtime's planners carry over, so no second `up`
            // is needed to put them back.
            let restarts = |flag: &str| options.restart.iter().any(|argument| argument == flag);
            if live.iter().any(|registration| registration.auto_update)
                && !restarts("--auto-update")
            {
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
            arguments.extend(options.restart.iter().cloned());
            binaries.run(&options.target, &arguments).with_context(|| {
                format!(
                    "{version} is installed and the queue migrated, but its `up` failed; run it \
again"
                )
            })?
        }
    };
    Ok(json!({
        "outcome": "installed",
        "target": options.target,
        "version": version,
        "previous": previous_path(&options.target),
        "previous_version": replaced_version,
        "migrated": migrated,
        "drained": drained,
        "up": started,
    }))
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
mod tests {
    use super::*;
    use crate::application::port_fakes::SupervisorsAndEvents;
    use crate::domain::{LeaseToken, SupervisorMode};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// The clock at `secs` after the epoch.
    struct At(i64);

    impl Clock for At {
        fn system_time(&self) -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(self.0.unsigned_abs())
        }

        fn monotonic(&self) -> std::time::Instant {
            std::time::Instant::now()
        }
    }

    /// Pid 1 is alive, every other pid is dead.
    struct OnlyOne;

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

    fn registration(token: &str, pid: u32, heartbeat_at: i64) -> SupervisorRegistration {
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
            max_load: None,
            providers: None,
            binary_version: Some("1.0.0".into()),
        }
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

    /// The cmux skip names its tests and why, with the tool it waited for
    /// (ADR-t2105-1).
    #[test]
    fn the_skipped_e2e_name_their_tests_and_why() {
        let cmux = E2eSkip::cmux("`cmux ping` failed: Access denied");
        assert_eq!(cmux.tests, CMUX_E2E);
        assert_eq!(
            cmux.to_json(),
            json!({
                "tests": CMUX_E2E,
                "reason": "cmux did not answer: `cmux ping` failed: Access denied",
            })
        );
        assert_eq!(
            cmux.sentence(),
            format!(
                "the e2e did not run {} because cmux did not answer: `cmux ping` failed: Access \
denied",
                CMUX_E2E.join(", ")
            )
        );
    }

    /// Every e2e that takes the running cmux (`fixture_with_cmux()`) is
    /// among the filters skipped when cmux does not answer, so the gate
    /// never runs one that would fail for want of it (ADR-t2105-1).
    #[test]
    fn the_cmux_e2e_are_the_ones_that_take_the_running_cmux() {
        let source = include_str!("../../tests/e2e.rs");
        let mut taking = Vec::new();
        let mut current = None;
        for line in source.lines() {
            if let Some(rest) = line.strip_prefix("fn ") {
                current = rest.split('(').next();
            } else if line.contains("fixture_with_cmux()") && !line.trim_start().starts_with("//") {
                taking.extend(current);
            }
        }
        assert!(!taking.is_empty(), "no e2e takes fixture_with_cmux()");
        for test in taking {
            assert!(
                CMUX_E2E.iter().any(|filter| test.contains(filter)),
                "{test} takes the running cmux but is not in CMUX_E2E"
            );
        }
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
}
