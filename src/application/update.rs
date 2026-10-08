//! The automatic update of the fixed binary (ADR-0045 decision 17). A
//! supervisor registered with `auto_update` (`up --auto-update`) looks at
//! main on its passes; when main moved past the last commit it updated to
//! (or the commit its own build names) and the commits in between change
//! the runtime ([`RUNTIME_PATHS`]), it starts the update job ([`run`], the
//! hidden `auto-update` command) in a session of its own and goes on
//! supervising. The job builds that commit in the queue's own checkout
//! and target (`<queue dir>/update/`, never a person's checkout), runs the
//! e2e there and replaces nothing when it fails (ADR-t963-1 decision 1),
//! then does
//! what `install` does ([`super::install::install`]: the check, the
//! compatible migrations, the swap that keeps `.previous`, the handoff), and
//! watches the supervisors that exec'd the new binary heartbeat on. A build
//! that fails or a check that fails replaces nothing; when no supervisor
//! comes back under the new binary the old binary is put back, and when
//! only some do it stays for them (ADR-t632-1); either way a supervisor that
//! is gone is started again and the `update_failed` ask opens for the
//! inbox; a build whose migrations would break the old binary is not
//! installed, and waits in the `approve_update` ask for a person to drain
//! and install it. Every step is an `update_*` event of the queue in
//! `run_events` (ADR-0073 decision 17, task 496), with the commit it is
//! about in its payload, so `events`, `watch`, `timeline` and `stats` see
//! it; the job's own output goes to the queue's `logs/`.

use super::{
    Clock, InstalledPlugin, ProcessControl, Queue, QueueOpener, RunCoordination, RunFiles,
    install::{self, Binaries, E2eGate, E2eSettings, InstallOptions, Source, previous_path},
    lifecycle,
};
use crate::domain::{
    APPROVE_UPDATE_OPTIONS, AskKind, EventId, HEARTBEAT_TIMEOUT_SECS, RunEvent,
    SupervisorRegistration, UPDATE_FAILED_OPTIONS,
};
use crate::domain::{EventKind, LeaseToken};
pub use crate::domain::{
    INSTALL_SOURCE, UPDATE_ANSWERED, UPDATE_AWAITING_APPROVAL, UPDATE_BUILT, UPDATE_DROPPED,
    UPDATE_E2E_PASSED, UPDATE_FAILED, UPDATE_INSTALLED, UPDATE_RESTORED, UPDATE_RETRY,
    UPDATE_STARTED,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

/// How many of the latest `update_*` a look reads: enough to find the
/// failure an answered ask is about and the answers a release had.
pub const UPDATE_HISTORY: usize = 500;

/// `asked_by` of the update's asks.
pub const UPDATE_ASKER: &str = "supervisor";

/// The paths whose change makes a landing change the runtime, relative to
/// the repository root: a directory ends with `/`. `build.rs` embeds the
/// build identifier, and `rust-toolchain.toml` names the Rust dagq is
/// built with.
pub const RUNTIME_PATHS: &[&str] = &[
    "src/",
    "migrations/",
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "rust-toolchain.toml",
];

/// Whether any of `paths` (repository-relative) is part of the runtime.
pub fn changes_runtime(paths: &[String]) -> bool {
    paths.iter().any(|path| {
        RUNTIME_PATHS
            .iter()
            .any(|runtime| match runtime.strip_suffix('/') {
                Some(dir) => path.starts_with(runtime) || path == dir,
                None => path == runtime,
            })
    })
}

/// The commit a build identifier names: `<commit>` of
/// `X.Y.Z-dev+<commit>[.dirty]`. `None` for a release (`X.Y.Z`) or a build
/// that did not know its commit (`+unknown`).
pub fn build_commit(version: &str) -> Option<&str> {
    crate::build_id::named_commit(version)
}

/// Where the update keeps its checkout, target and a build waiting for a
/// person, under the queue's directory.
#[derive(Debug, Clone)]
pub struct UpdatePaths {
    /// `<queue dir>/update/checkout`: a detached worktree of the repository.
    pub checkout: PathBuf,
    /// `<queue dir>/update/target`: the checkout's `CARGO_TARGET_DIR`.
    pub target: PathBuf,
    /// `<queue dir>/update/staged/dagq`: a build with a breaking
    /// migration, kept for the `install` a person runs.
    pub staged: PathBuf,
    /// `<queue dir>/update/release`: the `--root` of the release update's
    /// `cargo install` (ADR-t618-1 decision 5).
    pub release: PathBuf,
    /// `<queue dir>/update/e2e`: where the gate's e2e makes its fixtures
    /// (ADR-t963-1 decision 1).
    pub e2e: PathBuf,
}

impl UpdatePaths {
    pub fn under(queue_dir: &Path) -> Self {
        let root = queue_dir.join("update");
        Self {
            checkout: root.join("checkout"),
            target: root.join("target"),
            staged: root.join("staged").join("dagq"),
            release: root.join("release"),
            e2e: root.join("e2e"),
        }
    }
}

/// Record one step of the automatic update: the queue event `kind` with
/// `commit` (the main commit it is about) in its payload.
pub fn record(
    queue: &dyn Queue,
    kind: EventKind,
    commit: Option<&str>,
    mut payload: Value,
) -> Result<EventId> {
    if let (Some(commit), Some(object)) = (commit, payload.as_object_mut()) {
        object.insert("commit".into(), json!(commit));
    }
    queue.record_queue_event(kind, payload)
}

/// The main commit a step is about, if it names one.
pub fn step_commit(update: &RunEvent) -> Option<&str> {
    update.payload.get("commit").and_then(Value::as_str)
}

/// The steps a job writes before the one that ends it: a job whose latest
/// step is one of these and whose process is gone was interrupted.
pub const JOB_STEPS: &[&str] = &[
    UPDATE_STARTED,
    UPDATE_BUILT,
    UPDATE_E2E_PASSED,
    UPDATE_RESTORED,
];

/// Whether `update` is a step of a job still working: one of
/// [`JOB_STEPS`] and the job's process lives.
pub fn in_progress(update: &RunEvent, processes: &dyn ProcessControl) -> bool {
    JOB_STEPS.contains(&update.kind.as_str())
        && job_pid(update).is_some_and(|pid| processes.alive(pid))
}

/// Whether `update` is the supervisor's word about an answer (an
/// `update_answered`, an `update_retry`, or an `update_dropped`), not a
/// step a job wrote.
fn is_answer_step(update: &RunEvent) -> bool {
    matches!(
        update.kind.as_str(),
        UPDATE_ANSWERED | UPDATE_RETRY | UPDATE_DROPPED
    )
}

/// The newest step a job wrote (`updates` newest first): the answers of
/// the asks (`update_answered`, `update_retry`, `update_dropped`) are
/// skipped, so an answer written while a job still works does not hide it.
pub fn latest_job_step(updates: &[RunEvent]) -> Option<&RunEvent> {
    updates
        .iter()
        .find(|update| !is_answer_step(update) && !step_install(update))
}

/// The `update_failed` that opened the ask `ask_id`, if it is among
/// `updates`.
pub fn failed_step(updates: &[RunEvent], ask_id: crate::domain::AskId) -> Option<&RunEvent> {
    updates.iter().find(|update| {
        update.kind == UPDATE_FAILED
            && update.payload.get("ask_id").and_then(Value::as_i64) == Some(ask_id.as_i64())
    })
}

/// The commit whose failed build opened the automatic update's
/// `update_failed` ask `ask_id`. `None` for a person's install's and a
/// release's failure, a step that names no commit, or one not among
/// `updates`: no swap is known to contain what failed then.
pub fn failed_commit(updates: &[RunEvent], ask_id: crate::domain::AskId) -> Option<&str> {
    failed_step(updates, ask_id)
        .filter(|step| step_release(step).is_none() && !step_install(step))
        .and_then(step_commit)
}

/// [`JobPorts::is_ancestor`] of a job that builds no commit of the
/// repository (a release's, a person's install): it never tells.
pub fn no_ancestry(_: &str, _: &str) -> Result<bool> {
    bail!("this job knows no repository")
}

/// The answer the runtime gives an open `update_failed` ask when the
/// automatic update put in place a commit that contains the failed one.
pub const UPDATE_INSTALLED_ANSWER: &str = "installed";

/// Which of the open `update_failed` asks `asks` a job's swap to
/// `installed` settles: those whose [`failed_commit`] `is_ancestor` says
/// `installed` is or descends from. An ask whose commit is unknown, or
/// that `installed` does not contain, is left; a failed ancestry check is
/// returned beside, for a warning, and leaves the ask too.
pub fn settled_failures(
    asks: &[crate::domain::AskId],
    updates: &[RunEvent],
    installed: &str,
    is_ancestor: &dyn Fn(&str, &str) -> Result<bool>,
) -> (
    Vec<crate::domain::AskId>,
    Vec<(crate::domain::AskId, anyhow::Error)>,
) {
    let mut settled = Vec::new();
    let mut unchecked = Vec::new();
    for &ask in asks {
        let Some(failed) = failed_commit(updates, ask) else {
            continue;
        };
        match is_ancestor(failed, installed) {
            Ok(true) => settled.push(ask),
            Ok(false) => {}
            Err(error) => unchecked.push((ask, error)),
        }
    }
    (settled, unchecked)
}

/// Close the open `update_failed` asks of the automatic update that the
/// swap to `installed` settled (see [`settled_failures`]): the runtime
/// answers them [`UPDATE_INSTALLED_ANSWER`]. A person's install's asks
/// are not the job's to close (ADR-0073 decision 14). Nothing here fails
/// the swap, which is recorded already: what goes wrong is a warning.
fn close_settled_failures(ports: &JobPorts, queue: &mut dyn Queue, installed: &str) {
    let closed = (|| -> Result<()> {
        let open: Vec<crate::domain::AskId> = queue
            .asks(super::AskQuery {
                all: false,
                open: true,
                role: None,
            })?
            .into_iter()
            .filter(|ask| {
                ask.kind == AskKind::UpdateFailed && ask.task_id.is_none() && ask.run_id.is_none()
            })
            .map(|ask| ask.id)
            .collect();
        if open.is_empty() {
            return Ok(());
        }
        let updates = queue.update_events(UPDATE_HISTORY)?;
        let (settled, unchecked) = settled_failures(&open, &updates, installed, ports.is_ancestor);
        for (ask, error) in unchecked {
            tracing::warn!(
                "whether {installed} contains the failure of update_failed ask {ask} could not be told, so the ask stays open: {error:#}"
            );
        }
        for ask in settled {
            // One that cannot be closed does not keep the others open.
            if let Err(error) =
                queue.close_installed_update_ask(ask, UPDATE_INSTALLED_ANSWER, installed)
            {
                tracing::warn!(
                    "update_failed ask {ask}, which {installed} settles, could not be closed: {error:#}"
                );
            }
        }
        Ok(())
    })();
    if let Err(error) = closed {
        tracing::warn!("the update_failed asks {installed} settles could not be closed: {error:#}");
    }
}

/// The release of the release update's job whose failure opened the
/// `update_failed` ask `ask_id`; `None` when a build of the automatic
/// update failed (or the step is not among `updates`).
pub fn failed_release(updates: &[RunEvent], ask_id: crate::domain::AskId) -> Option<&str> {
    failed_step(updates, ask_id).and_then(step_release)
}

/// Whether the release update's `update_failed` `step` is about the plugin
/// alone: a job that only brought the plugin (`plugin_only`), or the
/// binary's job whose plugin update failed after the binary was replaced
/// (`stage: plugin`). `None` when the step says neither way (an older
/// runtime's `update_failed` without `plugin_only`, or one not about a
/// release).
pub fn failure_plugin_only(step: &RunEvent) -> Option<bool> {
    step_release(step)?;
    if step.payload["stage"] == "plugin" {
        return Some(true);
    }
    step.payload["plugin_only"].as_bool()
}

/// The newest step of the jobs of the release update (`release`) or of the
/// automatic update (not `release`), as [`latest_job_step`] finds it: the
/// one that tells whether a job of that kind was interrupted, even when a
/// job of the other kind ran after it.
pub fn latest_job_step_of(updates: &[RunEvent], release: bool) -> Option<&RunEvent> {
    updates.iter().find(|update| {
        step_release(update).is_some() == release
            && !is_answer_step(update)
            && !step_install(update)
    })
}

/// The pid of the job that wrote `update`, if it recorded one.
pub fn job_pid(update: &RunEvent) -> Option<u32> {
    update
        .payload
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
}

/// The automatic update as `status` shows it: whether a live supervisor has
/// it on, and where the latest update stands (`state`: `building`,
/// `testing` while a build of main runs its e2e, `installing`,
/// `interrupted` when its job died, `installed`,
/// `plugin_installed` when a plugin-only job brought the plugin to a
/// release without replacing the binary, `failed`, `awaiting_approval`,
/// `retry_requested`, `skipped`, `dropped` when the release update left a
/// request it no longer needed; `idle` when none ran), with its commit,
/// time and details. `updates` is newest first.
pub fn status(
    registrations: &[SupervisorRegistration],
    updates: &[RunEvent],
    processes: &dyn ProcessControl,
    now: i64,
) -> Value {
    let enabled = registrations.iter().any(|registration| {
        registration.auto_update
            && processes.alive(registration.pid)
            && now - registration.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
    });
    // A person's install is no step of the update (ADR-0073 decision 14):
    // its failure is its own ask's.
    let updates: Vec<RunEvent> = updates
        .iter()
        .filter(|update| !step_install(update))
        .cloned()
        .collect();
    let updates = updates.as_slice();
    let Some(latest) = updates.first() else {
        return json!({"enabled": enabled, "state": "idle"});
    };
    let latest = match latest_job_step(updates) {
        Some(step) if in_progress(step, processes) => step,
        _ => latest,
    };
    let state = match latest.kind.as_str() {
        UPDATE_STARTED if in_progress(latest, processes) => "building",
        // A build of main runs its e2e after it is built (ADR-t963-1); a
        // release goes on to its install.
        UPDATE_BUILT if in_progress(latest, processes) && step_release(latest).is_none() => {
            "testing"
        }
        UPDATE_BUILT | UPDATE_E2E_PASSED if in_progress(latest, processes) => "installing",
        UPDATE_RESTORED if in_progress(latest, processes) => "restoring",
        UPDATE_STARTED | UPDATE_BUILT | UPDATE_E2E_PASSED | UPDATE_RESTORED => "interrupted",
        UPDATE_INSTALLED if crate::domain::stats::updates::plugin_only(latest) => {
            "plugin_installed"
        }
        UPDATE_INSTALLED => "installed",
        UPDATE_FAILED => "failed",
        UPDATE_AWAITING_APPROVAL => "awaiting_approval",
        UPDATE_RETRY => "retry_requested",
        UPDATE_ANSWERED => "skipped",
        UPDATE_DROPPED => "dropped",
        _ => "unknown",
    };
    // The commit is the one the latest job worked on; an answer's row
    // names none.
    let commit = updates.iter().find_map(step_commit);
    json!({
        "enabled": enabled,
        "state": state,
        "commit": commit,
        "event_id": latest.id,
        "at": latest.created_at,
        "last": latest.payload,
    })
}

/// What the supervisor decides from main and the update log on a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// Build `commit`; `base` is what the runtime change was measured from.
    Build {
        commit: String,
        base: Option<String>,
    },
    /// Nothing to build.
    Idle,
}

