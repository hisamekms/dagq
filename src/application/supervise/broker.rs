//! The queue's resource broker under the supervisor (ADR-t827-3 decisions
//! 2 and 3, [Broker]): with a mode other than `disabled`, the supervisor
//! makes the broker ready on its first pass and before its claims
//! whenever it is not ([`BrokerControl::ensure`]: the machine, the image,
//! the container, the health), and looks at its health every
//! [`BrokerPort::health_interval`]. Every step runs on a job thread, one
//! at a time, so the image's long build never holds a claim; the loop
//! reaps it on a later pass. `preferred` claims whatever the broker's
//! state: a run goes on without it.
//!
//! Three failed looks in a row restart the container once
//! (`auto_repaired`, `repair: broker_restart`, ADR-0047's first layer).
//! A broker that still fails, or could not be made ready (`machine_busy`
//! when a person's machine runs, say), is the queue's attention
//! `broker_unhealthy` with its `reason`, recorded once per failure until
//! the broker runs again (`broker_started`, `broker_healthy`). A failed
//! start is tried again every [`BrokerPort::health_interval`].
//!
//! The supervisor stops the broker only at the end of a drain `down` asked
//! for (`broker_stop_requested` naming its token), and only when no other
//! supervisor of the queue runs: the container, then dagq's machine when
//! no container runs on it ([`BrokerControl::stop`], `broker_stopped` with
//! `by: supervisor`). `up`'s replacement of a supervisor does not ask, and
//! an exec (the handoff of `install` and the automatic update) does not
//! drain, so the broker keeps running through both for the next process. A handoff waits for the job running (an exec would orphan its podman
//! command); a job still running when the loop ends otherwise is left to
//! finish on its own.
//!
//! A `disabled` supervisor has no port and calls no podman, but a queue
//! run `preferred` before may have left tokens: each pass (and the one
//! before it stops) revokes every active mark's run, live or ended, with
//! the reason `mode_disabled`, and a run it starts or resumes loses any
//! MCP configuration left in its dir, so no worker is handed the tools of
//! a broker that no longer serves it (task 1125). The sweep finds a run by
//! its token file too, and a headless run loses what is left before each
//! turn it requests (task 1141).
//!
//! [Broker]: ../../../docs/design/broker.md

use super::*;
use crate::application::broker::{BrokerControl, BrokerFailure, StartReport};
use crate::application::broker_run::{Grant, RENEW_BEFORE_SECS, RunTokens};
use crate::domain::broker::{
    BROKER_ATTENTION_KINDS, BROKER_RESTART, BROKER_STOP_REQUESTED, BROKER_UNHEALTHY, BrokerMode,
};
use crate::domain::broker_usage::{ToolUsage, records_tool_use};

/// How often the health is looked at, and a failed start tried again.
pub const BROKER_HEALTH_INTERVAL: Duration = Duration::from_secs(30);
/// Failed looks in a row before the container is restarted.
pub const BROKER_FAILURES: u32 = 3;
/// The reason of `broker_token_revoked` for a token a `disabled`
/// supervisor found left by an earlier mode.
const MODE_DISABLED: &str = "mode_disabled";

/// What the supervisor drives the queue's broker with.
#[derive(Clone)]
pub struct BrokerPort {
    /// The mode in force at the supervisor's start; a port is given only
    /// for a mode other than `disabled`.
    pub mode: BrokerMode,
    pub control: Arc<dyn BrokerControl>,
    pub health_interval: Duration,
    /// Issues and revokes the runs' tokens.
    pub tokens: Arc<dyn RunTokens>,
    /// The client a worker runs (`dagq-broker-client` next to dagq, of
    /// dagq's build), or why there is none: its worker gets no tools
    /// (ADR-t827-1 decision 7).
    pub client: std::result::Result<PathBuf, BrokerFailure>,
}

/// What a broker job did.
enum JobOutcome {
    Ensured(std::result::Result<Box<StartReport>, BrokerFailure>),
    Looked(std::result::Result<(), String>),
    Restarted(std::result::Result<(), BrokerFailure>),
}

impl BrokerWatch {
    /// A start, a look or a restart runs: a handoff waits for it.
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }
}

