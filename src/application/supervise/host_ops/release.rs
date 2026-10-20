//! The supervisor's look for a new dagq release (ADR-t618-1 decisions 1 to
//! 3, [`crate::application::release_update`]): on its first pass and then
//! every [`RELEASE_LOOK`], a supervisor of a release build reads the
//! host's `[update]` and, unless it is off, starts a job thread that,
//! when a look is due for the queue, reads the index and records the
//! result. The loop does not
//! wait for it but, like the report job, before it ends or execs. Nothing
//! is claimed or held on it; a failure is only logged and recorded.
//!
//! What it found is acted on by [`HostOpsState::release_update_pass`]
//! (decisions 4 and 5): the `approve_release` ask for a new release, its
//! answer and the `update_failed` answers of a release's job, and the job
//! (the hidden `release-update` command, [`crate::application::update::run_release`])
//! that installs a release in a session of its own, like the automatic
//! update's.

use super::*;
use crate::application::lifecycle;
use crate::application::release_update::{self, ReleaseAction, ReleaseIndex};
use crate::application::update::{
    JOB_STEPS, RELEASE_SOURCE, UPDATE_ASKER, UPDATE_HISTORY, failed_step, failure_plugin_only,
    in_progress, job_pid, latest_job_step, latest_job_step_of, record, step_release,
};
use crate::domain::EventKind;
use crate::domain::release_update::{
    RELEASE_CHECKED, ReleaseMode, ReleaseUpdateConfig, acts_on_releases, is_release_build,
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
    /// The dagq plugin installed in the supervisor's Claude Code; `None`
    /// when the supervisor loads one from `--plugin-dir`, which the release
    /// update never touches (ADR-t618-2 decision 3).
    pub plugin: Option<Arc<dyn crate::application::InstalledPlugin>>,
    /// The supervisor's build identifier as the look takes it.
    pub current: String,
}