/// The commit an update is measured from: the one the latest job worked
/// on, else the commit this build names, else `fallback` (main when the
/// supervisor first looked).
pub fn base_commit(updates: &[RunEvent], version: &str, fallback: Option<&str>) -> Option<String> {
    updates
        .iter()
        .find(|update| update.kind == UPDATE_STARTED && step_release(update).is_none())
        .and_then(step_commit)
        .map(str::to_owned)
        .or_else(|| build_commit(version).map(str::to_owned))
        .or_else(|| fallback.map(str::to_owned))
}

/// Whether the latest word in the log is a `retry` answer after the latest
/// job: build main's head again whatever it changed. The release update's
/// steps are not the automatic update's.
pub fn retry_requested(updates: &[RunEvent]) -> bool {
    updates
        .iter()
        .filter(|update| step_release(update).is_none() && !step_install(update))
        .find(|update| matches!(update.kind.as_str(), UPDATE_STARTED | UPDATE_RETRY))
        .is_some_and(|update| update.kind == UPDATE_RETRY)
}

/// The update job's settings: the commit to build, the supervisor that
/// asked for it, where the binary goes and how long each wait may take.
#[derive(Debug, Clone)]
pub struct JobOptions {
    pub commit: String,
    /// The supervisor that started the job, whose handoff it watches.
    pub token: LeaseToken,
    /// The fixed binary to replace: the supervisor's own.
    pub target: PathBuf,
    /// A checkout of the repository the update's worktree is added from.
    pub repository: PathBuf,
    pub paths: UpdatePaths,
    /// Where the build's output is appended.
    pub log: PathBuf,
    /// A shell command in place of `cargo build --release --locked -p dagq`
    /// (tests), run in the checkout with
    /// `CARGO_TARGET_DIR` set.
    pub build_command: Option<String>,
    /// The e2e the build passes before it is put in place (ADR-t963-1
    /// decision 1), run in the checkout with `CARGO_TARGET_DIR` set.
    pub e2e: E2eSettings,
    /// The `up` arguments the question of a breaking build hands a person,
    /// beside `--db`: `--cmux`, `--claude`, `--plugin-dir`.
    pub restart: Vec<String>,
    /// How long the supervisor may take to exec the new binary (it
    /// finishes a validation or landing in progress first).
    pub handoff_timeout: Duration,
    /// How long the new supervisor may take to heartbeat on after it took
    /// its registration back (ADR-0045 decision 13).
    pub watch_timeout: Duration,
    pub poll: Duration,
    /// This process, recorded on its steps.
    pub pid: u32,
}

pub struct JobPorts<'a> {
    pub binaries: &'a dyn Binaries,
    pub files: &'a dyn RunFiles,
    pub processes: &'a dyn ProcessControl,
    pub clock: &'a dyn Clock,
    /// Opens the queue at a database path.
    pub queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener>,
    /// Start the supervisor of this registration again with the binary
    /// in place, after it died or was stopped (ADR-0045 decision 13);
    /// what was done.
    pub restart: &'a dyn Fn(&SupervisorRegistration) -> Result<Value>,
    /// Whether the first commit is the second or its ancestor in the
    /// repository the automatic update builds from (`git merge-base
    /// --is-ancestor`): which failed builds a swap put in place.
    pub is_ancestor: &'a dyn Fn(&str, &str) -> Result<bool>,
}

/// Build `options.commit` and put it in place of the supervisor's binary
/// (see the module). Every outcome is an `update_*` event and the
/// value returned: `installed`, `awaiting_approval` or `failed`; an error
/// is only a queue that could not be written.
pub fn run(ports: &JobPorts, db: &Path, options: &JobOptions) -> Result<Value> {
    let mut queue = (ports.queues)(db).open()?;
    let queue: &mut dyn Queue = &mut *queue;
    let job = Job {
        subject: Subject::Commit(&options.commit),
        token: Some(&options.token),
        target: &options.target,
        staged: &options.paths.staged,
        log: Some(&options.log),
        restart: &options.restart,
        handoff_timeout: options.handoff_timeout,
        watch_timeout: options.watch_timeout,
        poll: options.poll,
        pid: options.pid,
        e2e_skipped: Default::default(),
    };
    let paths = &options.paths;
    let built = ports
        .binaries
        .checkout(&options.repository, &paths.checkout, &options.commit)
        .and_then(|()| {
            ports.binaries.build_into(
                &paths.checkout,
                &paths.target,
                options.build_command.as_deref(),
                &options.log,
            )
        });
    let binary = match built {
        Ok(binary) => binary,
        Err(error) => return failed(queue, &job, "build", &error, json!({})),
    };
    record_built(queue, &job, &binary)?;
    if let Some(failure) = e2e_gate(ports, queue, &job, options)? {
        return Ok(failure);
    }
    put_in_place(ports, queue, db, &job, &binary, PluginStep::Untouched)
}

