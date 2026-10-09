//! The supervisor's side of the automatic update (ADR-0045 decision 17):
//! on a pass at most every `UpdateSettings::interval`, with `auto_update`
//! on its registration, it applies the answers of the `update_failed`
//! asks, and starts the update job ([`crate::application::update::run`])
//! when main moved past the commit it last updated to and the commits in
//! between change the runtime. The job runs in a session of its own and
//! outlives an exec of this process; one job runs at a time, and a landing
//! during it is built once it ended (the latest head, once). Only in
//! dagq's source repository (ADR-t614-1); elsewhere it builds nothing.

use super::*;
use crate::application::update::{
    JOB_STEPS, UPDATE_ASKER, UPDATE_HISTORY, base_commit, changes_runtime, failed_release,
    in_progress, job_pid, latest_job_step, latest_job_step_of, record, retry_requested,
    step_commit,
};
use crate::domain::EventKind;
use crate::domain::{AskKind, RunEvent, UPDATE_FAILED_OPTIONS};

/// How often the supervisor looks at main for an update by default.
pub const UPDATE_INTERVAL: Duration = Duration::from_secs(30);

/// How a supervisor updates its own binary (ADR-0045 decision 17).
#[derive(Debug, Clone)]
pub struct UpdateSettings {
    /// Register with the automatic update on (`supervise --auto-update`,
    /// which `up --auto-update` starts); `up` turns it on and off on a
    /// registered supervisor too.
    pub register: bool,
    /// Least time between two looks at main.
    pub interval: Duration,
    /// A shell command the job runs in place of `cargo build --release
    /// --locked` (tests).
    pub build_command: Option<String>,
    /// A shell command the job runs in place of the e2e (`cargo test
    /// --locked --test e2e -- --ignored`, ADR-t963-1 decision 1; tests).
    pub e2e_command: Option<String>,
    /// How long the job's e2e may run; `None` is the job's default.
    pub e2e_timeout: Option<Duration>,
    /// How often the job looks at the handoff and at the new supervisor's
    /// heartbeat; `None` is the job's default (tests shorten it, task
    /// 1048).
    pub poll: Option<Duration>,
    /// The cmux the job's `up` is given when it starts a supervisor
    /// registered in the retired in-cmux mode again.
    pub cmux: Option<PathBuf>,
    /// The cargo the release update's job installs a release with
    /// (ADR-t618-1 decision 5); `None` is `cargo`. Tests give a stub.
    pub cargo: Option<PathBuf>,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            register: false,
            interval: UPDATE_INTERVAL,
            build_command: None,
            e2e_command: None,
            e2e_timeout: None,
            poll: None,
            cmux: None,
            cargo: None,
        }
    }
}

/// What the supervisor remembers between its looks at main.
#[derive(Default)]
pub(super) struct UpdateWatch {
    last_check: Option<Instant>,
    /// The job this process started, reaped once it exits.
    job: Option<Box<dyn Spawned>>,
    /// A head already found to change nothing of the runtime.
    seen: Option<String>,
    /// Main when this process first looked: the base of a build that names
    /// no commit (a release, or `+unknown`) and has updated nothing yet.
    first_head: Option<String>,
    /// The repository was found not to be dagq's source, and that was
    /// logged; cleared once it is again.
    not_source: bool,
    /// Main's head the last look past the source check logged as building
    /// nothing, so each head is logged once (tests wait for the line
    /// instead of a fixed sleep, task 1048).
    passed: Option<String>,
}

impl HostOpsState {
    /// One look at main for the automatic update; what fails is logged and
    /// looked at again on the next pass.
    pub(super) fn auto_update_pass(&mut self, env: &mut HostEnv<'_>, options: &LoopSettings) {
        if let Err(error) = self.auto_update(env, options) {
            warn!(error = %format_args!("{error:#}"), "automatic update: {error:#}");
        }
    }