type ReleaseJob = thread::JoinHandle<Result<Option<(&'static str, Value)>>>;

/// The look running now and when the last one started; the release
/// update's job this process started and when its asks and answers were
/// last looked at.
#[derive(Default)]
pub(in crate::application::supervise) struct ReleaseWatch {
    job: Option<ReleaseJob>,
    last: Option<Instant>,
    install: Option<Box<dyn Spawned>>,
    last_update: Option<Instant>,
}

impl ReleaseWatch {
    pub(in crate::application::supervise) const fn running(&self) -> bool {
        self.job.is_some()
    }
}

impl HostOpsState {
    /// Reap the look once it ended; start one on the first pass and every
    /// [`RELEASE_LOOK`] after, when `start`.
    pub(in crate::application::supervise) fn release_pass(
        &mut self,
        env: &mut HostEnv<'_>,
        start: bool,
    ) {
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
        let since_last = self.release.last.map(|at| env.generators.clock.since(at));
        if !look_due(since_last) {
            return;
        }
        self.release.last = Some(env.generators.clock.monotonic());
        let config = (port.config)();
        if config.release == ReleaseMode::Off {
            return;
        }
        let now = env.generators.clock.now();
        let queues = self.queues.clone();
        let token = env.token.clone();
        self.release.job = Some(spawn_traced(move || {
            let queue = queues.open()?;
            release_update::check(
                &*queue,
                &*port.index,
                port.plugin.as_deref(),
                &config,
                &port.current,
                now,
                first_pass,
                &token,
            )
        }));
    }
}

impl HostOpsState {
    /// One look at what the release update has to do; what fails is logged
    /// and looked at again on the next pass.
    pub(in crate::application::supervise) fn release_update_pass(
        &mut self,
        env: &mut HostEnv<'_>,
        options: &LoopSettings,
    ) {
        if let Err(error) = self.release_update(env, options) {
            warn!(error = %format_args!("{error:#}"), "release update: {error:#}");
        }
    }

    /// At most every `UpdateSettings::interval`, for a release build whose
    /// host does not turn `release` off: apply the answers, report a job
    /// that died, then start a job or ask as [`release_update::next_action`]
    /// says. One job of the update runs at a time, whichever started it.
    fn release_update(&mut self, env: &mut HostEnv<'_>, options: &LoopSettings) -> Result<()> {
        if let Some(job) = self.release.install.as_mut()
            && let Some(exit) = job.try_wait()?
        {
            info!("release update job exited: {exit}");
            self.release.install = None;
        }
        let Some(port) = self.release_port.clone() else {
            return Ok(());
        };
        let since_last = self
            .release
            .last_update
            .map(|at| env.generators.clock.since(at));
        if !update_due(since_last, options.update.interval) {
            return Ok(());
        }
        self.release.last_update = Some(env.generators.clock.monotonic());
        // A development build never acts on a release (ADR-t618-1
        // decision 1): its answers are left for a supervisor that does.
        if !is_release_build(&port.current) {
            return Ok(());
        }
        let config = (port.config)();
        if !acts_on_releases(&config, &port.current) {
            return Ok(());
        }
        self.apply_release_answers(env, &port.current)?;
        if self.release.install.is_some() {
            return Ok(());
        }
        let updates = env.queue.update_events(UPDATE_HISTORY)?;
        // Each job's pid is reaped just before it is looked at.
        let working = |step: &RunEvent| {
            if let Some(pid) = job_pid(step) {
                env.processes.reap(pid);
            }
            in_progress(step, &**env.processes)
        };
        match job_look(&updates, working) {
            JobLook::Running => return Ok(()),
            JobLook::Interrupted(step, version) => {
                return self.release_job_interrupted(env, step, version);
            }
            JobLook::None => {}
        }
        let checked = env.queue.latest_queue_event(&[RELEASE_CHECKED])?;
        let read = |key: &str| {
            checked
                .as_ref()
                .and_then(|event| event.payload[key].as_str().map(str::to_owned))
        };
        // A plugin read under another build is stale: the look after an
        // exec reads it again before the plugin is asked about.
        let plugin = read("plugin").filter(|_| read("current").as_deref() == Some(&port.current));
        let latest = read("latest");
        // Leave requests this build cannot run with their reason.
        for dropped in release_update::dropped_requests(&port.current, latest.as_deref(), &updates)
        {
            info!(
                "release update: {} for {} is not run by {}: {} (plugin_only: {})",
                dropped["request"],
                dropped["release"],
                port.current,
                dropped["reason"],
                dropped["plugin_only"]
            );
            let mut payload = dropped;
            payload["supervisor"] = json!(env.token);
            record(&*env.queue, EventKind::UpdateDropped, None, payload)?;
        }
        let open: Vec<String> = env
            .queue
            .asks(AskQuery::default())?
            .into_iter()
            .filter(|ask| ask.kind == AskKind::ApproveRelease)
            .filter_map(|ask| ask.subject)
            .collect();
        let mut action = release_update::next_action(
            config.release,
            &port.current,
            latest.as_deref(),
            &updates,
            &open,
        );
        if action == ReleaseAction::Nothing && port.plugin.is_some() {
            action = release_update::next_plugin_action(
                config.release,
                &port.current,
                latest.as_deref(),
                plugin.as_deref(),
                &updates,
                &open,
            );
        }
        match action {
            ReleaseAction::Start(version) => {
                self.start_release_job(env, &version, &port.current, options, false)
            }
            ReleaseAction::StartPlugin(version) => {
                self.start_release_job(env, &version, &port.current, options, true)
            }
            ReleaseAction::Ask(version) => self.open_release_ask(env, &version, &port.current),
            ReleaseAction::AskPlugin(version) => {
                self.open_plugin_ask(env, &version, plugin.as_deref().unwrap_or("older"))
            }
            ReleaseAction::Nothing => Ok(()),
        }
    }

    /// Close the answered `approve_release` asks (`install` asks the next
    /// look to start the job, `skip` leaves the release) and the answered
    /// `update_failed` asks of a release's job (`retry` starts it again,
    /// `skip` leaves the release). Any other answer is left for the inbox.
    /// Each says with `plugin_only` whether it is about the plugin alone
    /// (ADR-t618-2 decision 4): as the ask was opened for (the
    /// `plugin_only` of its `ask_opened`, or of the `update_failed` that
    /// opened it), whatever build applies it; for an ask opened before that
    /// was recorded, whether its release is the build `current`.
    fn apply_release_answers(&mut self, env: &mut HostEnv<'_>, current: &str) -> Result<()> {
        for ask in env.queue.update_answers(&AskKind::ApproveRelease)? {
            let answer = ask.answer.as_deref().map(str::trim).unwrap_or_default();
            let Some(version) = ask.subject.clone() else {
                continue;
            };
            if !APPROVE_RELEASE_OPTIONS.contains(&answer) {
                continue;
            }
            let recorded = env.queue.ask_opened_payload(ask.id)?["plugin_only"].as_bool();
            let plugin_only = release_update::answer_plugin_only(recorded, &version, current);
            self.record_release_answer(
                env,
                EventKind::UpdateAnswered,
                ask.id,
                answer,
                &version,
                plugin_only,
            )?;
        }
        let updates = env.queue.update_events(UPDATE_HISTORY)?;
        for ask in env.queue.update_answers(&AskKind::UpdateFailed)? {
            let Some(step) = failed_step(&updates, ask.id) else {
                continue;
            };
            let Some(version) = step_release(step) else {
                continue;
            };
            let answer = ask.answer.as_deref().map(str::trim).unwrap_or_default();
            let kind = match answer {
                "retry" => EventKind::UpdateRetry,
                "skip" => EventKind::UpdateAnswered,
                _ => continue,
            };
            let recorded = env.queue.ask_opened_payload(ask.id)?["plugin_only"]
                .as_bool()
                .or_else(|| failure_plugin_only(step));
            let plugin_only = release_update::answer_plugin_only(recorded, version, current);
            self.record_release_answer(env, kind, ask.id, answer, version, plugin_only)?;
        }
        Ok(())
    }

    fn record_release_answer(
        &mut self,
        env: &mut HostEnv<'_>,
        kind: EventKind,
        ask: crate::domain::AskId,
        answer: &str,
        version: &str,
        plugin_only: bool,
    ) -> Result<()> {
        let payload = json!({
            "ask_id": ask,
            "answer": answer,
            "source": RELEASE_SOURCE,
            "release": version,
            "plugin_only": plugin_only,
            "supervisor": env.token,
        });
        record(&*env.queue, kind, None, payload)?;
        env.queue.close_ask(ask)?;
        info!(ask_id = %ask, "release update: ask {ask} answered {answer} for release {version}");
        Ok(())
    }

    /// Open the `approve_release` ask about `version`, closing the one of an
    /// older release still open.
    fn open_release_ask(
        &mut self,
        env: &mut HostEnv<'_>,
        version: &str,
        current: &str,
    ) -> Result<()> {
        let ask = release_ask(version, current, &env.layout.runner);
        let ask = open(env, ask)?;
        info!(ask_id = %ask.id, "release update: {version} is out; ask {} opened", ask.id);
        Ok(())
    }

    /// Open the `approve_release` ask about bringing the installed plugin,
    /// at `plugin`, to `version`, the release the binary is already
    /// (ADR-t618-2 decision 4).
    fn open_plugin_ask(
        &mut self,
        env: &mut HostEnv<'_>,
        version: &str,
        plugin: &str,
    ) -> Result<()> {
        let ask = open(env, plugin_ask(version, plugin))?;
        info!(ask_id = %ask.id, "release update: the plugin {plugin} is older than {version}; ask {} opened", ask.id);
        Ok(())
    }

    /// A job of a release that died before it recorded how it ended:
    /// record `update_failed` for it and ask the inbox, so it is neither
    /// lost nor tried again on its own.
    fn release_job_interrupted(
        &mut self,
        env: &mut HostEnv<'_>,
        step: &RunEvent,
        version: &str,
    ) -> Result<()> {
        let ask = open(env, interrupted_ask(step, version))?;
        record(
            &*env.queue,
            EventKind::UpdateFailed,
            None,
            interrupted_failure(step, version, ask.id, env.token),
        )?;
        warn!(
            "release update: the job for {version} was interrupted; ask {} opened",
            ask.id
        );
        Ok(())
    }

    /// Start the job that installs `version` (only the plugin, with
    /// `plugin_only`) in a session of its own, its output in the queue's
    /// `logs/`, and record `update_started`.
    fn start_release_job(
        &mut self,
        env: &mut HostEnv<'_>,
        version: &str,
        current: &str,
        options: &LoopSettings,
        plugin_only: bool,
    ) -> Result<()> {
        let layout = env.layout;
        let queue_dir = layout.db.parent().unwrap_or(Path::new("."));
        let logs = queue_dir.join("logs");
        env.files
            .create_dir_all(&logs)
            .with_context(|| format!("create {}", logs.display()))?;
        let name = format!("release-{}-{version}", env.generators.clock.now());
        let (log, build_log, report) = (
            logs.join(format!("{name}.log")),
            logs.join(format!("{name}.build.log")),
            logs.join(format!("{name}.json")),
        );
        let command = release_job_command(
            layout,
            env.token,
            version,
            &build_log,
            &options.update,
            plugin_only,
        );
        let job = env.spawner.spawn(
            &command,
            Streams::Files {
                stdout: &report,
                stderr: &log,
            },
        )?;
        let mut started = json!({
            "pid": job.id(),
            "source": RELEASE_SOURCE,
            "release": version,
            "supervisor": env.token,
            "version": current,
            "log": log,
            "build_log": build_log,
            "report": report,
        });
        if plugin_only {
            started["plugin_only"] = json!(true);
        }
        record(&*env.queue, EventKind::UpdateStarted, None, started)?;
        info!(
            "release update: job {} installs release {version}{} (log {})",
            job.id(),
            if plugin_only {
                " (the plugin only)"
            } else {
                ""
            },
            log.display()
        );
        self.release.install = Some(job);
        Ok(())
    }
}

/// What the release update's jobs say before it acts.
#[derive(Debug)]
enum JobLook<'a> {
    /// The latest job step of any update is of a job still working: one
    /// job runs at a time, whichever started it.
    Running,
    /// The latest job step of a release's job is of a job that died
    /// before recording how it ended, even when a build of the automatic
    /// update ran after it: its step and release.
    Interrupted(&'a RunEvent, &'a str),
    None,
}