/// Record `update_built` for `binary`.
fn record_built(queue: &dyn Queue, job: &Job, binary: &Path) -> Result<EventId> {
    job.subject.record(
        queue,
        EventKind::UpdateBuilt,
        json!({"pid": job.pid, "binary": binary, "log": job.log}),
    )
}

/// Run the e2e of the build in the update's checkout (ADR-t963-1 decision
/// 1): `update_e2e_passed` when it passes, else `update_failed` at the
/// `e2e` stage with its failed tests and log, and the `update_failed` ask,
/// whose value is returned. Nothing is replaced then, a breaking build
/// included.
fn e2e_gate(
    ports: &JobPorts,
    queue: &mut dyn Queue,
    job: &Job,
    options: &JobOptions,
) -> Result<Option<Value>> {
    let settings = &options.e2e;
    let log = &settings.log;
    let outcome = match ports.binaries.e2e(
        &options.paths.checkout,
        Some(&options.paths.target),
        settings,
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            let error = error.context("the e2e could not start");
            return failed(queue, job, "e2e", &error, json!({"e2e_log": log})).map(Some);
        }
    };
    // It ran without RUSTC_WRAPPER (ADR-t2086-1): said, whatever it found.
    if let Some(removed) = wrapper_removed(&outcome, job.pid)
        && let Err(error) = job
            .subject
            .record(&*queue, EventKind::SccacheWrapperRemoved, removed)
    {
        tracing::warn!(error = %format_args!("{error:#}"), "sccache_wrapper_removed could not be recorded: {error:#}");
    }
    // The gates before this one, for a marked test failing in a row
    // (ADR-t1165-1).
    let history = queue.e2e_gate_events(super::e2e_verdict::HISTORY)?;
    let verdict = super::e2e_verdict::judge(&outcome, settings, &history, ports.clock.now());
    if let Some(failure) = &verdict.failure {
        let error = anyhow::anyhow!("{failure}");
        let mut details = json!({
            "e2e_log": log,
            "failed_tests": outcome.failed_tests,
            "timed_out": outcome.timed_out,
            "secs": outcome.secs,
            "cleanup": outcome.cleanup,
        });
        extend(&mut details, &verdict.fields);
        return failed(queue, job, "e2e", &error, details).map(Some);
    }
    let mut passed = json!({
        "pid": job.pid,
        "secs": outcome.secs,
        "log": log,
        "cleanup": outcome.cleanup,
    });
    extend(&mut passed, &verdict.fields);
    // The tests it did not run for want of cmux go on to the
    // `update_installed` too, so the swap does not pass them silently
    // (ADR-t2105-1).
    if let Some(skipped) = &outcome.skipped {
        passed["skipped"] = skipped.to_json();
        job.e2e_skipped.replace(Some(skipped.clone()));
    }
    job.subject
        .record(&*queue, EventKind::UpdateE2ePassed, passed)?;
    Ok(None)
}

/// The payload of the `sccache_wrapper_removed` of the job `pid` whose
/// e2e ran without `RUSTC_WRAPPER` (ADR-t2086-1): `by` `update`, `job`
/// `e2e`, the server's `port` and why (`reason`).
fn wrapper_removed(outcome: &super::install::E2eOutcome, pid: u32) -> Option<Value> {
    let (port, why) = outcome.sccache_wrapper_removed.as_ref()?;
    Some(json!({"by": "update", "job": "e2e", "pid": pid, "port": port, "reason": why}))
}

/// Add the fields of `extra` (an object) to `value` (an object).
fn extend(value: &mut Value, extra: &Value) {
    if let (Some(value), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        value.extend(extra.clone());
    }
}

/// The release update's job (ADR-t618-1 decision 5): the settings of one
/// release to install.
#[derive(Debug, Clone)]
pub struct ReleaseJobOptions {
    /// The release (`X.Y.Z`).
    pub version: String,
    /// The supervisor that started the job, whose handoff it watches.
    pub token: LeaseToken,
    /// The binary to replace: the supervisor's own.
    pub target: PathBuf,
    pub paths: UpdatePaths,
    /// Where cargo's output is appended.
    pub log: PathBuf,
    /// The arguments the question of a breaking release hands a person,
    /// beside `--db`: `--cmux`, `--claude`, `--plugin-dir`.
    pub restart: Vec<String>,
    pub handoff_timeout: Duration,
    pub watch_timeout: Duration,
    pub poll: Duration,
    pub pid: u32,
    /// Only bring the installed plugin to the release, the binary being it
    /// already (ADR-t618-2 decision 4).
    pub plugin_only: bool,
}

/// Install release `options.version` with `installer` under the queue's
/// `update/release` (unless the binary to replace is that release already)
/// and put it in place of the supervisor's binary as [`run`] does a build:
/// the check, a breaking migration left to the `approve_update` ask, the
/// swap, the handoff, the watch and the restore. The steps carry `source:
/// "release"` and the `release`. Once the binary is in place, `plugin` (the
/// installed dagq plugin; `None` when the supervisor loads one from
/// `--plugin-dir`) is brought to the release (ADR-t618-2): a failure there
/// asks, and leaves the binary in place. With `options.plugin_only`, the
/// plugin is all the job updates.
pub fn run_release(
    ports: &JobPorts,
    installer: &dyn install::ReleaseInstaller,
    plugin: Option<&dyn InstalledPlugin>,
    db: &Path,
    options: &ReleaseJobOptions,
) -> Result<Value> {
    let mut queue = (ports.queues)(db).open()?;
    let queue: &mut dyn Queue = &mut *queue;
    let job = Job {
        subject: Subject::Release(&options.version),
        token: Some(&options.token),
        target: &options.target,
        staged: &options.paths.staged,
        log: Some(&options.log),
        restart: &options.restart,
        handoff_timeout: options.handoff_timeout,
        watch_timeout: options.watch_timeout,
        poll: options.poll,
        pid: options.pid,
        e2e_skipped: Default::default(),
    };
    let step = plugin.map_or(PluginStep::Skipped, PluginStep::Update);
    if options.plugin_only {
        return plugin_only(queue, &job, &options.version, step);
    }
    let installed = install::release_binary(
        ports.binaries,
        installer,
        &options.version,
        &options.target,
        &options.paths.release,
        &options.paths.target,
        &options.log,
    );
    let binary = match installed {
        Ok(binary) => binary,
        Err(error) => return failed(queue, &job, "build", &error, json!({})),
    };
    record_built(queue, &job, &binary)?;
    put_in_place(ports, queue, db, &job, &binary, step)
}

/// What a job does about the installed dagq plugin once the binary is in
/// place (ADR-t618-2).
#[derive(Clone, Copy)]
enum PluginStep<'a> {
    /// A build of main's commit: the plugin is left as it is.
    Untouched,
    /// The supervisor loads the plugin from `--plugin-dir` (decision 3):
    /// nothing is done, and `update_installed` says so.
    Skipped,
    /// Bring the installed plugin to the release (decisions 1 and 2).
    Update(&'a dyn InstalledPlugin),
}

/// `plugin` of an `update_installed` whose supervisor loads the plugin from
/// `--plugin-dir`.
pub const PLUGIN_SKIPPED: &str = "skipped: plugin-dir";

/// What `update_installed` says of the plugin after `step`, the note of
/// the attention when it was updated, and the error when its update failed.
fn plugin_outcome(step: PluginStep) -> (Option<Value>, Option<String>, Option<anyhow::Error>) {
    let plugin = match step {
        PluginStep::Untouched => return (None, None, None),
        PluginStep::Skipped => return (Some(json!(PLUGIN_SKIPPED)), None, None),
        PluginStep::Update(plugin) => plugin,
    };
    let runs = plugin.update();
    let commands: Vec<Value> = runs
        .iter()
        .map(
            |run| json!({"command": run.command, "output": run.output, "succeeded": run.succeeded}),
        )
        .collect();
    if let Some(run) = runs.iter().find(|run| !run.succeeded) {
        let error = anyhow::anyhow!("`{}` failed: {}", run.command, run.output);
        return (
            Some(json!({"updated": false, "commands": commands})),
            None,
            Some(error),
        );
    }
    // What it is now, for the report; an unreadable list is no failure.
    let version = plugin.version().ok().flatten();
    let message = format!(
        "The {} plugin of Claude Code was updated{}. The inbox and planner sessions open now keep the plugin they started with: reopen them to load the new one (headless jobs and sessions opened from now on load it already).",
        lifecycle::DAGQ_PLUGIN,
        version
            .as_deref()
            .map(|version| format!(" to {version}"))
            .unwrap_or_default()
    );
    (
        Some(json!({"updated": true, "version": version, "commands": commands})),
        Some(message),
        None,
    )
}

