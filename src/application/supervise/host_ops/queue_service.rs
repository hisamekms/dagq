//! The queue service under the supervisor ([Queue service], ADR-t1233-4
//! decisions 1 to 3): every [`QueueServicePort::interval`] a supervisor at
//! work looks whether the queue's service answers and runs its own build.
//! One that does not answer is started again, at most
//! [`QUEUE_SERVICE_RESTARTS`] times in [`QUEUE_SERVICE_RESTART_WINDOW`],
//! and one of another build (left by the binary before an `install` or an
//! automatic update handed the supervisor over) is replaced, uncounted. A
//! replacement that runs is taken as it is after, but a start succeeds
//! only for a service of this supervisor's own build (the control's
//! `start`): a supervisor whose executable an `install` replaced before it
//! execs fails the start (`queue_service_down`) and replaces the service
//! again at a later look. Each start is `queue_service_started` (`by:
//! supervisor`, `restart`, `replaced`).
//!
//! While the service is down the supervisor claims no new run and starts
//! no plan review, goal review, observer or throughput review: its own
//! operations on the queue go through the service (ADR-t3102-3 decision
//! 5). The runs in flight go on. A start that fails, the limit reached, or
//! a service found out of reach at the start, is an outage, recorded in
//! the queue's directory outside the DB ([`OUTAGE_FILE`]) once, and sent
//! once through `host.toml`'s `[push]` when it has one (decision 1). When
//! the service answers again the supervisor closes the record into the
//! queue's attention `queue_service_down` (with when it began and ended),
//! followed by `queue_service_started` or `queue_service_running`, so the
//! attention ends with it.
//!
//! A draining or handing-off supervisor does not look: an exec leaves the
//! service to the next process, which replaces it when its build changed.
//! At the end of a drain `down` asked for (`queue_service_stop_requested`
//! naming its token), the last supervisor of the queue stops the service
//! (`queue_service_stopped`, `by: supervisor`).
//!
//! [Queue service]: ../../../docs/design/queue-service.md

use super::inbox_nudge::NudgePush;
use super::*;
use crate::application::queue_service::{QueueServiceControl, ServiceProbe};
use crate::domain::queue_service::{
    OUTAGE_FILE, QUEUE_SERVICE_ATTENTION_KINDS, QUEUE_SERVICE_STOP_REQUESTED, ServiceOutage,
    ServiceState, UseCase,
};

/// How often a supervisor at work looks at the service.
pub const QUEUE_SERVICE_INTERVAL: Duration = Duration::from_secs(10);
/// Starts in [`QUEUE_SERVICE_RESTART_WINDOW`] before the supervisor stops
/// trying and leaves the service to a person.
pub const QUEUE_SERVICE_RESTARTS: usize = 3;
/// The window the starts are counted in.
pub const QUEUE_SERVICE_RESTART_WINDOW: Duration = Duration::from_secs(600);
/// How long a start waits for the service to answer.
pub const QUEUE_SERVICE_START_TIMEOUT: Duration = Duration::from_secs(10);

/// What the supervisor keeps the queue's service with.
#[derive(Clone)]
pub struct QueueServicePort {
    pub control: Arc<dyn QueueServiceControl>,
    pub interval: Duration,
    pub start_timeout: Duration,
}

/// The service as the supervisor knows it between passes.
#[derive(Default)]
pub(in crate::application::supervise) struct QueueServiceWatch {
    /// When it was last looked at.
    last: Option<Instant>,
    /// It answered, of this build, at the last look.
    up: bool,
    /// It answered at some look of this supervisor's.
    seen: bool,
    /// The starts in the window.
    starts: Vec<Instant>,
    /// The build of the service this supervisor last started, kept, not
    /// replaced, at the next looks. With a control whose start succeeds
    /// only for this supervisor's own build it is that build.
    accepted: Option<String>,
    /// The service this supervisor started before it registered
    /// ([`reach_at_start`]), recorded at its first look.
    started_at_start: Option<Value>,
    /// The `[push]` of an outage this look found, for the loop to send.
    push: Option<NudgePush>,
}