/// [`JobLook`] of the update events `updates`, newest first, with whether
/// a step is of a job still working (`in_progress`): asked of the latest
/// job step, then, unless it works, of the latest release step that is a
/// job step.
fn job_look<'a>(updates: &'a [RunEvent], in_progress: impl Fn(&RunEvent) -> bool) -> JobLook<'a> {
    if latest_job_step(updates).is_some_and(&in_progress) {
        return JobLook::Running;
    }
    match latest_job_step_of(updates, true) {
        Some(step) if step.kind.is_in(JOB_STEPS) && !in_progress(step) => {
            step_release(step).map_or(JobLook::None, |version| JobLook::Interrupted(step, version))
        }
        _ => JobLook::None,
    }
}

/// An ask of the release update, as [`RunLog::open_update_ask`] opens it.
#[derive(Debug, PartialEq)]
struct UpdateAsk {
    kind: AskKind,
    question: String,
    options: &'static [&'static str],
    subject: Option<String>,
    details: Value,
}

/// Open `ask` as the release update's.
fn open(env: &mut HostEnv<'_>, ask: UpdateAsk) -> Result<crate::domain::Ask> {
    env.queue.open_update_ask(
        ask.kind,
        &ask.question,
        ask.options,
        UPDATE_ASKER,
        ask.subject.as_deref(),
        ask.details,
    )
}