    fn auto_update(&mut self, env: &mut HostEnv<'_>, options: &LoopSettings) -> Result<()> {
        if let Some(job) = self.update.job.as_mut()
            && let Some(exit) = job.try_wait()?
        {
            info!("automatic update job exited: {exit}");
            self.update.job = None;
        }
        if self
            .update
            .last_check
            .is_some_and(|last| last.elapsed() < options.update.interval)
        {
            return Ok(());
        }
        self.update.last_check = Some(Instant::now());
        let enabled = env
            .queue
            .supervisors()?
            .iter()
            .any(|registration| registration.token == *env.token && registration.auto_update);
        if !enabled {
            return Ok(());
        }
        // Only dagq's source builds dagq (ADR-t614-1): elsewhere the
        // registration's auto_update builds nothing.
        if !env.repository.is_dagq_source() {
            if !self.update.not_source {
                warn!(
                    "automatic update: the repository is not dagq's source (its Cargo.toml has no [package] named dagq), so nothing is built; update dagq with `cargo install dagq` or `dagq install --from`"
                );
            }
            self.update.not_source = true;
            // Read after the source check, so a head logged here was looked
            // at as not dagq's source.
            if let Ok(head) = env.repository.main_head() {
                self.log_nothing_built(head.as_str(), "the repository is not dagq's source");
            }
            return Ok(());
        }
        self.update.not_source = false;
        self.apply_update_answers(env)?;
        if self.update.job.is_some() {
            return Ok(());
        }
        let updates = env.queue.update_events(50)?;
        if let Some(step) = latest_job_step(&updates) {
            // A job this process started before it exec'd is its child
            // with no other reaper: collect it once it ended.
            if let Some(pid) = job_pid(step) {
                env.processes.reap(pid);
            }
            if in_progress(step, &**env.processes) {
                return Ok(());
            }
        }
        // Of the automatic update's jobs only: the release update's is the
        // release pass's to report.
        if let Some(step) = latest_job_step_of(&updates, false)
            && JOB_STEPS.contains(&step.kind.as_str())
        {
            if let Some(pid) = job_pid(step) {
                env.processes.reap(pid);
            }
            if !in_progress(step, &**env.processes) {
                return self.job_interrupted(env, step);
            }
        }
        let head = env.repository.main_head()?.to_string();
        let first = self
            .update
            .first_head
            .get_or_insert_with(|| head.clone())
            .clone();
        let base = base_commit(&updates, &env.layout.version, Some(&first));
        if !retry_requested(&updates) {
            if base.as_deref() == Some(head.as_str())
                || self.update.seen.as_deref() == Some(head.as_str())
            {
                return Ok(());
            }
            let changes = match &base {
                Some(base) => match env.repository.changed_paths(base, &head) {
                    Ok(paths) => changes_runtime(&paths),
                    // A base Git does not know (a build of another clone):
                    // build main's head once, which becomes the base.
                    Err(error) => {
                        warn!(error = %format_args!("{error:#}"), "automatic update: what changed since {base} is unknown, so main's {head} is built: {error:#}");
                        true
                    }
                },
                None => true,
            };
            if !changes {
                self.log_nothing_built(&head, "it changes no runtime path");
                self.update.seen = Some(head);
                return Ok(());
            }
        }
        self.start_update_job(env, &head, base.as_deref(), options)
    }

    /// Log once per head that a look at main's `head` builds nothing, and
    /// why.
    fn log_nothing_built(&mut self, head: &str, why: &str) {
        if self.update.passed.as_deref() != Some(head) {
            info!("automatic update: main's {head} builds nothing: {why}");
            self.update.passed = Some(head.to_owned());
        }
    }

    /// A job that died before it recorded how it ended (killed, a reboot):
    /// record `update_failed` for its commit and ask the inbox, so the
    /// update is neither lost nor retried on its own.
    fn job_interrupted(&mut self, env: &mut HostEnv<'_>, step: &RunEvent) -> Result<()> {
        let commit = step_commit(step).unwrap_or_default().to_owned();
        let question = format!(
            "The automatic update's job for main's {} (pid {}) ended without recording how, at \
its {}; nothing tells whether the binary was replaced. Its logs are in the queue's logs/ \
directory. Answer `retry` to build main's head again at the supervisor's next check, or `skip` \
to wait for the next landing that changes the runtime.",
            &commit[..commit.len().min(12)],
            job_pid(step).unwrap_or_default(),
            step.kind
        );
        let ask = env.queue.open_update_ask(
            AskKind::UpdateFailed,
            &question,
            UPDATE_FAILED_OPTIONS,
            UPDATE_ASKER,
            None,
            Value::Null,
        )?;
        record(
            &*env.queue,
            EventKind::UpdateFailed,
            step_commit(step),
            json!({"stage": "interrupted", "after": step.kind, "ask_id": ask.id, "supervisor": env.token}),
        )?;
        warn!(
            "automatic update: the job for {commit} was interrupted; ask {} opened",
            ask.id
        );
        Ok(())
    }