/// The job that only brings the installed plugin to `version`, which the
/// binary is already (ADR-t618-2 decision 4): `update_installed` with
/// `plugin_only`, or `update_failed` at the `plugin` stage.
fn plugin_only(queue: &mut dyn Queue, job: &Job, version: &str, step: PluginStep) -> Result<Value> {
    let (plugin, message, error) = plugin_outcome(step);
    if let Some(error) = error {
        return failed(
            queue,
            job,
            "plugin",
            &error,
            json!({"plugin": plugin, "plugin_only": true, "version": version}),
        );
    }
    let mut payload = json!({
        "pid": job.pid,
        "version": version,
        "plugin_only": true,
        "plugin": plugin,
    });
    if let Some(message) = message {
        payload["message"] = json!(message);
    }
    if let Some(skipped) = &*job.e2e_skipped.borrow() {
        payload["e2e_skipped"] = skipped.to_json();
        let sentence = skipped.sentence();
        payload["message"] = json!(match payload["message"].as_str() {
            Some(message) => format!("{message}; {sentence}"),
            None => sentence,
        });
    }
    job.subject
        .record(&*queue, EventKind::UpdateInstalled, payload.clone())?;
    let mut value = payload;
    value["outcome"] = json!("installed");
    job.subject.tag(&mut value);
    Ok(value)
}

/// What a job puts in place: a build of main's commit (the automatic
/// update) or a release of crates.io (the release update); or what a
/// person's `dagq install` put in place, whose watch and failures go the
/// same way (ADR-0073 decisions 13 and 14).
#[derive(Debug, Clone, Copy)]
enum Subject<'a> {
    Commit(&'a str),
    Release(&'a str),
    Install,
}

impl Subject<'_> {
    /// Record the step `kind` with what it is about in its payload.
    fn record(self, queue: &dyn Queue, kind: EventKind, mut payload: Value) -> Result<EventId> {
        match self {
            Self::Commit(commit) => record(queue, kind, Some(commit), payload),
            Self::Release(version) => {
                payload["source"] = json!(RELEASE_SOURCE);
                payload["release"] = json!(version);
                record(queue, kind, None, payload)
            }
            Self::Install => {
                payload["source"] = json!(INSTALL_SOURCE);
                record(queue, kind, None, payload)
            }
        }
    }

    /// What the job's value names it by.
    fn tag(self, value: &mut Value) {
        match self {
            Self::Commit(commit) => value["commit"] = json!(commit),
            Self::Release(version) => {
                value["source"] = json!(RELEASE_SOURCE);
                value["release"] = json!(version);
            }
            Self::Install => value["source"] = json!(INSTALL_SOURCE),
        }
    }

    /// How a question names it.
    fn describe(self) -> String {
        match self {
            Self::Commit(commit) => format!("main's {}", &commit[..commit.len().min(12)]),
            Self::Release(version) => format!("release {version}"),
            Self::Install => "a person's `dagq install`".to_owned(),
        }
    }
}

/// `source` of the steps of the release update.
pub const RELEASE_SOURCE: &str = "release";

/// Whether `update` was written by a person's `dagq install` rather than
/// by a job.
pub fn step_install(update: &RunEvent) -> bool {
    update.payload.get("source").and_then(Value::as_str) == Some(INSTALL_SOURCE)
}

/// The release a step of the release update is about, if it is one.
pub fn step_release(update: &RunEvent) -> Option<&str> {
    (update.payload.get("source").and_then(Value::as_str) == Some(RELEASE_SOURCE))
        .then(|| update.payload.get("release").and_then(Value::as_str))
        .flatten()
}

/// One job's settings, whatever it puts in place.
struct Job<'a> {
    subject: Subject<'a>,
    /// The supervisor that started the job; none for a person's install.
    token: Option<&'a LeaseToken>,
    target: &'a Path,
    staged: &'a Path,
    /// The job's log; a person's install has none of its own.
    log: Option<&'a Path>,
    restart: &'a [String],
    handoff_timeout: Duration,
    watch_timeout: Duration,
    poll: Duration,
    pid: u32,
    /// The e2e tests the gate did not run (ADR-t1162-1), which
    /// `update_installed` names.
    e2e_skipped: std::cell::RefCell<Option<install::E2eSkip>>,
}

/// Check `binary` (its `update_built` recorded), leave it to a person when
/// it brings a breaking migration, else install it as `install` does, watch
/// the supervisors take it and put the old binary back when none does; once
/// it is in place and taken, do the `plugin` step.
fn put_in_place(
    ports: &JobPorts,
    queue: &mut dyn Queue,
    db: &Path,
    job: &Job,
    binary: &Path,
    plugin: PluginStep,
) -> Result<Value> {
    let pid = job.pid;
    let schema = match ports.binaries.schema(binary, db) {
        Ok(schema) => schema,
        Err(error) => return failed(queue, job, "check", &error, json!({})),
    };
    let breaking: Vec<i64> = schema
        .pending
        .iter()
        .filter(|migration| !migration.compatible)
        .map(|migration| migration.version)
        .collect();
    if !breaking.is_empty() {
        return match stage(ports, binary, job.staged) {
            Ok(version) => awaiting_approval(queue, db, job, &version, &breaking),
            Err(error) => failed(queue, job, "check", &error, json!({})),
        };
    }
    let registered = queue.supervisors()?;
    let before = job.token.and_then(|token| {
        registered
            .iter()
            .find(|registration| registration.token == *token)
            .cloned()
    });
    // The handoff is asked for within the install: a registration made
    // before it is not a handed-over supervisor's successor.
    let handoff_from = ports.clock.now();
    let no_drain = || -> Result<Value> { bail!("the automatic update never drains") };
    let installed = install::install(
        &install::Ports {
            binaries: ports.binaries,
            files: ports.files,
            processes: ports.processes,
            clock: ports.clock,
            queues: ports.queues,
            down: &no_drain,
        },
        Some(db),
        &InstallOptions {
            source: Source::Binary(binary.to_path_buf()),
            target: job.target.to_path_buf(),
            allow_breaking: false,
            restart: Vec::new(),
            handoff_timeout: job.handoff_timeout,
            poll: job.poll,
            // A built binary: the job ran its e2e before.
            e2e: E2eGate::NotApplicable,
        },
    );
    let report = match settle(
        ports,
        queue,
        job,
        &registered,
        before.as_ref(),
        handoff_from,
        installed,
    )? {
        Settled::Taken { report, .. } => report,
        Settled::Failed(failure) => return Ok(failure),
    };
    let version = report["version"].as_str().unwrap_or_default().to_owned();
    let mut payload = json!({
        "pid": pid,
        "version": version,
        "previous_version": report["previous_version"],
        "migrated": report["migrated"],
        "supervisors": report["supervisors"],
        "log": job.log,
    });
    // After the binary, never before (ADR-t618-2 decision 1).
    let (plugin, message, plugin_error) = plugin_outcome(plugin);
    if let Some(plugin) = plugin {
        payload["plugin"] = plugin;
    }
    if let Some(message) = message {
        payload["message"] = json!(message);
    }
    if let Some(skipped) = &*job.e2e_skipped.borrow() {
        payload["e2e_skipped"] = skipped.to_json();
        let sentence = skipped.sentence();
        payload["message"] = json!(match payload["message"].as_str() {
            Some(message) => format!("{message}; {sentence}"),
            None => sentence,
        });
    }
    job.subject
        .record(&*queue, EventKind::UpdateInstalled, payload.clone())?;
    if let Subject::Commit(commit) = job.subject {
        close_settled_failures(ports, queue, commit);
    }
    // The new binary works with the plugin it had, so it stays (decision
    // 2): the failure only asks.
    if let Some(error) = plugin_error {
        return failed(
            queue,
            job,
            "plugin",
            &error,
            json!({"plugin": payload["plugin"], "version": version}),
        );
    }
    let mut value = payload;
    value["outcome"] = json!("installed");
    job.subject.tag(&mut value);
    Ok(value)
}

/// How an install went once [`settle`] watched and, on a failure, brought
/// the supervisors back.
enum Settled {
    /// Every supervisor handed over heartbeats on under the new build:
    /// `install`'s report, and what the watch saw of each.
    Taken {
        report: Value,
        watched: Vec<Watched>,
    },
    /// The `update_failed` recorded (its ask opened), as the job's value.
    Failed(Value),
}