/// The `approve_release` ask about `version`, for a supervisor of build
/// `current` whose binary is `runner` (ADR-t618-1 decision 4).
fn release_ask(version: &str, current: &str, runner: &Path) -> UpdateAsk {
    let question = format!(
        "dagq {version} is released on crates.io (https://crates.io/crates/dagq/{version}); \
this queue's supervisor runs {current}. Answer `install` to have the supervisor install it with \
`cargo install --locked dagq@{version}` under the queue's update/release directory and put it in \
place of its binary {} as `dagq install` does: the queue's compatible migrations are applied and \
the supervisor is handed over without stopping the runs, and the binary it replaced is kept as \
<name>.previous. A release that brings a breaking migration is not installed: it asks again \
(`approve_update`), since it needs the supervisor drained. Answer `skip` to leave {version}; the \
next release asks again.",
        runner.display()
    );
    UpdateAsk {
        kind: AskKind::ApproveRelease,
        question,
        options: APPROVE_RELEASE_OPTIONS,
        subject: Some(version.to_owned()),
        details: json!({"plugin_only": false}),
    }
}

/// The `approve_release` ask about bringing the installed plugin, at
/// `plugin`, to `version`, the release the binary is already (ADR-t618-2
/// decision 4).
fn plugin_ask(version: &str, plugin: &str) -> UpdateAsk {
    let question = format!(
        "The {name} plugin installed in Claude Code is {plugin}, older than dagq {version} that \
this queue's supervisor runs. Answer `install` to have the supervisor bring only the plugin to the \
release with `claude {}` and `claude {}` (the binary is not touched); the inbox and planner \
sessions open now keep the plugin they started with, so reopen them afterwards to load it. Answer \
`skip` to leave the plugin as it is; the next release asks again.",
        lifecycle::PLUGIN_UPDATE_ARGUMENTS[0].join(" "),
        lifecycle::PLUGIN_UPDATE_ARGUMENTS[1].join(" "),
        name = lifecycle::DAGQ_PLUGIN,
    );
    UpdateAsk {
        kind: AskKind::ApproveRelease,
        question,
        options: APPROVE_RELEASE_OPTIONS,
        subject: Some(version.to_owned()),
        details: json!({"plugin_only": true}),
    }
}