impl HostOpsState {
    /// Look at the service ([`Self::look_at_queue_service`]) and keep
    /// whether new claims and jobs may start in `service_up`.
    pub(in crate::application::supervise) fn queue_service_pass(
        &mut self,
        env: &mut HostEnv<'_>,
        working: bool,
        reports: Option<&ReportPort>,
    ) -> Option<NudgePush> {
        self.service_up = self.look_at_queue_service(env, working, reports);
        self.queue_service.push.take()
    }

    /// The service this process started before it registered, recorded at
    /// its first look ([`reach_at_start`]).
    pub(in crate::application::supervise) fn started_queue_service(&mut self, started: Value) {
        self.queue_service.started_at_start = Some(started);
    }

    /// The service did not answer one of the supervisor's operations: look
    /// at it at the next pass, whenever the last look was.
    pub(in crate::application::supervise) fn queue_service_unreached(&mut self) {
        self.queue_service.last = None;
        self.queue_service.up = false;
        self.service_up = false;
    }

    /// Look at the service when due, start it again or replace it as
    /// needed, and say whether new claims and jobs may start: always
    /// without a port, else whether the service runs. `working` is false
    /// while the supervisor drains or hands off: it only answers.
    fn look_at_queue_service(
        &mut self,
        env: &mut HostEnv<'_>,
        working: bool,
        reports: Option<&ReportPort>,
    ) -> bool {
        let Some(port) = self.queue_service_port.clone() else {
            return true;
        };
        if let Some(started) = self.queue_service.started_at_start.take() {
            self.close_outage(env);
            self.record_queue_service(env, EventKind::QueueServiceStarted, started);
            self.queue_service.up = true;
            self.queue_service.seen = true;
        }
        if !working {
            return self.queue_service.up;
        }
        let since_last = self
            .queue_service
            .last
            .map(|at| env.generators.clock.since(at));
        if !look_due(since_last, port.interval) {
            return self.queue_service.up;
        }
        self.queue_service.last = Some(env.generators.clock.monotonic());
        let found = port.control.probe();
        let accepted = found.state == ServiceState::Running
            && found.build.is_some()
            && found.build == self.queue_service.accepted;
        if found.current() || accepted {
            let closed = self.close_outage(env);
            if closed || (!self.queue_service.up && self.queue_service_attention_stands(env)) {
                info!("the queue service answers again");
                self.record_queue_service(
                    env,
                    EventKind::QueueServiceRunning,
                    json!({"pid": found.pid, "build": found.build}),
                );
            }
            self.queue_service.up = true;
            self.queue_service.seen = true;
            return true;
        }
        // A service of another build that answers is replaced, and the
        // replacement is not counted as a restart.
        let replacing = found.state == ServiceState::Running;
        if !replacing {
            self.queue_service.up = false;
        }
        let window = QUEUE_SERVICE_RESTART_WINDOW;
        let limited = restart_limited(
            &mut self.queue_service.starts,
            env.generators.clock.monotonic(),
            window,
        );
        if !replacing && limited {
            warn!(
                "the queue service was started {} times in {}s and is still not running: left to a person",
                self.queue_service.starts.len(),
                window.as_secs()
            );
            self.queue_service_down(
                env,
                reports,
                "restart_limit",
                &format!(
                    "started {} times in {}s; {}",
                    self.queue_service.starts.len(),
                    window.as_secs(),
                    describe(&found)
                ),
            );
            return false;
        }
        if !replacing {
            self.queue_service
                .starts
                .push(env.generators.clock.monotonic());
        }
        let replaced = (found.state != ServiceState::Stopped).then(|| found.clone());
        match port.control.start(port.start_timeout) {
            Ok(started) => {
                info!(
                    pid = started.pid,
                    "the queue service runs at {}",
                    started.socket.display()
                );
                self.close_outage(env);
                self.record_queue_service(
                    env,
                    EventKind::QueueServiceStarted,
                    json!({
                        "by": "supervisor",
                        "pid": started.pid,
                        "build": started.build,
                        "api_version": started.api_version,
                        "socket": started.socket,
                        "restart": self.queue_service.seen,
                        "replaced": replaced,
                    }),
                );
                self.queue_service.accepted = started.build.clone();
                self.queue_service.up = true;
                self.queue_service.seen = true;
                true
            }
            Err(error) => {
                self.queue_service.up = false;
                warn!(error = %format_args!("{error:#}"), "the queue service could not be started: {error:#}; no new run is claimed meanwhile");
                self.queue_service_down(env, reports, "start_failed", &format!("{error:#}"));
                false
            }
        }
    }