    /// Close the answered `update_failed` asks: `retry` asks the next look
    /// to build main's head again, `skip` waits for the next landing. Any
    /// other answer is left for the inbox to read.
    fn apply_update_answers(&mut self, env: &mut HostEnv<'_>) -> Result<()> {
        let updates = env.queue.update_events(UPDATE_HISTORY)?;
        for ask in env.queue.update_answers(&AskKind::UpdateFailed)? {
            // The failure of a release's job is the release pass's, and
            // a person's install's the inbox's alone: its answer builds
            // nothing (ADR-0073 decision 14).
            if failed_release(&updates, ask.id).is_some()
                || crate::application::update::failed_step(&updates, ask.id)
                    .is_some_and(crate::application::update::step_install)
            {
                continue;
            }
            let answer = ask.answer.as_deref().map(str::trim).unwrap_or_default();
            let kind = match answer {
                "retry" => EventKind::UpdateRetry,
                "skip" => EventKind::UpdateAnswered,
                _ => continue,
            };
            record(
                &*env.queue,
                kind,
                None,
                json!({"ask_id": ask.id, "answer": answer, "supervisor": env.token}),
            )?;
            env.queue.close_ask(ask.id)?;
            info!(ask_id = %ask.id, "automatic update: ask {} answered {answer}", ask.id);
        }
        Ok(())
    }

    /// Start the update job for main's `head` in a session of its own, its
    /// output in the queue's `logs/`, and record `update_started`.
    fn start_update_job(
        &mut self,
        env: &mut HostEnv<'_>,
        head: &str,
        base: Option<&str>,
        options: &LoopSettings,
    ) -> Result<()> {
        let layout = env.layout;
        let queue_dir = layout.db.parent().unwrap_or(Path::new("."));
        let logs = queue_dir.join("logs");
        env.files
            .create_dir_all(&logs)
            .with_context(|| format!("create {}", logs.display()))?;
        let name = format!(
            "update-{}-{}",
            env.generators.clock.now(),
            &head[..head.len().min(12)]
        );
        let (log, build_log, report) = (
            logs.join(format!("{name}.log")),
            logs.join(format!("{name}.build.log")),
            logs.join(format!("{name}.json")),
        );
        let mut command = CommandSpec::new(&layout.runner);
        command
            .arg("--db")
            .arg(&layout.db)
            .arg("auto-update")
            .args(["--commit", head, "--token", env.token.as_str()])
            .envs(layout.supervisor_actor().env())
            // It opens the queue it names, not a client-mode `dagq`.
            .env_remove(crate::domain::queue_service::SOCKET_ENV)
            .arg("--to")
            .arg(&layout.runner)
            .arg("--repo")
            .arg(&layout.repo_root)
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
        if let Some(build) = &options.update.build_command {
            command.arg("--build-command").arg(build);
        }
        if let Some(e2e) = &options.update.e2e_command {
            command.arg("--e2e-command").arg(e2e);
        }
        if let Some(timeout) = options.update.e2e_timeout {
            command
                .arg("--e2e-timeout")
                .arg(timeout.as_secs().to_string());
        }
        if let Some(poll) = options.update.poll {
            command.arg("--poll-ms").arg(poll.as_millis().to_string());
        }
        let job = env.spawner.spawn(
            &command,
            Streams::Files {
                stdout: &report,
                stderr: &log,
            },
        )?;
        record(
            &*env.queue,
            EventKind::UpdateStarted,
            Some(head),
            json!({
                "pid": job.id(),
                "base": base,
                "supervisor": env.token,
                "version": layout.version,
                "log": log,
                "build_log": build_log,
                "report": report,
            }),
        )?;
        info!(
            "automatic update: main's {head} changes the runtime since {}; job {} builds it (log {})",
            base.unwrap_or("(none)"),
            job.id(),
            log.display()
        );
        self.update.job = Some(job);
        Ok(())
    }
}