/// The `update_failed` ask about the job of release `version` that died
/// at `step` without recording how it ended.
fn interrupted_ask(step: &RunEvent, version: &str) -> UpdateAsk {
    let plugin_only = step.payload["plugin_only"] == true;
    let question = if plugin_only {
        format!(
            "The job that brings the {} plugin to release {version} (pid {}) ended without \
recording how, at its {}; the binary was not touched. Its logs are in the queue's logs/ directory. \
Answer `retry` to update the plugin again at the supervisor's next check, or `skip` to leave it.",
            lifecycle::DAGQ_PLUGIN,
            job_pid(step).unwrap_or_default(),
            step.kind
        )
    } else {
        format!(
            "The job that installs release {version} (pid {}) ended without recording how, at \
its {}; nothing tells whether the binary was replaced. Its logs are in the queue's logs/ directory. \
Answer `retry` to install release {version} at the next check of a supervisor running an older \
build. A supervisor already on that release or newer records `update_dropped` instead; any older \
plugin is handled separately by auto mode or a plugin approval. Answer `skip` to \
leave it (the next release asks again).",
            job_pid(step).unwrap_or_default(),
            step.kind
        )
    };
    UpdateAsk {
        kind: AskKind::UpdateFailed,
        question,
        options: UPDATE_FAILED_OPTIONS,
        subject: None,
        details: json!({"plugin_only": plugin_only}),
    }
}

/// The `update_failed` of the job of release `version` that died at
/// `step`, recorded with its ask `ask` by `supervisor`.
fn interrupted_failure(
    step: &RunEvent,
    version: &str,
    ask: crate::domain::AskId,
    supervisor: &LeaseToken,
) -> Value {
    let plugin_only = step.payload["plugin_only"] == true;
    json!({
        "stage": if plugin_only { "plugin" } else { "interrupted" },
        "interrupted": true,
        "plugin_only": plugin_only,
        "after": step.kind,
        "ask_id": ask,
        "supervisor": supervisor,
        "source": RELEASE_SOURCE,
        "release": version,
    })
}

/// The command of the job that installs release `version` (only the
/// plugin, with `plugin_only`), started by the supervisor `token` with the
/// paths and providers of `layout`, cargo's output appended to
/// `build_log`: a supervisor the job starts again gets this one's
/// providers and plugin directory, and no `--cmux` (ADR-t2159-1 decision
/// 6).
fn release_job_command(
    layout: &Layout,
    token: &LeaseToken,
    version: &str,
    build_log: &Path,
    update: &UpdateSettings,
    plugin_only: bool,
) -> CommandSpec {
    let mut command = CommandSpec::new(&layout.runner);
    command
        .arg("--db")
        .arg(&layout.db)
        .arg("release-update")
        .args(["--release", version, "--token", token.as_str()])
        .envs(layout.supervisor_actor().env())
        // It opens the queue it names, not a client-mode `dagq`.
        .env_remove(crate::domain::queue_service::SOCKET_ENV)
        .arg("--to")
        .arg(&layout.runner)
        .arg("--log")
        .arg(build_log)
        .arg("--claude")
        .arg(&layout.claude)
        .arg("--codex")
        .arg(&layout.codex)
        .current_dir(&layout.repo_root)
        .new_session();
    if let Some(dir) = &layout.plugin_dir {
        command.arg("--plugin-dir").arg(dir);
    }
    if let Some(cargo) = &update.cargo {
        command.arg("--cargo").arg(cargo);
    }
    if plugin_only {
        command.arg("--plugin-only");
    }
    command
}

