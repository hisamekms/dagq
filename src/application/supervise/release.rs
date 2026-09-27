//! The supervisor's look for a new dagq release (ADR-t618-1 decisions 1 to
//! 3, [`crate::application::release_update`]): on its first pass and then
//! every [`RELEASE_LOOK`], a supervisor of a release build reads the
//! host's `[update]` and, unless it is off, starts a job thread that,
//! when a look is due for the queue, reads the index and records the
//! result. The loop does not
//! wait for it but, like the report job, before it ends or execs. Nothing
//! is claimed or held on it; a failure is only logged and recorded.
//!
//! What it found is acted on by [`Supervisor::release_update_pass`]
//! (decisions 4 and 5): the `approve_release` ask for a new release, its
//! answer and the `update_failed` answers of a release's job, and the job
//! (the hidden `release-update` command, [`crate::application::update::run_release`])
//! that installs a release in a session of its own, like the automatic
//! update's.

use super::*;
use crate::application::release_update::{self, ReleaseAction, ReleaseIndex};
use crate::application::update::{
    JOB_STEPS, RELEASE_SOURCE, UPDATE_ANSWERED, UPDATE_ASKER, UPDATE_FAILED, UPDATE_HISTORY,
    UPDATE_RETRY, UPDATE_STARTED, failed_release, in_progress, job_pid, latest_job_step,
    latest_job_step_of, record, step_release,
};
use crate::domain::release_update::{
    RELEASE_CHECKED, ReleaseMode, ReleaseUpdateConfig, is_release_build,
};
use crate::domain::{APPROVE_RELEASE_OPTIONS, AskKind, RunEvent, UPDATE_FAILED_OPTIONS};

/// How often the supervisor looks whether a look at the index is due.
pub const RELEASE_LOOK: Duration = Duration::from_secs(60);

/// What the supervisor looks for a release with.
#[derive(Clone)]
pub struct ReleasePort {
    /// The host's `[update]`, read again at each look.
    pub config: Arc<dyn Fn() -> ReleaseUpdateConfig + Send + Sync>,
    /// Reads crates.io's sparse index.
    pub index: Arc<dyn ReleaseIndex>,
    /// The supervisor's build identifier as the look takes it.
    pub current: String,
}

type ReleaseJob = thread::JoinHandle<Result<Option<(&'static str, Value)>>>;

/// The look running now and when the last one started; the release
/// update's job this process started and when its asks and answers were
/// last looked at.
#[derive(Default)]
pub(super) struct ReleaseWatch {
    job: Option<ReleaseJob>,
    last: Option<Instant>,
    install: Option<Box<dyn Spawned>>,
    last_update: Option<Instant>,
}

impl ReleaseWatch {
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }
}

impl Supervisor<'_> {
    /// Reap the look once it ended; start one on the first pass and every
    /// [`RELEASE_LOOK`] after, when `start`.
    pub(super) fn release_pass(&mut self, start: bool) {
        let Some(port) = self.release_port.clone() else {
            return;
        };
        if let Some(job) = self.release.job.take() {
            if !job.is_finished() {
                self.release.job = Some(job);
                return;
            }
            match job.join() {
                Ok(Ok(Some((kind, payload)))) => {
                    info!("release check: {kind} {payload}");
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => {
                    warn!(error = %format_args!("{error:#}"), "release check: {error:#}");
                }
                Err(_) => warn!("the release check panicked"),
            }
        }
        // Only a release build looks (ADR-t618-1 decision 1).
        if !start || !is_release_build(&port.current) {
            return;
        }
        let first_pass = self.release.last.is_none();
        if self
            .release
            .last
            .is_some_and(|last| last.elapsed() < RELEASE_LOOK)
        {
            return;
        }
        self.release.last = Some(Instant::now());
        let config = (port.config)();
        if config.release == ReleaseMode::Off {
            return;
        }
        let now = self.generators.clock.now();
        let queues = self.queues.clone();
        let token = self.token.clone();
        self.release.job = Some(spawn_traced(move || {
            let queue = queues.open()?;
            release_update::check(
                &*queue,
                &*port.index,
                &config,
                &port.current,
                now,
                first_pass,
                &token,
            )
        }));
    }
}