/// The broker as the supervisor knows it between passes.
#[derive(Default)]
pub(super) struct BrokerWatch {
    job: Option<thread::JoinHandle<JobOutcome>>,
    /// The broker answered its health after the last start.
    ready: bool,
    /// When the last start or look began.
    last: Option<Instant>,
    /// Failed looks in a row.
    failures: u32,
    /// The container was restarted in this run of failures.
    restarted: bool,
    /// The port of the last start.
    port: Option<u16>,
    /// Whether the last start's health named dagq's build.
    build_matches: bool,
}

impl Supervisor<'_> {
    /// Reap the broker's job once it ended; then start the next one when it
    /// is due: a start while the broker is not ready (on the first pass and
    /// before the claims), a look at its health every interval. `working`
    /// is false while the supervisor drains or hands off: it only reaps.
    pub(super) fn broker_pass(&mut self, working: bool) {
        let Some(port) = self.broker_port.clone() else {
            return;
        };
        if let Some(job) = self.broker.job.take() {
            if !job.is_finished() {
                self.broker.job = Some(job);
                return;
            }
            match job.join() {
                Ok(outcome) => self.broker_outcome(&port, outcome),
                Err(_) => warn!("the broker's job panicked"),
            }
        }
        if !working {
            return;
        }
        let restart =
            self.broker.ready && self.broker.failures >= BROKER_FAILURES && !self.broker.restarted;
        let due = self
            .broker
            .last
            .is_none_or(|last| last.elapsed() >= port.health_interval);
        if !restart && !due {
            return;
        }
        self.broker.last = Some(Instant::now());
        let control = port.control.clone();
        self.broker.job = Some(if restart {
            self.broker.restarted = true;
            warn!(
                "the broker's health failed {} times in a row: restarting its container",
                self.broker.failures
            );
            spawn_traced(move || JobOutcome::Restarted(control.restart()))
        } else if self.broker.ready {
            spawn_traced(move || JobOutcome::Looked(control.health()))
        } else {
            info!(mode = port.mode.as_str(), "making the queue's broker ready");
            spawn_traced(move || JobOutcome::Ensured(control.ensure().map(Box::new)))
        });
    }

    fn broker_outcome(&mut self, port: &BrokerPort, outcome: JobOutcome) {
        match outcome {
            JobOutcome::Ensured(Ok(report)) => {
                self.broker.ready = true;
                self.broker.failures = 0;
                self.broker.restarted = false;
                self.broker.port = Some(report.port);
                self.broker.build_matches = report.build_matches;
                info!(
                    port = report.port,
                    "the queue's broker runs on 127.0.0.1:{}", report.port
                );
                if let Some(duration_ms) = report.build_ms {
                    self.record_broker(
                        EventKind::BrokerImageBuilt,
                        json!({"build": self.layout.version, "image": report.image, "duration_ms": duration_ms}),
                    );
                }
                self.record_broker(
                    EventKind::BrokerStarted,
                    json!({
                        "mode": port.mode,
                        "port": report.port,
                        "build": self.layout.version,
                        "image": report.image,
                        "container": report.container,
                        "container_outcome": report.container_outcome,
                        "machine": report.machine,
                        "images": report.images,
                    }),
                );
            }
            JobOutcome::Ensured(Err(failure)) => {
                self.broker.ready = false;
                warn!(
                    code = failure.code.as_str(),
                    "the queue's broker could not be made ready: {failure}; runs go on without it"
                );
                self.broker_unhealthy(failure.code.as_str(), &failure.message);
            }
            JobOutcome::Looked(Ok(())) => {
                self.broker.failures = 0;
                self.broker.restarted = false;
                if self.broker_attention_stands() {
                    info!("the queue's broker answers its health again");
                    self.record_broker(EventKind::BrokerHealthy, json!({}));
                }
            }
            JobOutcome::Looked(Err(error)) => {
                self.broker.failures += 1;
                warn!(
                    failures = self.broker.failures,
                    "the queue's broker did not answer its health: {error}"
                );
                if self.broker.restarted && self.broker.failures >= BROKER_FAILURES {
                    self.broker_unhealthy("unhealthy", &error);
                }
            }
            JobOutcome::Restarted(Ok(())) => {
                let failures = self.broker.failures;
                self.broker.failures = 0;
                info!("the queue's broker answers its health after its restart");
                self.record_broker(
                    EventKind::AutoRepaired,
                    json!({
                        "repair": BROKER_RESTART,
                        "layer": "runtime",
                        "conditions": {"failures": failures},
                        "detail": {"container": crate::application::broker::container_name(&self.layout.queue_hash)},
                        "supervisor": self.token,
                    }),
                );
                if self.broker_attention_stands() {
                    self.record_broker(EventKind::BrokerHealthy, json!({}));
                }
            }
            JobOutcome::Restarted(Err(failure)) => {
                warn!(
                    code = failure.code.as_str(),
                    "the queue's broker still fails after its restart: {failure}"
                );
                // Made ready again from the machine up, every interval.
                self.broker.ready = false;
                self.broker_unhealthy(failure.code.as_str(), &failure.message);
            }
        }
    }

    /// At the end of a drain: stop the broker when `down` asked this
    /// supervisor to (the latest `broker_stop_requested` names its token)
    /// and no other supervisor of the queue runs. A job still running is
    /// waited for first, since it holds the queue's broker lock.
    pub(super) fn stop_broker_after_down(&mut self) {
        let Some(port) = self.broker_port.clone() else {
            return;
        };
        let asked = match self.queue.latest_queue_event(&[BROKER_STOP_REQUESTED]) {
            Ok(latest) => latest.is_some_and(|event| {
                event.payload["supervisors"]
                    .as_array()
                    .is_some_and(|tokens| {
                        tokens
                            .iter()
                            .any(|token| token.as_str() == Some(self.token.as_str()))
                    })
            }),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the broker's stop request could not be read: {error:#}");
                false
            }
        };
        if !asked {
            return;
        }
        let now = self.generators.clock.now();
        let others = match self.queue.supervisors() {
            Ok(registrations) => registrations
                .into_iter()
                .filter(|registration| {
                    registration.token != self.token
                        && !heartbeat_stale(
                            self.processes.alive(registration.pid),
                            now - registration.heartbeat_at,
                        )
                })
                .count(),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue's supervisors could not be read: {error:#}");
                return;
            }
        };
        if others > 0 {
            info!(
                "{others} other supervisor(s) of the queue still run: the broker is left to the last one"
            );
            return;
        }
        if let Some(job) = self.broker.job.take()
            && job.join().is_err()
        {
            warn!("the broker's job panicked");
        }
        match port.control.stop() {
            Ok(report) => {
                info!(
                    container_stopped = report.container_stopped,
                    machine_stopped = report.machine_stopped,
                    "the drain `down` asked for is over: the queue's broker is stopped"
                );
                if let Some(cleanup) = report
                    .gvproxy
                    .as_ref()
                    .filter(|cleanup| !cleanup.failures.is_empty())
                {
                    warn!("{}", cleanup.summary());
                }
                self.record_broker(
                    EventKind::BrokerStopped,
                    json!({
                        "container": crate::application::broker::container_name(&self.layout.queue_hash),
                        "container_stopped": report.container_stopped,
                        "machine_stopped": report.machine_stopped,
                        "gvproxy": report.gvproxy,
                        "by": "supervisor",
                    }),
                );
            }
            Err(failure) => {
                warn!(
                    code = failure.code.as_str(),
                    "the queue's broker could not be stopped after the drain: {failure}"
                );
            }
        }
    }

    /// Why `run`'s worker cannot be given the broker's tools now, as the
    /// `reason` and `message` of `broker_unavailable`; else the grant.
    fn broker_offer(
        &self,
        port: &BrokerPort,
        run: &TaskRun,
    ) -> std::result::Result<Grant, (String, String)> {
        if run.actual_provider() != Provider::Claude {
            return Err((
                "provider".to_owned(),
                format!(
                    "a {} worker is given no broker tools yet",
                    run.actual_provider().as_str()
                ),
            ));
        }
        let port_number = match self.broker.port {
            Some(number) if self.broker.ready && self.broker.failures == 0 => number,
            Some(_) if self.broker.ready => {
                return Err((
                    "unhealthy".to_owned(),
                    format!(
                        "the queue's broker did not answer its last {} look(s) at its health",
                        self.broker.failures
                    ),
                ));
            }
            // Not ready as far as this supervisor knows: a broker recorded
            // as running dagq's build that answers now is used.
            _ => match port.control.running_port() {
                Ok(number) => {
                    return match &port.client {
                        Ok(client) => Ok(Grant {
                            client: client.clone(),
                            port: number,
                        }),
                        Err(failure) => {
                            Err((failure.code.as_str().to_owned(), failure.message.clone()))
                        }
                    };
                }
                Err(why) => return Err(("not_ready".to_owned(), why)),
            },
        };
        if !self.broker.build_matches {
            return Err((
                "version_mismatch".to_owned(),
                "the broker's health names another build than dagq's".to_owned(),
            ));
        }
        match &port.client {
            Ok(client) => Ok(Grant {
                client: client.clone(),
                port: port_number,
            }),
            Err(failure) => Err((failure.code.as_str().to_owned(), failure.message.clone())),
        }
    }

    /// Give `run`'s worker (or its resume) the broker's tools with the
    /// mode `preferred` (ADR-t827-4 decisions 1 and 3): revoke any token
    /// the run held, issue one, put its file and the run's MCP
    /// configuration in place, and record `broker_token_issued`. A broker
    /// that cannot be used is `broker_unavailable` with its `reason`, and
    /// the worker starts without the tools; nothing stops the run. With
    /// `disabled` only what an earlier mode left of the run goes
    /// (`mode_disabled`). Whether the tools were given.
    pub(super) fn broker_grant(&mut self, run: &TaskRun) -> bool {
        let Some(port) = self.broker_port.clone() else {
            if let Some(tokens) = self.broker_leftovers.clone() {
                self.broker_revoke(&*tokens, run, MODE_DISABLED);
            }
            return false;
        };
        self.broker_revoke(&*port.tokens, run, "reissued");
        let grant = match self.broker_offer(&port, run) {
            Ok(grant) => grant,
            Err((reason, message)) => {
                info!(run_id = %run.id(), reason, "run {} starts without the broker's tools: {message}", run.id());
                self.record_run_broker(
                    run.id(),
                    EventKind::BrokerUnavailable,
                    json!({"reason": reason, "message": message}),
                );
                return false;
            }
        };
        let now = u64::try_from(self.generators.clock.now()).unwrap_or(0);
        match port.tokens.issue(run, &grant, now) {
            Ok(issued) => {
                info!(run_id = %run.id(), jti = issued.jti, "run {}'s worker gets the broker's tools on 127.0.0.1:{}", run.id(), grant.port);
                self.record_run_broker(run.id(), EventKind::BrokerTokenIssued, issued.payload());
                true
            }
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}'s broker token could not be issued: {error:#}", run.id());
                // Whatever part of it was put in place goes.
                if let Err(error) = port.tokens.revoke(run.id(), run.run_dir().map(Path::new)) {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}'s broker token could not be cleaned up: {error:#}", run.id());
                }
                self.record_run_broker(
                    run.id(),
                    EventKind::BrokerUnavailable,
                    json!({"reason": "token_failed", "message": format!("{error:#}")}),
                );
                false
            }
        }
    }

    /// Before a headless turn of `run` is requested: with `disabled`, what
    /// an earlier mode left of the run goes (`mode_disabled`), so the turn
    /// (an answer, a revise, a resume) gets no `--mcp-config` of a broker
    /// that no longer serves it, marked or not (task 1141). With another
    /// mode the run keeps the tools its grant gave.
    pub(super) fn broker_before_turn(&mut self, run: &TaskRun) {
        if self.broker_port.is_none()
            && let Some(tokens) = self.broker_leftovers.clone()
        {
            self.broker_revoke(&*tokens, run, MODE_DISABLED);
        }
    }

    /// Revoke every token of `run` (`reason` in `broker_token_revoked`).
    /// Whether one was; `None` when the revoke failed.
    fn broker_revoke(
        &mut self,
        tokens: &dyn RunTokens,
        run: &TaskRun,
        reason: &str,
    ) -> Option<bool> {
        match tokens.revoke(run.id(), run.run_dir().map(Path::new)) {
            Ok(revoked) => {
                let any = !revoked.is_empty();
                for jti in revoked {
                    info!(run_id = %run.id(), jti, reason, "run {}'s broker token is revoked ({reason})", run.id());
                    self.record_run_broker(
                        run.id(),
                        EventKind::BrokerTokenRevoked,
                        json!({"jti": jti, "reason": reason}),
                    );
                }
                Some(any)
            }
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}'s broker token could not be revoked: {error:#}", run.id());
                None
            }
        }
    }

    /// Record the ended `run`'s `broker_tool_use`: its calls through the
    /// broker and around it ([`crate::domain::broker_usage`]). Counts that
    /// cannot be read are a warning, never a hold on the sweep.
    fn broker_record_usage(&mut self, run: &TaskRun, usage: Result<ToolUsage>) {
        match usage {
            Ok(usage) => {
                info!(run_id = %run.id(), brokered = usage.brokered, direct = usage.direct, "run {} called the broker {} times and the built-in tools {} times", run.id(), usage.brokered, usage.direct);
                self.record_run_broker(run.id(), EventKind::BrokerToolUse, usage.payload());
            }
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}'s use of the broker could not be counted: {error:#}", run.id());
            }
        }
    }

    /// Every pass: revoke the tokens of the runs that ended (the reason is
    /// the run's status: `integrated`, `failed`, `interrupted`, ...), and
    /// of runs the queue does not know, and issue again the token of a
    /// live run with less than [`RENEW_BEFORE_SECS`] left. A run that
    /// ended while no supervisor ran is revoked by the next one's first
    /// pass. With `disabled`, every token held goes instead
    /// ([`Self::broker_sweep_disabled`]).
    pub(super) fn broker_sweep(&mut self) {
        let Some(port) = self.broker_port.clone() else {
            if let Some(tokens) = self.broker_leftovers.clone() {
                self.broker_sweep_disabled(&*tokens);
            }
            return;
        };
        let held = match port.tokens.held() {
            Ok(held) => held,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the broker's active tokens could not be read: {error:#}");
                return;
            }
        };
        let now = u64::try_from(self.generators.clock.now()).unwrap_or(0);
        for token in held {
            let run = match self.broker_sweep_run(&token.run_id) {
                Ok(run) => run,
                Err(_) => continue,
            };
            let Some(run) = run else {
                warn!(
                    jti = token.jti,
                    run = token.run_id,
                    "a broker token names no run of the queue: retired"
                );
                // Its token file goes too: nothing holds it.
                let removed = match RunId::new(&token.run_id) {
                    Ok(id) => port.tokens.revoke(&id, None).map(|_| ()),
                    Err(_) => port.tokens.retire(&token.jti),
                };
                if let Err(error) = removed {
                    warn!(error = %format_args!("{error:#}"), "a broker token could not be retired: {error:#}");
                }
                continue;
            };
            if broker_run_ended(run.status()) {
                // Counted before the revoke, recorded once with it
                // (`records_tool_use`): the marks left are read only
                // when the revoke failed.
                let usage = port.tokens.usage(&run);
                let revoked = self.broker_revoke(&*port.tokens, &run, run.status().as_str());
                let marks_left = match revoked {
                    Some(_) => None,
                    None => port
                        .tokens
                        .held()
                        .ok()
                        .map(|held| held.iter().any(|token| token.run_id == run.id().as_str())),
                };
                if records_tool_use(revoked, marks_left) {
                    self.broker_record_usage(&run, usage);
                }
                continue;
            }
            // A mark the run's token file does not hold (an older token
            // left by a renewal whose retire failed, or a file that is
            // gone) is retired, never renewed: one token per run.
            let Some(exp) = token.exp else {
                match port.tokens.retire(&token.jti) {
                    Ok(()) => self.record_run_broker(
                        run.id(),
                        EventKind::BrokerTokenRevoked,
                        json!({"jti": token.jti, "reason": "stale"}),
                    ),
                    Err(error) => {
                        warn!(error = %format_args!("{error:#}"), "a stale broker token could not be retired: {error:#}");
                    }
                }
                continue;
            };
            if exp.saturating_sub(now) >= RENEW_BEFORE_SECS {
                continue;
            }
            let Ok(grant) = self.broker_offer(&port, &run) else {
                continue;
            };
            match port.tokens.issue(&run, &grant, now) {
                Ok(issued) => {
                    let mut payload = issued.payload();
                    payload["renews"] = json!(token.jti);
                    self.record_run_broker(run.id(), EventKind::BrokerTokenIssued, payload);
                    match port.tokens.retire(&token.jti) {
                        Ok(()) => self.record_run_broker(
                            run.id(),
                            EventKind::BrokerTokenRevoked,
                            json!({"jti": token.jti, "reason": "renewed"}),
                        ),
                        Err(error) => {
                            warn!(error = %format_args!("{error:#}"), "the older broker token could not be retired: {error:#}");
                        }
                    }
                }
                Err(error) => {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}'s broker token could not be issued again: {error:#}", run.id());
                }
            }
        }
    }

    /// With `disabled` (task 1125): revoke the tokens an earlier mode left,
    /// the live runs' too, since no broker serves them now: each run's
    /// marks, token file and `<run dir>/broker` go, one
    /// `broker_token_revoked` (`mode_disabled`) per mark. Only files: no
    /// podman. A run is found by its marks and by its token file, marked
    /// or not (task 1141); one the queue does not know loses its marks and
    /// token file.
    fn broker_sweep_disabled(&mut self, tokens: &dyn RunTokens) {
        let held = match tokens.held() {
            Ok(held) => held,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the broker's tokens left by an earlier mode could not be read: {error:#}");
                return;
            }
        };
        let mut runs: Vec<String> = held.into_iter().map(|token| token.run_id).collect();
        // A token file no mark names (a revoke that failed partway, a
        // retire of a run the queue does not know) goes too (task 1141).
        match tokens.token_files() {
            Ok(files) => runs.extend(files),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the broker's token files left by an earlier mode could not be read: {error:#}");
            }
        }
        runs.sort();
        runs.dedup();
        for run_id in runs {
            let id = RunId::new(&run_id).ok();
            match self.broker_sweep_run(&run_id) {
                Err(_) => continue,
                Ok(Some(run)) => {
                    // `disabled` counts no tool use (goal 59's (2)).
                    self.broker_revoke(tokens, &run, MODE_DISABLED);
                }
                Ok(None) => {
                    warn!(
                        run = run_id,
                        "a broker token names no run of the queue: retired"
                    );
                    let removed = match &id {
                        Some(id) => tokens.revoke(id, None).map(|_| ()),
                        // A mark whose run id is not one: only its marks.
                        None => tokens.held().and_then(|held| {
                            held.iter()
                                .filter(|token| token.run_id == run_id)
                                .try_for_each(|token| tokens.retire(&token.jti))
                        }),
                    };
                    if let Err(error) = removed {
                        warn!(error = %format_args!("{error:#}"), "a broker token could not be retired: {error:#}");
                    }
                }
            }
        }
    }

    /// A failed read must leave all credentials in place for the next pass.
    fn broker_sweep_run(&self, run_id: &str) -> Result<Option<TaskRun>> {
        let Ok(id) = RunId::new(run_id) else {
            return Ok(None);
        };
        match self.queue.run(&id) {
            Ok(run) => Ok(Some(run)),
            Err(error) if error.is::<crate::application::RunNotFound>() => Ok(None),
            Err(error) => {
                warn!(run_id, error = %format_args!("{error:#}"), "a broker token's run could not be read; skipped until the next pass: {error:#}");
                Err(error)
            }
        }
    }

    fn record_run_broker(&mut self, run: &RunId, kind: EventKind, payload: Value) {
        if let Err(error) = self.queue.record_runtime_event(run, kind, payload) {
            warn!(run_id = %run, error = %format_args!("{error:#}"), "the broker's {kind} of run {run} could not be recorded: {error:#}");
        }
    }

    /// Whether the latest broker event is `broker_unhealthy`.
    fn broker_attention_stands(&self) -> bool {
        match self.queue.latest_queue_event(&BROKER_ATTENTION_KINDS) {
            Ok(latest) => latest.is_some_and(|event| event.kind == BROKER_UNHEALTHY),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the broker's events could not be read: {error:#}");
                false
            }
        }
    }

    /// The attention `broker_unhealthy`, unless it already stands: one
    /// failure tells the inbox once, whatever its reasons on the way.
    fn broker_unhealthy(&mut self, reason: &str, message: &str) {
        let latest = match self.queue.latest_queue_event(&BROKER_ATTENTION_KINDS) {
            Ok(latest) => latest,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the broker's events could not be read: {error:#}");
                return;
            }
        };
        if latest.is_some_and(|event| event.kind == BROKER_UNHEALTHY) {
            return;
        }
        self.record_broker(
            EventKind::BrokerUnhealthy,
            json!({
                "reason": reason,
                "message": message,
                "failures": self.broker.failures,
                "restarted": self.broker.restarted,
            }),
        );
    }

    fn record_broker(&mut self, kind: EventKind, mut payload: Value) {
        payload["supervisor"] = json!(self.token);
        if let Err(error) = self.queue.record_queue_event(kind, payload) {
            warn!(error = %format_args!("{error:#}"), "the broker's {kind} could not be recorded: {error:#}");
        }
    }
}

/// A run whose token is revoked: it ended.
fn broker_run_ended(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Integrated | RunStatus::Succeeded | RunStatus::Failed | RunStatus::Interrupted
    )
}