/// After `installed`, the outcome of `install` (ADR-0073 decisions 13 and
/// 14, ADR-t632-1): watch the supervisors it handed over heartbeat on under
/// the new build; when none does (or every handoff failed), put the old
/// binary back if `.previous` is the build it replaced; bring each one that
/// failed back; and record `update_failed` with its ask. `registered` is
/// the queue's registrations before the install, `before` the job's own
/// supervisor among them (none for a person's install). A person's install
/// that failed before any handoff is its own error, unchanged.
#[allow(clippy::too_many_arguments)]
fn settle(
    ports: &JobPorts,
    queue: &mut dyn Queue,
    job: &Job,
    registered: &[SupervisorRegistration],
    before: Option<&SupervisorRegistration>,
    handoff_from: i64,
    installed: Result<Value>,
) -> Result<Settled> {
    // An install that handed some of the supervisors over but not all kept
    // the new binary for them (ADR-t632-1): the ones handed over are
    // watched, and the ones that did not take it fail as a watch would.
    let (report, refused) = match installed {
        Ok(report) => (report, Vec::new()),
        Err(error) => match install::KeptBinary::of(&error) {
            Some(kept) => (kept.report.clone(), refused(&kept.report)),
            None => {
                // `install` put the old binary back itself if it had
                // replaced it; the supervisors it handed over may be gone
                // or stuck with the new one, each brought back as after a
                // failed watch.
                if let Some(handoff) = install::HandoffFailed::of(&error) {
                    let mut supervisors = Vec::new();
                    for failed in &handoff.supervisors {
                        let (Some(token), Some(pid)) = (
                            failed["token"].as_str().map(LeaseToken::new),
                            failed["pid"]
                                .as_u64()
                                .and_then(|pid| u32::try_from(pid).ok()),
                        ) else {
                            continue;
                        };
                        let before = registered_before(registered, &token, pid);
                        let mut entry = json!({
                            "token": token,
                            "pid": pid,
                            "error": failed["error"],
                            "supervisor": bring_back(ports, &*queue, before, Some(&token))?,
                        });
                        if stopping(failed) {
                            entry["stopping"] = json!(true);
                        }
                        supervisors.push(entry);
                    }
                    let mut details = json!({
                        "restored": handoff.restored.clone(),
                        "kept": false,
                        "supervisors": supervisors,
                    });
                    // The job's own supervisor is brought back even when it
                    // was not handed over (not live when the install looked).
                    if let Some(before) = before.filter(|before| {
                        !handoff
                            .supervisors
                            .iter()
                            .any(|s| s["token"] == before.token.as_str() || s["pid"] == before.pid)
                    }) {
                        details["supervisor"] =
                            bring_back(ports, &*queue, Some(before), Some(&before.token))?;
                    }
                    return failed(queue, job, "install", &error, details).map(Settled::Failed);
                }
                // A person's install that failed before any handoff
                // replaced nothing, or put it back: its error says so.
                if matches!(job.subject, Subject::Install) {
                    return Err(error);
                }
                // It failed before any handoff: only the job's own
                // supervisor can be in doubt.
                let serving = before.map(|before| before.token.clone());
                let supervisor = bring_back(ports, &*queue, before, serving.as_ref())?;
                return failed(
                    queue,
                    job,
                    "install",
                    &error,
                    json!({"supervisor": supervisor}),
                )
                .map(Settled::Failed);
            }
        },
    };
    let version = report["version"].as_str().unwrap_or_default().to_owned();
    let handed = handed_over(&report, before);
    let stage = if refused.is_empty() {
        "watch"
    } else {
        "handoff"
    };
    let mut watched = refused;
    watched.extend(watch(ports, &*queue, &handed, &version, handoff_from, job)?);
    let failures: Vec<&Watched> = watched.iter().filter(|w| w.error.is_some()).collect();
    if failures.is_empty() {
        return Ok(Settled::Taken { report, watched });
    }
    // The binary is one file for every supervisor: it goes back only when
    // none of them runs the new build, and a supervisor that does keeps it.
    let everyone = failures.len() == watched.len();
    let restored = if everyone {
        restore(ports, job, report["previous_version"].as_str())
    } else {
        json!({
            "restored": false,
            "reason": format!(
                "{} of the {} supervisors handed over run {version}, so it stays in place",
                watched.len() - failures.len(),
                watched.len()
            ),
        })
    };
    let mut supervisors = Vec::new();
    for watched in &watched {
        let mut entry = json!({
            "token": watched.token,
            "pid": watched.pid,
            "now": watched.now,
            "error": watched.error,
        });
        if watched.stopping {
            entry["stopping"] = json!(true);
        }
        if watched.error.is_some() {
            let before = registered_before(registered, &watched.token, watched.pid);
            entry["supervisor"] = bring_back(ports, &*queue, before, Some(&watched.now))?;
        }
        supervisors.push(entry);
    }
    // A step of the job still working (it goes on to its `update_failed`);
    // it must not keep the supervisor from being brought back or the ask
    // from opening. A person's install is no job, and writes no step.
    if restored["restored"] == true && !matches!(job.subject, Subject::Install) {
        let _ = job.subject.record(
            &*queue,
            EventKind::UpdateRestored,
            json!({
                "pid": job.pid,
                "version": version,
                "restored_version": restored["version"],
            }),
        );
    }
    let error = anyhow::anyhow!(
        "{}",
        failures
            .iter()
            .filter_map(|w| w.error.as_deref())
            .collect::<Vec<_>>()
            .join("; ")
    );
    failed(
        queue,
        job,
        stage,
        &error,
        json!({
            "restored": restored,
            "kept": !everyone,
            "supervisors": supervisors,
            "version": version,
        }),
    )
    .map(Settled::Failed)
}

/// The error of a person's `dagq install` whose handoff or watch failed
/// (ADR-0073 decisions 13 and 14): `report` is the `update_failed` it
/// recorded, with the ask it opened for the inbox, for the command to
/// print beside the error.
#[derive(Debug)]
pub struct InstallFailed {
    pub message: String,
    pub report: Value,
}

impl std::fmt::Display for InstallFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InstallFailed {}