impl Supervisor<'_> {
    /// One look at what the release update has to do; what fails is logged
    /// and looked at again on the next pass.
    pub(super) fn release_update_pass(&mut self, options: &LoopSettings) {
        if let Err(error) = self.release_update(options) {
            warn!(error = %format_args!("{error:#}"), "release update: {error:#}");
        }
    }

    /// At most every `UpdateSettings::interval`, for a release build whose
    /// host does not turn `release` off: apply the answers, report a job
    /// that died, then start a job or ask as [`release_update::next_action`]
    /// says. One job of the update runs at a time, whichever started it.
    fn release_update(&mut self, options: &LoopSettings) -> Result<()> {
        if let Some(job) = self.release.install.as_mut()
            && let Some(exit) = job.try_wait()?
        {
            info!("release update job exited: {exit}");
            self.release.install = None;
        }
        let Some(port) = self.release_port.clone() else {
            return Ok(());
        };
        if self
            .release
            .last_update
            .is_some_and(|last| last.elapsed() < options.update.interval)
        {
            return Ok(());
        }
        self.release.last_update = Some(Instant::now());
        // A development build never acts on a release (ADR-t618-1
        // decision 1): its answers are left for a supervisor that does.
        if !is_release_build(&port.current) {
            return Ok(());
        }
        let config = (port.config)();
        if config.release == ReleaseMode::Off {
            return Ok(());
        }
        self.apply_release_answers()?;
        if self.release.install.is_some() {
            return Ok(());
        }
        let updates = self.queue.update_events(UPDATE_HISTORY)?;
        if let Some(step) = latest_job_step(&updates) {
            if let Some(pid) = job_pid(step) {
                self.processes.reap(pid);
            }
            if in_progress(step, &*self.processes) {
                return Ok(());
            }
        }
        // A job of a release that died, even when a build of the automatic
        // update ran after it.
        if let Some(step) = latest_job_step_of(&updates, true)
            && JOB_STEPS.contains(&step.kind.as_str())
        {
            if let Some(pid) = job_pid(step) {
                self.processes.reap(pid);
            }
            if !in_progress(step, &*self.processes)
                && let Some(version) = step_release(step)
            {
                return self.release_job_interrupted(step, version);
            }
        }
        let latest = self
            .queue
            .latest_queue_event(&[RELEASE_CHECKED])?
            .and_then(|event| event.payload["latest"].as_str().map(str::to_owned));
        let open: Vec<String> = self
            .queue
            .asks(AskQuery::default())?
            .into_iter()
            .filter(|ask| ask.kind == AskKind::ApproveRelease)
            .filter_map(|ask| ask.subject)
            .collect();
        match release_update::next_action(
            config.release,
            &port.current,
            latest.as_deref(),
            &updates,
            &open,
        ) {
            ReleaseAction::Start(version) => {
                self.start_release_job(&version, &port.current, options)
            }
            ReleaseAction::Ask(version) => self.open_release_ask(&version, &port.current),
            ReleaseAction::Nothing => Ok(()),
        }
    }

    /// Close the answered `approve_release` asks (`install` asks the next
    /// look to start the job, `skip` leaves the release) and the answered
    /// `update_failed` asks of a release's job (`retry` starts it again,
    /// `skip` leaves the release). Any other answer is left for the inbox.
    fn apply_release_answers(&mut self) -> Result<()> {
        for ask in self.queue.update_answers(&AskKind::ApproveRelease)? {
            let answer = ask.answer.as_deref().map(str::trim).unwrap_or_default();
            let Some(version) = ask.subject.clone() else {
                continue;
            };
            if !APPROVE_RELEASE_OPTIONS.contains(&answer) {
                continue;
            }
            self.record_release_answer(UPDATE_ANSWERED, ask.id, answer, &version)?;
        }
        let updates = self.queue.update_events(UPDATE_HISTORY)?;
        for ask in self.queue.update_answers(&AskKind::UpdateFailed)? {
            let Some(version) = failed_release(&updates, ask.id) else {
                continue;
            };
            let answer = ask.answer.as_deref().map(str::trim).unwrap_or_default();
            let kind = match answer {
                "retry" => UPDATE_RETRY,
                "skip" => UPDATE_ANSWERED,
                _ => continue,
            };
            self.record_release_answer(kind, ask.id, answer, version)?;
        }
        Ok(())
    }

    fn record_release_answer(
        &mut self,
        kind: &str,
        ask: crate::domain::AskId,
        answer: &str,
        version: &str,
    ) -> Result<()> {
        record(
            &*self.queue,
            kind,
            None,
            json!({
                "ask_id": ask,
                "answer": answer,
                "source": RELEASE_SOURCE,
                "release": version,
                "supervisor": self.token,
            }),
        )?;
        self.queue.close_ask(ask)?;
        info!(ask_id = %ask, "release update: ask {ask} answered {answer} for release {version}");
        Ok(())
    }

    /// Open the `approve_release` ask about `version`, closing the one of an
    /// older release still open.
    fn open_release_ask(&mut self, version: &str, current: &str) -> Result<()> {
        let question = format!(
            "dagq {version} is released on crates.io (https://crates.io/crates/dagq/{version}); \
this queue's supervisor runs {current}. Answer `install` to have the supervisor install it with \
`cargo install --locked dagq@{version}` under the queue's update/release directory and put it in \
place of its binary {} as `dagq install` does: the queue's compatible migrations are applied and \
the supervisor is handed over without stopping the runs, and the binary it replaced is kept as \
<name>.previous. A release that brings a breaking migration is not installed: it asks again \
(`approve_update`), since it needs the supervisor drained. Answer `skip` to leave {version}; the \
next release asks again.",
            self.layout.runner.display()
        );
        let ask = self.queue.open_update_ask(
            AskKind::ApproveRelease,
            &question,
            APPROVE_RELEASE_OPTIONS,
            UPDATE_ASKER,
            Some(version),
        )?;
        info!(ask_id = %ask.id, "release update: {version} is out; ask {} opened", ask.id);
        Ok(())
    }

    /// A job of a release that died before it recorded how it ended:
    /// record `update_failed` for it and ask the inbox, so it is neither
    /// lost nor tried again on its own.
    fn release_job_interrupted(&mut self, step: &RunEvent, version: &str) -> Result<()> {
        let question = format!(
            "The job that installs release {version} (pid {}) ended without recording how, at its \
{}; nothing tells whether the binary was replaced. Its logs are in the queue's logs/ directory. \
Answer `retry` to install release {version} again at the supervisor's next check, or `skip` to \
leave it (the next release asks again).",
            job_pid(step).unwrap_or_default(),
            step.kind
        );
        let ask = self.queue.open_update_ask(
            AskKind::UpdateFailed,
            &question,
            UPDATE_FAILED_OPTIONS,
            UPDATE_ASKER,
            None,
        )?;
        record(
            &*self.queue,
            UPDATE_FAILED,
            None,
            json!({
                "stage": "interrupted",
                "after": step.kind,
                "ask_id": ask.id,
                "supervisor": self.token,
                "source": RELEASE_SOURCE,
                "release": version,
            }),
        )?;
        warn!(
            "release update: the job for {version} was interrupted; ask {} opened",
            ask.id
        );
        Ok(())
    }

    /// Start the job that installs `version` in a session of its own, its
    /// output in the queue's `logs/`, and record `update_started`.
    fn start_release_job(
        &mut self,
        version: &str,
        current: &str,
        options: &LoopSettings,
    ) -> Result<()> {
        let layout = self.layout;
        let queue_dir = layout.db.parent().unwrap_or(Path::new("."));
        let logs = queue_dir.join("logs");
        self.files
            .create_dir_all(&logs)
            .with_context(|| format!("create {}", logs.display()))?;
        let name = format!("release-{}-{version}", self.generators.clock.now());
        let (log, build_log, report) = (
            logs.join(format!("{name}.log")),
            logs.join(format!("{name}.build.log")),
            logs.join(format!("{name}.json")),
        );
        let mut command = CommandSpec::new(&layout.runner);
        command
            .arg("--db")
            .arg(&layout.db)
            .arg("release-update")
            .args(["--release", version, "--token", self.token.as_str()])
            .envs(layout.supervisor_actor().env())
            .arg("--to")
            .arg(&layout.runner)
            .arg("--log")
            .arg(&build_log)
            .arg("--claude")
            .arg(&layout.claude)
            .arg("--codex")
            .arg(&layout.codex)
            .current_dir(&layout.repo_root)
            .new_session();
        if let Some(cmux) = &options.update.cmux {
            command.arg("--cmux").arg(cmux);
        }
        if let Some(dir) = &layout.plugin_dir {
            command.arg("--plugin-dir").arg(dir);
        }
        if let Some(cargo) = &options.update.cargo {
            command.arg("--cargo").arg(cargo);
        }
        let job = self.spawner.spawn(
            &command,
            Streams::Files {
                stdout: &report,
                stderr: &log,
            },
        )?;
        record(
            &*self.queue,
            UPDATE_STARTED,
            None,
            json!({
                "pid": job.id(),
                "source": RELEASE_SOURCE,
                "release": version,
                "supervisor": self.token,
                "version": current,
                "log": log,
                "build_log": build_log,
                "report": report,
            }),
        )?;
        info!(
            "release update: job {} installs release {version} (log {})",
            job.id(),
            log.display()
        );
        self.release.install = Some(job);
        Ok(())
    }
}