    /// At the end of a drain: stop the service when `down` asked this
    /// supervisor to and no other supervisor of the queue runs.
    pub(in crate::application::supervise) fn stop_queue_service_after_down(
        &mut self,
        env: &mut HostEnv<'_>,
    ) {
        let Some(port) = self.queue_service_port.clone() else {
            return;
        };
        if !self.last_supervisor_asked(env, QUEUE_SERVICE_STOP_REQUESTED) {
            return;
        }
        match port.control.stop(port.start_timeout) {
            Ok(Some(pid)) => {
                info!(
                    pid,
                    "the drain `down` asked for is over: the queue service is stopped"
                );
                self.record_queue_service(
                    env,
                    EventKind::QueueServiceStopped,
                    json!({"pid": pid, "by": "supervisor"}),
                );
            }
            Ok(None) => {}
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue service could not be stopped after the drain: {error:#}");
            }
        }
    }

    /// Whether `down` asked this supervisor, by the latest `requested`
    /// event (`queue_service_stop_requested` or
    /// `queue_broker_stop_requested`), to stop what it names at the end of
    /// its drain, and no other supervisor of the queue runs.
    pub(in crate::application::supervise) fn last_supervisor_asked(
        &mut self,
        env: &mut HostEnv<'_>,
        requested: &str,
    ) -> bool {
        let asked = match env.queue.latest_queue_event(&[requested]) {
            Ok(latest) => latest.is_some_and(|event| {
                event.payload["supervisors"]
                    .as_array()
                    .is_some_and(|tokens| {
                        tokens
                            .iter()
                            .any(|token| token.as_str() == Some(env.token.as_str()))
                    })
            }),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the stop request {requested} could not be read: {error:#}");
                false
            }
        };
        if !asked {
            return false;
        }
        let now = env.generators.clock.now();
        let others = match env.queue.supervisors() {
            Ok(registrations) => registrations
                .into_iter()
                .filter(|registration| {
                    registration.token != *env.token
                        && !heartbeat_stale(
                            env.processes.alive(registration.pid),
                            now - registration.heartbeat_at,
                        )
                })
                .count(),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue's supervisors could not be read: {error:#}");
                return false;
            }
        };
        if others > 0 {
            info!(
                "{others} other supervisor(s) of the queue still run: {requested} is left to the last one"
            );
            return false;
        }
        true
    }

    fn queue_service_attention_stands(&mut self, env: &mut HostEnv<'_>) -> bool {
        match env.queue.latest_queue_event(&QUEUE_SERVICE_ATTENTION_KINDS) {
            Ok(latest) => latest.is_some_and(|event| event.kind == EventKind::QueueServiceDown),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue service's events could not be read: {error:#}");
                false
            }
        }
    }

    /// An outage: its record and its `[push]` ([`note_outage`]), unless
    /// one stands already.
    fn queue_service_down(
        &mut self,
        env: &mut HostEnv<'_>,
        reports: Option<&ReportPort>,
        reason: &str,
        message: &str,
    ) {
        let now = env.generators.clock.now();
        if let Some(push) = note_outage(
            &**env.files,
            &env.layout.db,
            now,
            reason,
            message,
            Some(env.token),
            reports,
        ) {
            self.queue_service.push = Some(push);
        }
    }

    /// Close the outage's record, if one stands, into `queue_service_down`
    /// through the service, which answers again; whether there was one. A
    /// record the service does not take is left for the next look.
    fn close_outage(&mut self, env: &mut HostEnv<'_>) -> bool {
        let file = outage_file(&env.layout.db);
        let Some(outage) = read_outage(&**env.files, &file) else {
            return false;
        };
        let now = env.generators.clock.now();
        let mut payload = outage.down_payload(now);
        payload["supervisor"] = json!(outage.supervisor.as_deref().unwrap_or(env.token.as_str()));
        match env
            .queue
            .record_queue_event(EventKind::QueueServiceDown, payload)
        {
            Ok(_) => {
                if let Err(error) = env.files.remove_file(&file) {
                    warn!(error = %format_args!("{error:#}"), "the queue service's outage record could not be removed: {error:#}");
                }
                true
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue service's outage could not be recorded yet: {error:#}");
                false
            }
        }
    }

    fn record_queue_service(&mut self, env: &mut HostEnv<'_>, kind: EventKind, mut payload: Value) {
        payload["supervisor"] = json!(env.token);
        if let Err(error) = env.queue.record_queue_event(kind, payload) {
            warn!(error = %format_args!("{error:#}"), "the queue service's {kind} could not be recorded: {error:#}");
        }
    }
}

