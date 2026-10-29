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

/// How many of the newest update events a look at the automatic update's
/// jobs reads.
const JOB_LOOK_BACK: usize = 50;

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
            cargo: None,
        }
    }
}

/// What the supervisor remembers between its looks at main.
#[derive(Default)]
pub(in crate::application::supervise) struct UpdateWatch {
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
    /// The landing checks logged as waiting for the update's gate
    /// (ADR-t2719-1), so each is logged once a wait; cleared once the gate
    /// holds none.
    held_checks: Vec<String>,
}

impl UpdateWatch {
    /// Log once per `check` (a landing check that waits) that it waits for
    /// `what`; `None` once nothing holds the check, so the next wait is
    /// logged again.
    pub(in crate::application::supervise) fn tell_held_check(
        &mut self,
        check: &str,
        what: Option<crate::domain::heavy_turn::HeavyJob>,
    ) {
        match what {
            Some(what) if !self.held_checks.iter().any(|held| held == check) => {
                info!("{check} waits for {} to end", what.describe());
                self.held_checks.push(check.to_owned());
            }
            Some(_) => {}
            None => self.held_checks.clear(),
        }
    }
}

impl HostOpsState {
    /// One look at main for the automatic update; what fails is logged and
    /// looked at again on the next pass.
    pub(in crate::application::supervise) fn auto_update_pass(
        &mut self,
        env: &mut HostEnv<'_>,
        options: &LoopSettings,
    ) {
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
        let since_last = self
            .update
            .last_check
            .map(|at| env.generators.clock.since(at));
        if !check_due(since_last, options.update.interval) {
            return Ok(());
        }
        self.update.last_check = Some(env.generators.clock.monotonic());
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
        let Some(updates) = look_at_job(&mut *env.queue, &**env.processes, env.token)? else {
            return Ok(());
        };
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
    /// why. Tests wait for this line rather than for a fixed sleep.
    fn log_nothing_built(&mut self, head: &str, why: &str) {
        if self.update.passed.as_deref() != Some(head) {
            info!("automatic update: main's {head} builds nothing: {why}");
            self.update.passed = Some(head.to_owned());
        }
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
        let command = update_job_command(layout, env.token, head, &build_log, &options.update);
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

/// The update events a look at the automatic update's jobs reads, newest
/// first, or `None` when the look ends at its job: the latest job step of
/// any update is of a job still working, or the automatic update's latest
/// is of a job found gone. That job is reported interrupted
/// ([`job_interrupted`]) only when its step is still the latest in the
/// events read again after its pid was looked at. Each job's pid is reaped
/// just before it is looked at.
fn look_at_job(
    queue: &mut (dyn HostOpsQueue + Send),
    processes: &dyn ProcessControl,
    token: &LeaseToken,
) -> Result<Option<Vec<RunEvent>>> {
    let updates = queue.update_events(JOB_LOOK_BACK)?;
    if let Some(step) = latest_job_step(&updates) {
        // A job this process started before it exec'd is its child
        // with no other reaper: collect it once it ended.
        if let Some(pid) = job_pid(step) {
            processes.reap(pid);
        }
        if in_progress(step, processes) {
            return Ok(None);
        }
    }
    // Of the automatic update's jobs only: the release update's is the
    // release pass's to report.
    if let Some(step) = latest_job_step_of(&updates, false)
        && step.kind.is_in(JOB_STEPS)
    {
        if let Some(pid) = job_pid(step) {
            processes.reap(pid);
        }
        if !in_progress(step, processes) {
            // The job may have recorded how it ended and exited after
            // the events were read: only a step still its latest once
            // its process is known gone tells it was interrupted.
            let again = queue.update_events(JOB_LOOK_BACK)?;
            if still_latest_step(step, &again, false) {
                job_interrupted(queue, token, step)?;
            }
            return Ok(None);
        }
    }
    Ok(Some(updates))
}

/// A job that died before it recorded how it ended (killed, a reboot):
/// record `update_failed` for its commit and ask the inbox, so the
/// update is neither lost nor retried on its own.
fn job_interrupted(
    queue: &mut (dyn HostOpsQueue + Send),
    token: &LeaseToken,
    step: &RunEvent,
) -> Result<()> {
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
    let ask = queue.open_update_ask(
        AskKind::UpdateFailed,
        &question,
        UPDATE_FAILED_OPTIONS,
        UPDATE_ASKER,
        None,
        Value::Null,
    )?;
    record(
        &*queue,
        EventKind::UpdateFailed,
        step_commit(step),
        json!({"stage": "interrupted", "after": step.kind, "ask_id": ask.id, "supervisor": token}),
    )?;
    warn!(
        "automatic update: the job for {commit} was interrupted; ask {} opened",
        ask.id
    );
    Ok(())
}

/// The command of the automatic update's job for main's `head`, started by
/// the supervisor `token` with the paths and providers of `layout`, the
/// build's output appended to `build_log`: a supervisor the job starts
/// again gets this one's providers and plugin directory, and no `--cmux`
/// (ADR-t2159-1 decision 6).
fn update_job_command(
    layout: &Layout,
    token: &LeaseToken,
    head: &str,
    build_log: &Path,
    update: &UpdateSettings,
) -> CommandSpec {
    let mut command = CommandSpec::new(&layout.runner);
    command
        .arg("--db")
        .arg(&layout.db)
        .arg("auto-update")
        .args(["--commit", head, "--token", token.as_str()])
        .envs(layout.supervisor_actor().env())
        // It opens the queue it names, not a client-mode `dagq`.
        .env_remove(crate::domain::queue_service::SOCKET_ENV)
        .arg("--to")
        .arg(&layout.runner)
        .arg("--repo")
        .arg(&layout.repo_root)
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
    if let Some(build) = &update.build_command {
        command.arg("--build-command").arg(build);
    }
    if let Some(e2e) = &update.e2e_command {
        command.arg("--e2e-command").arg(e2e);
    }
    if let Some(timeout) = update.e2e_timeout {
        command
            .arg("--e2e-timeout")
            .arg(timeout.as_secs().to_string());
    }
    if let Some(poll) = update.poll {
        command.arg("--poll-ms").arg(poll.as_millis().to_string());
    }
    command
}

/// Whether `step`, a job step of the release update's jobs (`release`) or
/// of the automatic update's whose process was then found gone, is still
/// the newest step of that kind in `updates`, read after that look: a job
/// that recorded how it ended (`update_installed`, `update_failed`, …) and
/// exited in between was not interrupted.
pub(super) fn still_latest_step(step: &RunEvent, updates: &[RunEvent], release: bool) -> bool {
    latest_job_step_of(updates, release).is_some_and(|latest| latest.id == step.id)
}

/// Whether the automatic update looks again: on the first pass and
/// `interval` after the last look, given as the time since.
fn check_due(since_last: Option<Duration>, interval: Duration) -> bool {
    since_last.is_none_or(|since| since >= interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The automatic update's job is the runtime binary's `auto-update` of
    /// main's head for this supervisor, given this supervisor's providers
    /// and plugin directory for a supervisor it starts again, and no
    /// `--cmux` (ADR-t2159-1 decision 6).
    #[test]
    fn the_update_job_is_given_the_supervisors_providers_and_no_cmux() {
        let layout = Layout::for_test(Path::new("/q"));
        let update = UpdateSettings {
            poll: Some(Duration::from_millis(20)),
            ..UpdateSettings::default()
        };
        let command = update_job_command(
            &layout,
            &LeaseToken::new("token-1"),
            "abc123",
            Path::new("/q/logs/update.build.log"),
            &update,
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
                "auto-update",
                "--commit",
                "abc123",
                "--token",
                "token-1",
                "--to",
                "/opt/bin/dagq",
                "--repo",
                "/repo",
                "--log",
                "/q/logs/update.build.log",
                "--claude",
                "/opt/bin/claude",
                "--codex",
                "/opt/bin/codex",
                "--plugin-dir",
                "/plugin",
                "--poll-ms",
                "20",
            ]
        );
        assert!(!args.contains(&"--cmux"), "{args:?}");
    }

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: crate::domain::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    /// A job found gone after its `update_e2e_passed` was read, which
    /// recorded `update_installed` (or `update_failed`) before it exited,
    /// is not interrupted; one whose `update_e2e_passed` is still its
    /// newest step once it is gone is, even when the release update's job
    /// or an answer wrote after it.
    #[test]
    fn a_job_that_recorded_its_end_before_it_exited_is_not_interrupted() {
        use crate::domain::{
            UPDATE_ANSWERED, UPDATE_E2E_PASSED, UPDATE_FAILED, UPDATE_INSTALLED, UPDATE_STARTED,
        };
        let passed = event(2, UPDATE_E2E_PASSED, json!({"pid": 7, "commit": "abc"}));
        let read = [
            passed.clone(),
            event(1, UPDATE_STARTED, json!({"pid": 7, "commit": "abc"})),
        ];
        assert!(still_latest_step(&passed, &read, false));
        for end in [UPDATE_INSTALLED, UPDATE_FAILED] {
            let mut again = vec![event(3, end, json!({"pid": 7, "commit": "abc"}))];
            again.extend(read.iter().cloned());
            assert!(!still_latest_step(&passed, &again, false), "{end}");
        }
        let mut again = vec![
            event(4, UPDATE_ANSWERED, json!({"ask_id": 1})),
            event(
                3,
                UPDATE_STARTED,
                json!({"pid": 9, "source": "release", "release": "0.4.0"}),
            ),
        ];
        again.extend(read.iter().cloned());
        assert!(still_latest_step(&passed, &again, false));
        // A release's job the same way, apart from the automatic update's.
        let release = json!({"pid": 9, "source": "release", "release": "0.4.0"});
        let started = event(5, UPDATE_STARTED, release.clone());
        let read = [
            event(6, UPDATE_STARTED, json!({"pid": 8, "commit": "def"})),
            started.clone(),
        ];
        assert!(still_latest_step(&started, &read, true));
        let mut again = vec![event(7, UPDATE_INSTALLED, release)];
        again.extend(read.iter().cloned());
        assert!(!still_latest_step(&started, &again, true));
    }

    /// A look at the automatic update's jobs reads the events again once
    /// it found the job's pid gone: a job whose `update_e2e_passed` was
    /// read while it worked, and which recorded how it ended and exited
    /// before its pid was looked at, is not reported; one whose
    /// `update_e2e_passed` is still its latest step after that look is
    /// reported interrupted, with an `update_failed` ask.
    #[test]
    fn a_look_at_the_job_reads_the_events_again_after_its_pid() {
        use super::super::job_fakes::JobQueue;
        use crate::domain::{UPDATE_E2E_PASSED, UPDATE_FAILED, UPDATE_INSTALLED, UPDATE_STARTED};
        let token = LeaseToken::new("token-1");
        let job = json!({"pid": 7, "commit": "abc"});
        let read = vec![
            event(2, UPDATE_E2E_PASSED, job.clone()),
            event(1, UPDATE_STARTED, job.clone()),
        ];
        for end in [UPDATE_INSTALLED, UPDATE_FAILED] {
            let mut after = vec![event(3, end, job.clone())];
            after.extend(read.iter().cloned());
            let (mut queue, processes) = JobQueue::new(read.clone(), after);
            let look = look_at_job(&mut queue, &processes, &token).unwrap();
            assert!(look.is_none(), "{end}");
            assert_eq!(*queue.recorded.lock().unwrap(), [], "{end}");
            assert_eq!(*queue.asks.lock().unwrap(), [], "{end}");
        }
        let (mut queue, processes) = JobQueue::new(read.clone(), read);
        let look = look_at_job(&mut queue, &processes, &token).unwrap();
        assert!(look.is_none());
        let recorded = queue.recorded.lock().unwrap();
        let [(kind, payload)] = &recorded[..] else {
            panic!("{recorded:?}");
        };
        assert_eq!(kind, UPDATE_FAILED);
        assert_eq!(payload["stage"], "interrupted");
        assert_eq!(payload["after"], UPDATE_E2E_PASSED);
        assert_eq!(payload["commit"], "abc");
        assert_eq!(*queue.asks.lock().unwrap(), [AskKind::UpdateFailed]);
    }

    /// The automatic update looks on the first pass and at its interval
    /// after the last look, not 1 ms before.
    #[test]
    fn the_automatic_update_looks_again_at_its_interval() {
        let interval = Duration::from_secs(300);
        let ms = Duration::from_millis(1);
        assert!(check_due(None, interval));
        assert!(!check_due(Some(interval - ms), interval));
        assert!(check_due(Some(interval), interval));
    }
}