impl InstallFailed {
    /// The [`InstallFailed`] `error` is or wraps.
    pub fn of(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

/// The registrations a person's install brings a supervisor back from:
/// `before`, the record taken before the install, then each row of `now`
/// (read after it) whose token the record lacks, a supervisor that
/// registered while the install built and checked its binary (`up`, or
/// launchd starting one again) included. Such a row was read after the
/// handoff, so where `installed` reports the pid it takes the build, mode
/// and workspace reported, the ones the registration had when it was
/// handed over (the row the same pid made again under the new build has
/// no mode of its own).
fn registered_since(
    before: Vec<SupervisorRegistration>,
    now: Vec<SupervisorRegistration>,
    installed: &Result<Value>,
) -> Vec<SupervisorRegistration> {
    let reported: Vec<Value> = match installed {
        Ok(report) => report["supervisors"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        Err(error) => match (
            install::KeptBinary::of(error),
            install::HandoffFailed::of(error),
        ) {
            (Some(kept), _) => kept.report["supervisors"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            (None, Some(handoff)) => handoff.supervisors.clone(),
            (None, None) => Vec::new(),
        },
    };
    let mut registered = before;
    for mut row in now {
        if registered.iter().any(|r| r.token == row.token) {
            continue;
        }
        if let Some(handed) = reported.iter().find(|handed| handed["pid"] == row.pid) {
            row.binary_version = handed["version"].as_str().map(str::to_owned);
            row.mode = handed["mode"].as_str().and_then(|mode| mode.parse().ok());
            row.workspace_id = handed["workspace_id"].as_str().map(str::to_owned);
        }
        registered.push(row);
    }
    registered
}

/// How long a person's install watches the supervisors it handed over by
/// default, as the automatic update's job does (ADR-0073 decision 13).
pub const WATCH_TIMEOUT: Duration = Duration::from_secs(60);

/// A person's `dagq install` (ADR-0073 decision 14): [`install::install`],
/// then the watch of decision 13 as the automatic update's job does it
/// ([`settle`]): each supervisor handed over must heartbeat on under the
/// new build within `watch_timeout`, and the install succeeds only once
/// they all do, its report naming what the watch saw of each (`watch`).
/// When every one fails, the old binary goes back if `.previous` is the
/// build it replaced; when some do, it stays (ADR-t632-1); either way the
/// failed ones are brought back with `ports.restart`, `update_failed`
/// (`source: "install"`) is recorded and its ask opens for the inbox, and
/// the error is an [`InstallFailed`]. With no queue, no supervisor handed
/// over, or the drain of a breaking migration (`up` starts the
/// supervisor), nothing is watched.
pub fn install_watched(
    ports: &JobPorts,
    down: &dyn Fn() -> Result<Value>,
    db: Option<&Path>,
    options: &InstallOptions,
    watch_timeout: Duration,
) -> Result<Value> {
    let db = db.filter(|db| ports.files.is_file(db));
    // What each supervisor was, for its mode when it is brought back; one
    // that registers while the install builds and checks (a long while for
    // a checkout and its e2e) is added after it ([`registered_since`]).
    let registered = db
        .and_then(|db| (ports.queues)(db).open().ok())
        .and_then(|queue| queue.supervisors().ok())
        .unwrap_or_default();
    let handoff_from = ports.clock.now();
    let installed = install::install(
        &install::Ports {
            binaries: ports.binaries,
            files: ports.files,
            processes: ports.processes,
            clock: ports.clock,
            queues: ports.queues,
            down,
        },
        db,
        options,
    );
    let Some(db) = db else {
        return installed;
    };
    match &installed {
        Err(error)
            if install::KeptBinary::of(error).is_none()
                && install::HandoffFailed::of(error).is_none() =>
        {
            return installed;
        }
        // Nothing handed over (no live supervisor, or the drain of a
        // breaking migration, whose `up` started the supervisor and after
        // which this binary may not open the queue): nothing to watch.
        Ok(report)
            if report["supervisors"]
                .as_array()
                .is_none_or(|supervisors| supervisors.is_empty()) =>
        {
            let mut report = report.clone();
            report["watch"] = json!([]);
            return Ok(report);
        }
        _ => {}
    }
    let mut queue = (ports.queues)(db).open()?;
    let queue: &mut dyn Queue = &mut *queue;
    let registered = registered_since(registered, queue.supervisors()?, &installed);
    let staged = UpdatePaths::under(db.parent().unwrap_or(Path::new("."))).staged;
    let job = Job {
        subject: Subject::Install,
        token: None,
        target: &options.target,
        staged: &staged,
        log: None,
        restart: &options.restart,
        handoff_timeout: options.handoff_timeout,
        watch_timeout,
        poll: options.poll,
        pid: std::process::id(),
        e2e_skipped: Default::default(),
    };
    match settle(
        ports,
        queue,
        &job,
        &registered,
        None,
        handoff_from,
        installed,
    )? {
        Settled::Taken {
            mut report,
            watched,
        } => {
            report["watch"] = json!(
                watched
                    .iter()
                    .map(|watched| json!({
                        "token": watched.token,
                        "pid": watched.pid,
                        "now": watched.now,
                        "heartbeat": true,
                    }))
                    .collect::<Vec<_>>()
            );
            Ok(report)
        }
        Settled::Failed(report) => Err(InstallFailed {
            message: format!(
                "the install failed at its {}: {}. The binary: {}. The inbox was told by the \
`update_failed` ask {}; each supervisor's state is in the result",
                report["stage"].as_str().unwrap_or_default(),
                report["error"].as_str().unwrap_or_default(),
                report["restored"],
                report["ask_id"]
            ),
            report,
        }
        .into()),
    }
}

fn registration(
    queue: &dyn RunCoordination,
    token: &LeaseToken,
) -> Result<Option<SupervisorRegistration>> {
    Ok(queue
        .supervisors()?
        .into_iter()
        .find(|registration| registration.token == *token))
}

/// The registration a supervisor the install handed over had before it:
/// by its token, or by its pid when the handoff reported a new token.
fn registered_before<'a>(
    registered: &'a [SupervisorRegistration],
    token: &LeaseToken,
    pid: u32,
) -> Option<&'a SupervisorRegistration> {
    registered
        .iter()
        .find(|r| r.token == *token)
        .or_else(|| registered.iter().find(|r| r.pid == pid))
}

/// Check a build that waits for a person as `install` would (its version
/// and a start on a throwaway queue) and keep a copy of it, so a later
/// build in the target does not replace what the person is asked about.
fn stage(ports: &JobPorts, binary: &Path, staged: &Path) -> Result<String> {
    let version = ports.binaries.version(binary)?;
    ports.binaries.probe(binary)?;
    if let Some(dir) = staged.parent() {
        ports
            .files
            .create_dir_all(dir)
            .with_context(|| format!("create {}", dir.display()))?;
    }
    ports
        .files
        .copy(binary, staged)
        .with_context(|| format!("keep the build at {}", staged.display()))?;
    Ok(version)
}

/// The supervisors the install handed over, by token and pid: the ones
/// its report names without an error, or the job's own supervisor when it
/// names none.
fn handed_over(report: &Value, before: Option<&SupervisorRegistration>) -> Vec<(LeaseToken, u32)> {
    let handed: Vec<(LeaseToken, u32)> = report["supervisors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|supervisor| supervisor["error"].is_null())
        .filter_map(|supervisor| {
            Some((
                LeaseToken::new(supervisor["token"].as_str()?),
                u32::try_from(supervisor["pid"].as_u64()?).ok()?,
            ))
        })
        .collect();
    if !handed.is_empty() {
        return handed;
    }
    before
        .map(|before| vec![(before.token.clone(), before.pid)])
        .unwrap_or_default()
}

/// The supervisors the install's report names as not having taken the
/// handoff, each failed with its error.
fn refused(report: &Value) -> Vec<Watched> {
    report["supervisors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|supervisor| {
            let error = supervisor["error"].as_str()?;
            let token = LeaseToken::new(supervisor["token"].as_str()?);
            Some(Watched {
                now: token.clone(),
                token,
                pid: u32::try_from(supervisor["pid"].as_u64()?).ok()?,
                first: None,
                done: false,
                error: Some(error.to_owned()),
                stopping: stopping(supervisor),
            })
        })
        .collect()
}

/// What the watch saw of one supervisor it handed over: the token it
/// serves under now (another one when the same pid registered again under
/// the new build) or why it failed.
#[derive(Debug)]
struct Watched {
    token: LeaseToken,
    pid: u32,
    now: LeaseToken,
    first: Option<i64>,
    done: bool,
    error: Option<String>,
    /// It did not take the handoff for the stop request it drains for
    /// (task 1277).
    stopping: bool,
}

/// Whether a supervisor of `install`'s report did not take the handoff for
/// its stop request (`stopping: true`, task 1277).
fn stopping(supervisor: &Value) -> bool {
    supervisor["stopping"] == true
}

/// Wait for each supervisor in `handed` to heartbeat on under `version`
/// after the handoff: a heartbeat later than the one it took its
/// registration back with, within the watch timeout (ADR-0045 decision
/// 13). A token that deregistered is followed to a registration the same
/// pid made under `version` since `handoff_from` ([`lifecycle::successor`]).
/// Every supervisor is watched to its end, so the outcome of one does not
/// decide another's.
fn watch(
    ports: &JobPorts,
    queue: &dyn Queue,
    handed: &[(LeaseToken, u32)],
    version: &str,
    handoff_from: i64,
    job: &Job,
) -> Result<Vec<Watched>> {
    let deadline = Instant::now() + job.watch_timeout;
    let mut watched: Vec<Watched> = handed
        .iter()
        .map(|(token, pid)| Watched {
            token: token.clone(),
            pid: *pid,
            now: token.clone(),
            first: None,
            done: false,
            error: None,
            stopping: false,
        })
        .collect();
    loop {
        let registrations = queue.supervisors()?;
        let expired = Instant::now() >= deadline;
        for watched in watched.iter_mut().filter(|w| !w.done && w.error.is_none()) {
            if let Err(error) = observe(
                ports,
                &registrations,
                watched,
                version,
                handoff_from,
                expired,
                job,
            ) {
                watched.error = Some(format!("{error:#}"));
            }
        }
        if watched.iter().all(|w| w.done || w.error.is_some()) {
            return Ok(watched);
        }
        thread::sleep(job.poll);
    }
}

/// One look at `watched`: done once it heartbeats on under `version`, an
/// error when it cannot any more.
fn observe(
    ports: &JobPorts,
    registrations: &[SupervisorRegistration],
    watched: &mut Watched,
    version: &str,
    handoff_from: i64,
    expired: bool,
    job: &Job,
) -> Result<()> {
    let token = watched.token.clone();
    let Some(current) = lifecycle::successor(
        registrations,
        &watched.now,
        watched.pid,
        version,
        handoff_from,
    ) else {
        bail!("supervisor {token} deregistered after it took the handoff to {version}");
    };
    if current.token != watched.now {
        watched.now = current.token.clone();
        watched.first = None;
    }
    ensure!(
        ports.processes.alive(current.pid),
        "supervisor {token} (pid {}) exited after it took the handoff to {version}",
        current.pid
    );
    ensure!(
        current.binary_version.as_deref() == Some(version),
        "supervisor {token} runs {} instead of {version}",
        current.binary_version.as_deref().unwrap_or("(unrecorded)")
    );
    match watched.first {
        None => watched.first = Some(current.heartbeat_at),
        Some(first) if current.heartbeat_at > first => {
            watched.done = true;
            return Ok(());
        }
        Some(_) => {}
    }
    ensure!(
        !expired,
        "supervisor {token} did not heartbeat within {}s of taking the handoff to {version}",
        job.watch_timeout.as_secs()
    );
    Ok(())
}

/// Put the replaced binary back at the target, only when `.previous` is the
/// build the supervisor ran before (ADR-0045 decision 13): a `.previous`
/// of another build was not put there by this update.
fn restore(ports: &JobPorts, job: &Job, previous_version: Option<&str>) -> Value {
    let previous = previous_path(job.target);
    let kept = ports.binaries.version(&previous).ok();
    if kept.is_none() || kept.as_deref() != previous_version {
        return json!({
            "restored": false,
            "reason": format!(
                "{} is {} rather than the replaced {}",
                previous.display(),
                kept.as_deref().unwrap_or("missing"),
                previous_version.unwrap_or("(unknown)")
            ),
        });
    }
    match ports.binaries.restore(job.target) {
        Ok(()) => json!({"restored": true, "version": kept}),
        Err(error) => json!({"restored": false, "reason": format!("{error:#}")}),
    }
}

/// After a failed install or watch, make sure a supervisor serves the
/// queue with the binary in place: one that still heartbeats under the
/// build it had (its exec failed and it went on) is left alone; one that
/// lives but does not (the new binary hangs) is stopped; and one that is
/// gone is started again ([`JobPorts::restart`]). `serving` is the token
/// it serves under now (another one when its pid registered again), or
/// `before`'s. What was found and done.
fn bring_back(
    ports: &JobPorts,
    queue: &dyn Queue,
    before: Option<&SupervisorRegistration>,
    serving: Option<&LeaseToken>,
) -> Result<Value> {
    let Some(before) = before else {
        return Ok(json!({"state": "not_registered"}));
    };
    let Some(mut current) = registration(queue, serving.unwrap_or(&before.token))? else {
        return Ok(json!({"state": "deregistered"}));
    };
    let now = ports.clock.now();
    let alive = ports.processes.alive(current.pid);
    if alive
        && now - current.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
        && current.handoff_binary.is_none()
        && current.binary_version == before.binary_version
    {
        return Ok(json!({"state": "running", "version": current.binary_version}));
    }
    if alive {
        let _ = ports.processes.terminate(current.pid);
        let deadline = Instant::now() + Duration::from_secs(10);
        while ports.processes.alive(current.pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        if ports.processes.alive(current.pid) {
            let _ = ports.processes.kill(current.pid);
        }
    }
    // A registration the same pid made again under the new build has no
    // mode of its own: it is started again the way `up` started it.
    if current.mode.is_none() {
        current.mode = before.mode;
        current.workspace_id = current.workspace_id.or_else(|| before.workspace_id.clone());
    }
    Ok(match (ports.restart)(&current) {
        Ok(restarted) => json!({"state": "restarted", "stopped": alive, "restart": restarted}),
        Err(error) => json!({
            "state": "stopped",
            "stopped": alive,
            "error": format!("{error:#}"),
        }),
    })
}

/// How a question names the job's log.
fn job_log(job: &Job) -> String {
    job.log
        .map(|log| log.display().to_string())
        .unwrap_or_else(|| "(none)".to_owned())
}

/// Record `update_failed` and open the `update_failed` ask with what
/// failed and what became of the binary and the supervisor.
fn failed(
    queue: &mut dyn Queue,
    job: &Job,
    stage: &str,
    error: &anyhow::Error,
    details: Value,
) -> Result<Value> {
    let error = format!("{error:#}");
    let mut situation = match stage {
        "build" => "Nothing was replaced.".to_owned(),
        "e2e" => format!(
            "The build did not pass its e2e, so nothing was replaced (a build with a breaking \
migration is not kept for a person either). The e2e's log is {}.",
            details["e2e_log"].as_str().unwrap_or("(none)")
        ),
        "check" => "The build did not pass its check, so nothing was replaced.".to_owned(),
        _ if details["kept"] == true => format!(
            "The new binary stays at {}: other supervisors run it. The ones that failed are \
brought back with it as said below; one still running the build it had is left as it is, and \
`down --force` and `up` start it with the new binary (or `install --rollback` puts the old binary \
back for every supervisor).",
            job.target.display()
        ),
        _ => format!(
            "If the new binary had been put in place, the one it replaced is back at {} unless \
said otherwise below.",
            job.target.display()
        ),
    };
    if let Some(supervisor) = details.get("supervisor") {
        situation.push_str(&format!(" The supervisor: {supervisor}."));
    }
    if let Some(supervisors) = details.get("supervisors") {
        situation.push_str(&format!(" The supervisors: {supervisors}."));
    }
    if let Some(restored) = details.get("restored") {
        situation.push_str(&format!(" The binary: {restored}."));
    }
    let question = match job.subject {
        Subject::Release(version) if stage == "plugin" => format!(
            "The update of the {plugin} plugin of Claude Code to release {version} failed: {error}\n\nThe binary stays as it is: dagq {version} runs, and works with the plugin it had. The \
job's log is {}.\n\nAnswer `retry` to have the supervisor run `claude {}` and `claude {}` again at \
its next check (after fixing what failed), or `skip` to leave the plugin as it is. By hand, run the \
two commands, then reopen the inbox and planner sessions to load the new plugin.",
            job_log(job),
            lifecycle::PLUGIN_UPDATE_ARGUMENTS[0].join(" "),
            lifecycle::PLUGIN_UPDATE_ARGUMENTS[1].join(" "),
            plugin = lifecycle::DAGQ_PLUGIN,
        ),
        Subject::Commit(_) => format!(
            "The automatic update to {} failed at its {stage}: {error}\n\n{situation} The job's \
log is {}.\n\nAnswer `retry` to build main's head again at the supervisor's next check (after \
fixing what failed), or `skip` to wait for the next landing that changes the runtime. If no \
supervisor serves the queue now, `up` starts one.",
            job.subject.describe(),
            job_log(job)
        ),
        Subject::Install => format!(
            "A person's `dagq install` of {version} into {target} failed at its {stage} (it was \
not the automatic update): {error}\n\n{situation}\n\nNothing applies the answer of this ask: no \
supervisor builds or installs anything for it. By hand, run the same `dagq install` again after \
fixing what failed, `dagq install --rollback` to put the binary it replaced back for every \
supervisor, or `dagq down --force` and `dagq up` to start the supervisors with the binary in place. \
Answer `retry` when the person will run the install again, or `skip` to leave it as it is; either \
way the inbox closes the ask.",
            version = details["version"].as_str().unwrap_or("the new binary"),
            target = job.target.display(),
        ),
        Subject::Release(version) => format!(
            "The update to {} failed at its {stage}: {error}\n\n{situation} The job's log is \
{}.\n\nAnswer `retry` to install release {version} at the next check of a supervisor running an older \
build (after fixing what failed; it needs cargo). A supervisor already on that release or newer \
records `update_dropped` instead; any older plugin is handled separately by auto mode or a plugin \
approval. Answer `skip` to leave release {version} (the next release asks \
again). By hand, `dagq install --release {version}` explicitly installs that release, and `dagq install --from \
<binary>` puts a dagq {version} installed another way in place. If no supervisor serves the queue \
now, `up` starts one.",
            job.subject.describe(),
            job_log(job)
        ),
    };
    // What a `retry` of a release's job asks for: after a plugin failure
    // (the binary's job had replaced the binary, or the job was the
    // plugin's alone), only the plugin again.
    let purpose = match job.subject {
        Subject::Release(_) => json!({"plugin_only": stage == "plugin"}),
        Subject::Commit(_) => Value::Null,
        // Its ask supersedes only an earlier install's, not a job's.
        Subject::Install => json!({"source": INSTALL_SOURCE}),
    };
    let ask = queue
        .open_update_ask(
            AskKind::UpdateFailed,
            &question,
            UPDATE_FAILED_OPTIONS,
            UPDATE_ASKER,
            // Open beside a job's, which it does not supersede.
            matches!(job.subject, Subject::Install).then_some(INSTALL_SOURCE),
            purpose,
        )?
        .id;
    let mut payload = json!({
        "pid": job.pid,
        "stage": stage,
        "error": error,
        "log": job.log,
        "ask_id": ask,
    });
    if let (Some(object), Value::Object(details)) = (payload.as_object_mut(), details) {
        object.extend(details);
    }
    job.subject
        .record(&*queue, EventKind::UpdateFailed, payload.clone())?;
    payload["outcome"] = json!("failed");
    job.subject.tag(&mut payload);
    Ok(payload)
}

/// Open the `approve_update` ask for a build with a breaking migration,
/// naming the command that drains and installs it. The command starts the
/// supervisor again as the drained one ran, so no `up` follows it.
fn awaiting_approval(
    queue: &mut dyn Queue,
    db: &Path,
    job: &Job,
    version: &str,
    breaking: &[i64],
) -> Result<Value> {
    let staged = job.staged;
    let mut command = format!(
        "dagq --db {} install --from {} --to {} --allow-breaking",
        db.display(),
        staged.display(),
        job.target.display()
    );
    for argument in job.restart {
        command.push(' ');
        command.push_str(argument);
    }
    let migrations = breaking
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let built = match job.subject {
        Subject::Commit(_) => format!("{} built as {version}", job.subject.describe()),
        Subject::Release(_) | Subject::Install => {
            format!("{} (installed as {version})", job.subject.describe())
        }
    };
    let question = format!(
        "{built}, and it brings breaking migration(s) {migrations}: the running supervisor and its \
runs' wrappers could not open the queue after them, so it was not installed. Answer `install` and \
run `{command}` from the inbox to drain the supervisor (it waits for its runs), back the queue up, \
migrate and start it again with the new binary; or `skip` to leave it. The build is kept at {}.",
        staged.display()
    );
    let ask = queue
        .open_update_ask(
            AskKind::ApproveUpdate,
            &question,
            APPROVE_UPDATE_OPTIONS,
            UPDATE_ASKER,
            None,
            Value::Null,
        )?
        .id;
    let payload = json!({
        "pid": job.pid,
        "version": version,
        "migrations": breaking,
        "binary": staged,
        "command": command,
        "ask_id": ask,
    });
    job.subject
        .record(&*queue, EventKind::UpdateAwaitingApproval, payload.clone())?;
    let mut value = payload;
    value["outcome"] = json!("awaiting_approval");
    job.subject.tag(&mut value);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_landing_changes_the_runtime_when_it_touches_its_sources_or_manifests() {
        let paths = |paths: &[&str]| paths.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
        assert!(changes_runtime(&paths(&["docs/a.md", "src/main.rs"])));
        assert!(changes_runtime(&paths(&["migrations/0032_x.sql"])));
        assert!(changes_runtime(&paths(&["Cargo.lock"])));
        assert!(changes_runtime(&paths(&["build.rs"])));
        assert!(changes_runtime(&paths(&["rust-toolchain.toml"])));
        assert!(!changes_runtime(&paths(&["docs/src/a.md", "README.md"])));
        assert!(!changes_runtime(&paths(&["crates/a/src/lib.rs"])));
        assert!(!changes_runtime(&paths(&["srcs/a.rs", "Cargo.toml.bak"])));
        assert!(!changes_runtime(&[]));
    }

    #[test]
    fn a_build_identifier_names_its_commit_unless_a_release_or_unknown() {
        assert_eq!(build_commit("0.4.0-dev+abc123"), Some("abc123"));
        assert_eq!(build_commit("0.4.0-dev+abc123.dirty"), Some("abc123"));
        assert_eq!(build_commit("0.4.0-dev+unknown"), None);
        assert_eq!(build_commit("0.4.0"), None);
    }

    /// No process is alive.
    struct NoneAlive;

    impl ProcessControl for NoneAlive {
        fn alive(&self, _: u32) -> bool {
            false
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

    /// Every process is alive.
    struct AllAlive;

    impl ProcessControl for AllAlive {
        fn alive(&self, _: u32) -> bool {
            true
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

    /// A build of main runs its e2e after `update_built` (`testing`) and
    /// installs after `update_e2e_passed`; a release installs after
    /// `update_built`; a job gone at either step was interrupted.
    #[test]
    fn status_shows_the_e2e_of_a_build_of_main_as_testing() {
        let state = |kind: &str, payload: Value, processes: &dyn ProcessControl| {
            status(&[], &[update(1, kind, payload)], processes, 0)["state"].clone()
        };
        let main = json!({"pid": 7, "commit": "abc"});
        let release = json!({"pid": 7, "source": "release", "release": "0.5.0"});
        assert_eq!(state(UPDATE_BUILT, main.clone(), &AllAlive), "testing");
        assert_eq!(
            state(UPDATE_E2E_PASSED, main.clone(), &AllAlive),
            "installing"
        );
        assert_eq!(state(UPDATE_BUILT, release, &AllAlive), "installing");
        assert_eq!(state(UPDATE_BUILT, main.clone(), &NoneAlive), "interrupted");
        assert_eq!(state(UPDATE_E2E_PASSED, main, &NoneAlive), "interrupted");
    }

    fn update(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: "2026-09-28T01:00:00.000Z".to_owned(),
            actor: None,
        }
    }

    /// The latest `update_installed` of a plugin-only job shows as
    /// `plugin_installed`; one that replaced the binary as `installed`.
    #[test]
    fn status_tells_a_plugin_only_install_from_a_binary_install() {
        let state = |updates: &[RunEvent]| status(&[], updates, &NoneAlive, 0)["state"].clone();
        let binary = update(
            1,
            UPDATE_INSTALLED,
            json!({"pid": 7, "source": "release", "release": "0.5.0", "version": "0.5.0"}),
        );
        let plugin = update(
            2,
            UPDATE_INSTALLED,
            json!({"pid": 8, "source": "release", "release": "0.5.0", "version": "0.5.0",
                   "plugin_only": true, "plugin": "0.5.0"}),
        );
        assert_eq!(state(std::slice::from_ref(&binary)), "installed");
        assert_eq!(state(&[plugin.clone(), binary.clone()]), "plugin_installed");
        let status = status(&[], &[plugin], &NoneAlive, 0);
        assert_eq!(status["last"]["plugin_only"], true);
        assert_eq!(status["enabled"], false);
    }

    /// A person's install's failure is no step of the automatic update:
    /// `status` shows the job before it, no interrupted job hides behind it
    /// and no `retry` is read from it.
    #[test]
    fn a_persons_install_failure_is_no_step_of_the_update() {
        let started = update(1, UPDATE_STARTED, json!({"pid": 7, "commit": "abc"}));
        let installed = update(2, UPDATE_INSTALLED, json!({"pid": 7, "commit": "abc"}));
        let manual = update(
            3,
            UPDATE_FAILED,
            json!({"pid": 9, "source": INSTALL_SOURCE, "stage": "watch", "ask_id": 4}),
        );
        assert!(step_install(&manual));
        let updates = [manual.clone(), installed.clone(), started.clone()];
        let status = status(&[], &updates, &NoneAlive, 0);
        assert_eq!(status["state"], "installed", "{status}");
        assert_eq!(status["event_id"], 2, "{status}");
        assert_eq!(latest_job_step(&updates).unwrap().id, installed.id);
        let interrupted = [manual.clone(), started];
        assert_eq!(
            latest_job_step_of(&interrupted, false).unwrap().kind,
            UPDATE_STARTED
        );
        assert!(!retry_requested(&interrupted));
        assert_eq!(status_of_only(&manual)["state"], "idle");
    }

    /// A job that wrote its end before the supervisor recorded its start
    /// ended, not interrupted: its start reads as before its steps, and a
    /// step of another job (another pid, another commit, or before the
    /// start or the answer before) stays where it was.
    #[test]
    fn a_job_that_ended_before_its_start_was_recorded_reads_as_ended() {
        let retry = update(1, UPDATE_RETRY, json!({"ask_id": 1, "answer": "retry"}));
        let failed = update(
            2,
            UPDATE_FAILED,
            json!({"pid": 8, "commit": "def", "stage": "build", "ask_id": 2}),
        );
        let started = update(3, UPDATE_STARTED, json!({"pid": 8, "commit": "def"}));
        let ordered = crate::domain::in_job_order(vec![started.clone(), failed.clone(), retry]);
        let ids: Vec<i64> = ordered.iter().map(|u| u.id.as_i64()).collect();
        assert_eq!(ids, [2, 3, 1]);
        assert_eq!(latest_job_step(&ordered).unwrap().id, failed.id);
        assert_eq!(status(&[], &ordered, &NoneAlive, 0)["state"], "failed");
        assert!(!retry_requested(&ordered));

        let ids = |updates: Vec<RunEvent>| -> Vec<i64> {
            crate::domain::in_job_order(updates)
                .iter()
                .map(|u| u.id.as_i64())
                .collect()
        };
        let other_pid = update(2, UPDATE_FAILED, json!({"pid": 9, "commit": "def"}));
        assert_eq!(ids(vec![started.clone(), other_pid]), [3, 2]);
        let other_commit = update(2, UPDATE_FAILED, json!({"pid": 8, "commit": "abc"}));
        assert_eq!(ids(vec![started.clone(), other_commit]), [3, 2]);
        let earlier_job = update(1, UPDATE_STARTED, json!({"pid": 7, "commit": "def"}));
        assert_eq!(
            ids(vec![started.clone(), earlier_job, failed.clone()]),
            [3, 1, 2]
        );
        // A reused pid's earlier job of the same commit, before a retry,
        // is another job.
        let retried = update(2, UPDATE_RETRY, json!({"ask_id": 2, "answer": "retry"}));
        let earlier = update(1, UPDATE_FAILED, json!({"pid": 8, "commit": "def"}));
        assert_eq!(ids(vec![started.clone(), retried, earlier]), [3, 2, 1]);
        // The release update's job is ordered the same way.
        let release = json!({"pid": 8, "source": "release", "release": "0.5.0"});
        assert_eq!(
            ids(vec![
                update(5, UPDATE_STARTED, release.clone()),
                update(4, UPDATE_BUILT, release),
            ]),
            [4, 5]
        );
    }

    fn status_of_only(update: &RunEvent) -> Value {
        status(&[], std::slice::from_ref(update), &NoneAlive, 0)
    }

    /// A dropped request shows as `dropped`, and is no job's step: the
    /// job that ran before it is still the latest one.
    #[test]
    fn a_dropped_request_is_shown_and_is_no_jobs_step() {
        let started = update(
            1,
            UPDATE_STARTED,
            json!({"pid": 7, "source": "release", "release": "0.5.0"}),
        );
        let dropped = update(
            2,
            UPDATE_DROPPED,
            json!({"source": "release", "release": "0.4.0", "ask_id": 3, "plugin_only": true}),
        );
        let updates = [dropped, started];
        assert_eq!(status(&[], &updates, &NoneAlive, 0)["state"], "dropped");
        assert_eq!(latest_job_step(&updates).unwrap().kind, UPDATE_STARTED);
        assert_eq!(
            latest_job_step_of(&updates, true).unwrap().kind,
            UPDATE_STARTED
        );
    }

    /// A release's failure says whether a `retry` is about the plugin
    /// alone: a plugin stage or `plugin_only`; the build's failure of an
    /// older runtime says neither, and the automatic update's is none.
    #[test]
    fn a_release_failure_tells_whether_it_was_about_the_plugin() {
        let failed = |payload: Value| update(1, UPDATE_FAILED, payload);
        let release = |extra: Value| {
            let mut payload = json!({"source": "release", "release": "0.4.0", "ask_id": 3});
            if let (Some(payload), Value::Object(extra)) = (payload.as_object_mut(), extra) {
                payload.extend(extra);
            }
            failed(payload)
        };
        assert_eq!(
            failure_plugin_only(&release(json!({"stage": "plugin"}))),
            Some(true)
        );
        assert_eq!(
            failure_plugin_only(&release(
                json!({"stage": "interrupted", "plugin_only": false})
            )),
            Some(false)
        );
        assert_eq!(
            failure_plugin_only(&release(json!({"stage": "build"}))),
            None
        );
        assert_eq!(
            failure_plugin_only(&failed(json!({"stage": "plugin", "commit": "abc"}))),
            None
        );
        let updates = [release(json!({"stage": "build"}))];
        assert_eq!(
            failed_step(&updates, crate::domain::AskId::new(3)).map(|step| step.id),
            Some(updates[0].id)
        );
        assert!(failed_step(&updates, crate::domain::AskId::new(4)).is_none());
    }

    #[test]
    fn an_e2e_without_the_wrapper_is_said_by_the_job() {
        assert_eq!(
            wrapper_removed(&crate::application::install::E2eOutcome::default(), 9),
            None
        );
        let outcome = crate::application::install::E2eOutcome {
            sccache_wrapper_removed: Some((4300, "no sccache server listens on port 4300".into())),
            ..crate::application::install::E2eOutcome::default()
        };
        assert_eq!(
            wrapper_removed(&outcome, 9),
            Some(json!({"by": "update", "job": "e2e", "pid": 9, "port": 4300,
                        "reason": "no sccache server listens on port 4300"}))
        );
    }

    /// The asks a job's swap settles: a failure of a commit the installed
    /// one is or descends from. Not one whose commit is unknown (a release's,
    /// a person's install's, a step without one, one not among the updates),
    /// nor one the installed commit does not contain, nor one whose
    /// ancestry could not be told, which is returned for a warning.
    #[test]
    fn a_swap_settles_the_failures_of_the_commits_it_contains() {
        use crate::domain::AskId;
        let updates = vec![
            update(7, UPDATE_FAILED, json!({"ask_id": 7, "commit": "broken"})),
            update(
                6,
                UPDATE_FAILED,
                json!({"ask_id": 6, "commit": "elsewhere"}),
            ),
            update(
                5,
                UPDATE_FAILED,
                json!({"ask_id": 5, "commit": "unknown-to-git"}),
            ),
            update(
                4,
                UPDATE_FAILED,
                json!({"ask_id": 4, "commit": "installed"}),
            ),
            update(
                3,
                UPDATE_FAILED,
                json!({"ask_id": 3, "source": RELEASE_SOURCE, "release": "0.4.0"}),
            ),
            update(
                2,
                UPDATE_FAILED,
                json!({"ask_id": 2, "source": INSTALL_SOURCE}),
            ),
            update(1, UPDATE_FAILED, json!({"ask_id": 1})),
        ];
        let is_ancestor = |ancestor: &str, descendant: &str| -> Result<bool> {
            assert_eq!(descendant, "installed");
            match ancestor {
                "broken" | "installed" => Ok(true),
                "elsewhere" => Ok(false),
                _ => bail!("fatal: Not a valid commit name {ancestor}"),
            }
        };
        let asks: Vec<AskId> = (1..=8).map(AskId::new).collect();
        let (settled, unchecked) = settled_failures(&asks, &updates, "installed", &is_ancestor);
        assert_eq!(settled, vec![AskId::new(4), AskId::new(7)]);
        assert_eq!(
            unchecked.iter().map(|(ask, _)| *ask).collect::<Vec<_>>(),
            vec![AskId::new(5)]
        );
        assert_eq!(failed_commit(&updates, AskId::new(3)), None);
        assert_eq!(failed_commit(&updates, AskId::new(2)), None);
        assert_eq!(failed_commit(&updates, AskId::new(1)), None);
        assert_eq!(failed_commit(&updates, AskId::new(8)), None);
        assert_eq!(failed_commit(&updates, AskId::new(7)), Some("broken"));
    }
}