/// The record of an outage of the service of the queue at `db`, beside
/// it ([`OUTAGE_FILE`]).
pub(in crate::application::supervise) fn outage_file(db: &Path) -> PathBuf {
    db.parent().unwrap_or(Path::new(".")).join(OUTAGE_FILE)
}

/// The outage `file` records, if one stands and reads.
fn read_outage(files: &dyn RunFiles, file: &Path) -> Option<ServiceOutage> {
    let text = files.read_to_string(file).ok()?;
    serde_json::from_str(&text).ok()
}

/// Record an outage of the service of the queue at `db` found at `now`
/// (`reason`, `message`, by `supervisor`), unless one stands already, and
/// the `[push]` to send for it when `reports` has one: once per outage
/// (ADR-t3102-3 decision 1). A record that cannot be written is logged and
/// pushes nothing, so the next look tries again.
pub(in crate::application::supervise) fn note_outage(
    files: &dyn RunFiles,
    db: &Path,
    now: i64,
    reason: &str,
    message: &str,
    supervisor: Option<&LeaseToken>,
    reports: Option<&ReportPort>,
) -> Option<NudgePush> {
    let file = outage_file(db);
    if read_outage(files, &file).is_some() {
        return None;
    }
    let push = reports.and_then(|port| match (port.push_config)() {
        Ok(Some(config)) => Some((
            config,
            crate::domain::kpi::push::queue_service_down_message(
                &port.push_target.name,
                &port.push_target.queue,
                now,
                reason,
                message,
            ),
        )),
        Ok(None) => None,
        Err(error) => {
            warn!(error = %format_args!("{error:#}"), "[push] could not be read to tell of the queue service's outage: {error:#}");
            None
        }
    });
    let outage = ServiceOutage {
        since: now,
        reason: reason.to_owned(),
        message: message.to_owned(),
        supervisor: supervisor.map(|token| token.as_str().to_owned()),
        pushed: push.is_some(),
    };
    let written = serde_json::to_vec_pretty(&outage)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| Ok(files.write(&file, &bytes)?));
    if let Err(error) = written {
        warn!(error = %format_args!("{error:#}"), "the queue service's outage could not be recorded at {}: {error:#}", file.display());
        return None;
    }
    warn!(
        "the queue service is down ({reason}: {message}); recorded at {}",
        file.display()
    );
    push
}