/// Whether the release is looked for now: on the first pass and
/// [`RELEASE_LOOK`] after the last look, given as the time since.
fn look_due(since_last: Option<Duration>) -> bool {
    since_last.is_none_or(|since| since >= RELEASE_LOOK)
}

/// Whether the release update looks again: on the first pass and
/// `interval` after the last look, given as the time since.
fn update_due(since_last: Option<Duration>, interval: Duration) -> bool {
    since_last.is_none_or(|since| since >= interval)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::update::{UPDATE_FAILED, UPDATE_STARTED};
    use crate::domain::{AskId, EventId};

    /// The release's job is the runtime binary's `release-update` of the
    /// release for this supervisor, given this supervisor's providers and
    /// plugin directory for a supervisor it starts again, and no `--cmux`
    /// (ADR-t2159-1 decision 6).
    #[test]
    fn the_release_job_is_given_the_supervisors_providers_and_no_cmux() {
        let layout = Layout::for_test(Path::new("/q"));
        let command = release_job_command(
            &layout,
            &LeaseToken::new("token-1"),
            "0.4.0",
            Path::new("/q/logs/release.build.log"),
            &UpdateSettings::default(),
            true,
        );
        assert_eq!(command.get_program(), "/opt/bin/dagq");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "--db",
                "/q/queue.db",
                "release-update",
                "--release",
                "0.4.0",
                "--token",
                "token-1",
                "--to",
                "/opt/bin/dagq",
                "--log",
                "/q/logs/release.build.log",
                "--claude",
                "/opt/bin/claude",
                "--codex",
                "/opt/bin/codex",
                "--plugin-dir",
                "/plugin",
                "--plugin-only",
            ]
        );
        assert!(!args.contains(&"--cmux"), "{args:?}");
    }

    /// The release is looked for on the first pass and at [`RELEASE_LOOK`]
    /// after the last look, and the release update at its interval, not
    /// 1 ms before.
    #[test]
    fn the_release_and_its_update_look_again_at_their_intervals() {
        let interval = Duration::from_secs(300);
        let ms = Duration::from_millis(1);
        assert!(look_due(None));
        assert!(!look_due(Some(RELEASE_LOOK - ms)));
        assert!(look_due(Some(RELEASE_LOOK)));
        assert!(update_due(None, interval));
        assert!(!update_due(Some(interval - ms), interval));
        assert!(update_due(Some(interval), interval));
    }

    fn step(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    fn started(pid: u32) -> RunEvent {
        step(
            UPDATE_STARTED,
            json!({"pid": pid, "source": RELEASE_SOURCE, "release": "0.4.0"}),
        )
    }

    fn release_of(look: JobLook<'_>) -> Option<(&str, &str)> {
        match look {
            JobLook::Interrupted(step, version) => Some((step.kind.as_str(), version)),
            JobLook::Running => Some(("running", "")),
            JobLook::None => None,
        }
    }

    /// A release's job that died without recording how it ended is
    /// reported, even when a step of the automatic update came after it;
    /// once its `update_failed` is recorded it is neither reported nor
    /// started again. A job still working, of either update, holds the
    /// release update back.
    #[test]
    fn a_release_job_that_died_is_reported_once_and_a_live_one_waits() {
        let alive = |pids: &'static [u32]| {
            move |step: &RunEvent| {
                step.kind.is_in(JOB_STEPS) && job_pid(step).is_some_and(|pid| pids.contains(&pid))
            }
        };
        // Newest first.
        let died = [
            step(
                UPDATE_FAILED,
                json!({"stage": "build", "commit": "abc", "ask_id": 0}),
            ),
            started(7),
        ];
        assert_eq!(
            release_of(job_look(&died, alive(&[]))),
            Some((UPDATE_STARTED, "0.4.0"))
        );
        assert_eq!(
            release_of(job_look(&died[1..], alive(&[7]))),
            Some(("running", ""))
        );
        // An automatic update's job still working.
        let auto = [
            step(UPDATE_STARTED, json!({"pid": 8, "commit": "abc"})),
            started(7),
        ];
        assert_eq!(
            release_of(job_look(&auto, alive(&[8]))),
            Some(("running", ""))
        );
        let reported = [
            step(
                UPDATE_FAILED,
                json!({"stage": "interrupted", "interrupted": true, "after": UPDATE_STARTED, "ask_id": 3, "source": RELEASE_SOURCE, "release": "0.4.0"}),
            ),
            died[0].clone(),
            started(7),
        ];
        assert_eq!(release_of(job_look(&reported, alive(&[]))), None);
        assert_eq!(release_of(job_look(&[], alive(&[]))), None);
    }

    /// The ask about a new release names it, the build that runs and the
    /// command that installs it; the ask about the plugin alone says the
    /// binary is not touched. Both are `approve_release` with the options
    /// `install` and `skip`, about their release, recording what they are
    /// for.
    #[test]
    fn the_release_and_plugin_asks_say_what_install_does() {
        let ask = release_ask("0.4.0", "0.3.0", Path::new("/bin/dagq"));
        assert_eq!(ask.kind, AskKind::ApproveRelease);
        assert_eq!(ask.options, ["install", "skip"]);
        assert_eq!(ask.subject.as_deref(), Some("0.4.0"));
        assert_eq!(ask.details, json!({"plugin_only": false}));
        assert!(
            ask.question.starts_with(
                "dagq 0.4.0 is released on crates.io (https://crates.io/crates/dagq/0.4.0); this queue's supervisor runs 0.3.0."
            ) && ask.question.contains("`cargo install --locked dagq@0.4.0`")
                && ask.question.contains("in place of its binary /bin/dagq"),
            "{}",
            ask.question
        );
        let ask = plugin_ask("0.4.0", "0.3.0");
        assert_eq!(ask.kind, AskKind::ApproveRelease);
        assert_eq!(ask.options, ["install", "skip"]);
        assert_eq!(ask.subject.as_deref(), Some("0.4.0"));
        assert_eq!(ask.details, json!({"plugin_only": true}));
        assert!(
            ask.question
                .starts_with("The claude-dagq plugin installed in Claude Code is 0.3.0, older than dagq 0.4.0")
                && ask.question.contains(
                    "`claude plugin marketplace update dagq` and `claude plugin update claude-dagq@dagq` (the binary is not touched)"
                ),
            "{}",
            ask.question
        );
    }

    /// The job that died is the inbox's through `update_failed` (stage
    /// `interrupted`, or `plugin` for the plugin alone) and its ask, which
    /// names the release, the pid and the step it died at.
    #[test]
    fn an_interrupted_job_is_reported_with_its_release() {
        let binary = started(7);
        let ask = interrupted_ask(&binary, "0.4.0");
        assert_eq!(ask.kind, AskKind::UpdateFailed);
        assert_eq!(ask.options, UPDATE_FAILED_OPTIONS);
        assert_eq!(ask.subject, None);
        assert_eq!(ask.details, json!({"plugin_only": false}));
        assert!(
            ask.question.starts_with(
                "The job that installs release 0.4.0 (pid 7) ended without recording how, at its update_started;"
            ) && ask.question.contains("records `update_dropped` instead"),
            "{}",
            ask.question
        );
        assert_eq!(
            interrupted_failure(&binary, "0.4.0", AskId::new(3), &LeaseToken::new("me")),
            json!({
                "stage": "interrupted",
                "interrupted": true,
                "plugin_only": false,
                "after": "update_started",
                "ask_id": 3,
                "supervisor": "me",
                "source": "release",
                "release": "0.4.0",
            })
        );
        let plugin = step(
            UPDATE_STARTED,
            json!({"pid": 9, "source": RELEASE_SOURCE, "release": "0.4.0", "plugin_only": true}),
        );
        let ask = interrupted_ask(&plugin, "0.4.0");
        assert_eq!(ask.details, json!({"plugin_only": true}));
        assert!(
            ask.question.starts_with(
                "The job that brings the claude-dagq plugin to release 0.4.0 (pid 9) ended without recording how"
            ),
            "{}",
            ask.question
        );
        let failure = interrupted_failure(&plugin, "0.4.0", AskId::new(4), &LeaseToken::new("me"));
        assert_eq!(failure["stage"], "plugin");
        assert_eq!(failure["plugin_only"], true);
    }
}