/// Before the supervisor registers: whether its queue's service answers,
/// started when this supervisor keeps it and it does not, and replaced
/// when it is of another build. `None` once it
/// answers (with the start, if any, handed to host運用 to record at its
/// first look); else the outcome of a supervisor that starts nothing, the
/// outage recorded and its `[push]` sent now (no loop runs to send it).
pub(in crate::application::supervise) fn reach_at_start(
    ports: &Ports<'_>,
    started: &mut Option<Value>,
) -> Result<Option<Value>> {
    let hello = |service: &dyn crate::application::supervisor_service::ServiceCaller| {
        service.call(UseCase::Hello, Value::Null)
    };
    let error = match hello(&*ports.service) {
        // A service of this build, or one this supervisor keeps none of.
        Ok(answer)
            if ports.queue_service.is_none()
                || answer["build"].as_str() == Some(crate::VERSION) =>
        {
            return Ok(None);
        }
        // Another build's (the binary before an `install` or an automatic
        // update handed this process over) is replaced before anything is
        // sent to it: it may not answer this build's operations.
        Ok(answer) => anyhow::Error::new(
            crate::application::supervisor_service::ServiceUnreachable::not_done(format!(
                "the queue service runs build {}",
                answer["build"]
            )),
        ),
        Err(error) if !crate::application::supervisor_service::unreachable(&error) => {
            return Err(error);
        }
        Err(error) => error,
    };
    let (reason, message) = match &ports.queue_service {
        None => ("unreachable", format!("{error:#}")),
        Some(port) => {
            match crate::application::queue_service::ensure(&*port.control, port.start_timeout) {
                Ok(outcome) => match hello(&*ports.service) {
                    Ok(_) => {
                        if outcome["outcome"] != "reused" {
                            *started = Some(json!({
                                "by": "supervisor",
                                "pid": outcome["service"]["pid"],
                                "build": outcome["service"]["build"],
                                "api_version": outcome["service"]["api_version"],
                                "socket": outcome["service"]["socket"],
                                "restart": false,
                                "replaced": outcome["replaced"],
                            }));
                        }
                        return Ok(None);
                    }
                    Err(error) => ("start_failed", format!("{error:#}")),
                },
                Err(error) => ("start_failed", format!("{error:#}")),
            }
        }
    };
    warn!("the queue service does not answer ({reason}: {message}); the supervisor claims nothing");
    let now = ports.generators.clock.now();
    if let Some((config, message)) = note_outage(
        &*ports.files,
        &ports.layout.db,
        now,
        reason,
        &message,
        None,
        ports.reports.as_ref(),
    ) && let Some(port) = &ports.reports
    {
        let outcome = (port.run_push)(&crate::application::push::request(
            &config,
            &port.push_target,
            &message,
        ));
        if !outcome.success {
            warn!(
                "the queue service's outage could not be sent through [push]: {}",
                crate::application::push::failure_reason(&config, &outcome)
            );
        }
    }
    Ok(Some(json!({
        "outcome": "queue_service_unreachable",
        "reason": reason,
        "message": message,
        "outage": outage_file(&ports.layout.db),
    })))
}

/// What a look found, for the attention's message.
fn describe(found: &ServiceProbe) -> String {
    match &found.error {
        Some(error) => format!("{}: {error}", found.state.as_str()),
        None => found.state.as_str().to_owned(),
    }
}

/// Keep the `starts` still in `window` at `at`, and say whether they reach
/// [`QUEUE_SERVICE_RESTARTS`]: the service is then left to a person.
fn restart_limited(starts: &mut Vec<Instant>, at: Instant, window: Duration) -> bool {
    starts.retain(|start| at.saturating_duration_since(*start) < window);
    starts.len() >= QUEUE_SERVICE_RESTARTS
}

/// Whether the queue service is looked at now: on the first pass and
/// `interval` after the last look, given as the time since.
fn look_due(since_last: Option<Duration>, interval: Duration) -> bool {
    since_last.is_none_or(|since| since >= interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The queue service is looked at on the first pass and at its
    /// interval after the last look, not 1 ms before.
    #[test]
    fn the_queue_service_is_looked_at_again_at_its_interval() {
        let interval = Duration::from_secs(5);
        let ms = Duration::from_millis(1);
        assert!(look_due(None, interval));
        assert!(!look_due(Some(interval - ms), interval));
        assert!(look_due(Some(interval), interval));
    }

    /// The starts are counted in the window up to 1 ms before its end: at
    /// its end the oldest leaves it and the service is started again.
    #[test]
    fn the_restart_limit_counts_the_starts_in_the_window_only() {
        let window = QUEUE_SERVICE_RESTART_WINDOW;
        let base = Instant::now();
        let starts = || {
            (0..QUEUE_SERVICE_RESTARTS as u64)
                .map(|n| base + Duration::from_secs(n))
                .collect::<Vec<_>>()
        };
        let mut held = starts();
        assert!(restart_limited(
            &mut held,
            base + window - Duration::from_millis(1),
            window
        ));
        assert_eq!(held.len(), QUEUE_SERVICE_RESTARTS);
        let mut freed = starts();
        assert!(!restart_limited(&mut freed, base + window, window));
        assert_eq!(freed.len(), QUEUE_SERVICE_RESTARTS - 1);
        assert!(!restart_limited(&mut Vec::new(), base, window));
    }
}
