//! 観測と分析 of the supervisor (rule C3 of docs/design/architecture.md):
//! its state ([`ObservationState`]) and the submodules that change it. The
//! state's fields are private to this module, so no other context's
//! submodule can change them; the loop calls the state's methods.

use super::*;
use crate::application::ObservationQueue;

pub(super) mod agent_watch;
pub(super) mod ci_watch;
pub(super) mod forecast;
pub(super) mod ledger;
pub(super) mod live_alerts;
pub(super) mod main_log;
pub(super) mod observer;
pub(super) mod push;
pub(super) mod report;
pub(super) mod resources;
pub(super) mod throughput_review;

/// 観測と分析's state: the jobs on its timer (the observer, the throughput
/// review), the KPI reports and their push, the forecast snapshots, the
/// candidates' sample and the CI watch. The claim, the resume and the
/// landing read the CI watch's hold ([`CiWatchState::held`],
/// [`CiWatchState::unreadable`]).
pub(super) struct ObservationState {
    /// The observer job running now: one at a time, outside the run slots.
    observer: Option<observer::ObserverJob>,
    /// Opens the connections of the context's jobs off the loop.
    queues: Arc<dyn QueueOpener<dyn ObservationQueue + Send>>,
    /// When this process last launched each observation, so one that dies
    /// before it records anything is not relaunched on every pass.
    observers_launched: Vec<(ObserveMode, Instant)>,
    /// The observation due again now whatever the queue says: one that
    /// found its provider unusable, which starts again on the other
    /// provider (ADR-t1063-1 decision 4, task 1223) or, with
    /// `[provider_fallback] jobs` off, on the same one once its hold ends
    /// (ADR-t1857-1).
    observer_again: Option<ObserveMode>,
    /// The throughput review running now (ADR-t996-1), and the periods this
    /// process started.
    throughput_review: throughput_review::ThroughputReviewWatch,
    /// The `candidates_sampled` this process recorded last (ADR-0051
    /// decision 3); `None` until its first claim pass records one.
    candidates: Option<CandidatesSample>,
    /// Writes the daily KPI reports; `None` writes none.
    reports: Option<ReportPort>,
    /// The report job and the day the reports were last found written.
    report: report::ReportWatch,
    /// Takes the forecast snapshots; `None` takes none.
    forecasts: Option<ForecastPort>,
    /// The snapshot job and where the last look for triggers left off.
    forecast: forecast::ForecastWatch,
    /// The KPI push's messages waiting and the one being sent.
    push: push::PushWatch,
    /// How often the watch of the adopted agent definitions looks
    /// (ADR-t1728-1 (g)); `None` watches none.
    agent_watch_check: Option<Duration>,
    agent_watch: agent_watch::AgentWatch,
    /// Watches the landing branch's CI (ADR-t1920-1).
    ci_watch_port: Option<CiWatchPort>,
    ci: ci_watch::CiWatchState,
    /// The record of `stats`' judgments of now.
    live_alerts: live_alerts::LiveAlertWatch,
    /// The baseline of the attributes the measurement folds.
    attributes_baseline: live_alerts::AttributesBaseline,
    /// The record of main's history.
    main_log: main_log::MainLogWatch,
    /// Builds the ledger; `None` builds none.
    ledger_port: Option<ledger::LedgerPort>,
    ledger: ledger::LedgerWatch,
    /// Measures the runs' resources from outside; `None` measures none.
    resources_port: Option<resources::ResourcesPort>,
    resources: resources::ResourcesWatch,
}

impl ObservationState {
    pub(super) fn new(ports: &Ports<'_>) -> Self {
        Self {
            observer: None,
            queues: ports.observation_queues.clone(),
            observers_launched: Vec::new(),
            observer_again: None,
            throughput_review: throughput_review::ThroughputReviewWatch::default(),
            candidates: None,
            reports: ports.reports.clone(),
            report: report::ReportWatch::default(),
            forecasts: ports.forecasts.clone(),
            forecast: forecast::ForecastWatch::default(),
            push: push::PushWatch::default(),
            agent_watch_check: ports.agent_watch,
            agent_watch: agent_watch::AgentWatch::default(),
            ci_watch_port: ports.ci_watch.clone(),
            ci: ci_watch::CiWatchState::default(),
            live_alerts: live_alerts::LiveAlertWatch::default(),
            attributes_baseline: live_alerts::AttributesBaseline::default(),
            main_log: main_log::MainLogWatch::default(),
            ledger_port: ports.ledger.clone(),
            ledger: ledger::LedgerWatch::default(),
            resources_port: ports.resources.clone(),
            resources: resources::ResourcesWatch::default(),
        }
    }

    /// Measure the resources of the runs of the slots, `held`, at its
    /// interval ([`resources::ResourcesWatch::pass`]).
    pub(super) fn resources_pass(&mut self, env: &mut ObservationEnv<'_>, held: Vec<RunId>) {
        self.resources.pass(env, self.resources_port.as_ref(), held);
    }

    /// Join or leave the measure of the runs' resources at the end of the
    /// loop ([`resources::ResourcesWatch::finish`]).
    pub(super) fn resources_finish(&mut self, env: &mut ObservationEnv<'_>, deadline: &EndBy) {
        self.resources.finish(env, deadline);
    }

    /// Build the ledger at its interval ([`ledger::LedgerWatch::pass`]).
    pub(super) fn ledger_pass(&mut self, env: &mut ObservationEnv<'_>) {
        self.ledger.pass(env, self.ledger_port.as_ref());
    }

    /// Join or leave the ledger's build at the end of the loop
    /// ([`ledger::LedgerWatch::finish`]).
    pub(super) fn ledger_finish(&mut self, env: &mut ObservationEnv<'_>, deadline: &EndBy) {
        self.ledger.finish(env, deadline);
    }

    /// Record `stats`' judgments of now ([`live_alerts::LiveAlertWatch::pass`]).
    pub(super) fn live_alerts_pass(
        &mut self,
        env: &mut ObservationEnv<'_>,
        signals: &dyn AgentSignals,
        interval: Duration,
    ) {
        self.live_alerts.pass(env, signals, interval);
    }

    /// Compare the attributes the measurement folds with the state
    /// ([`live_alerts::AttributesBaseline::pass`]).
    pub(super) fn attributes_baseline_pass(&mut self, env: &mut ObservationEnv<'_>) {
        self.attributes_baseline.pass(env);
    }

    /// Writes the daily KPI reports; `None` writes none.
    pub(super) fn reports(&self) -> Option<ReportPort> {
        self.reports.clone()
    }

    /// The CI watch, whose hold the claim, the resume and the landing read.
    pub(super) const fn ci(&self) -> &ci_watch::CiWatchState {
        &self.ci
    }

    /// A job of the context runs that a handoff waits for: the KPI report,
    /// the forecast, the KPI push being sent and the CI check.
    pub(super) const fn handoff_waits(&self) -> bool {
        self.push.running() || self.report.running() || self.forecast.running() || self.ci.running()
    }

    /// A job of the context runs that a loop with no slot waits for, like a
    /// run: the observer, the throughput review, the report, the forecast,
    /// the CI check and the message being sent, and with `drain_pushes`
    /// the messages still to be tried too.
    pub(super) fn busy(&self, drain_pushes: bool) -> bool {
        self.observer.is_some()
            || self.throughput_review.running()
            || self.report.running()
            || self.forecast.running()
            // A CI check holds the claims until it answers.
            || self.ci.running()
            // A message being sent is bounded by the command's timeout.
            || self.push.running()
            || (drain_pushes && self.push.busy())
    }

    /// Publish the hold transition and candidate sample together when both
    /// change. A failed write is retried from the next pass's decision.
    pub(super) fn record_candidates(
        &mut self,
        env: &mut ObservationEnv<'_>,
        sample: Result<CandidatesSample>,
        hold: Option<(EventKind, Value)>,
    ) {
        let result = sample.and_then(|sample| {
            let hold_log = hold.as_ref().map(|(kind, payload)| {
                (
                    *kind,
                    payload["message"].as_str().unwrap_or_default().to_owned(),
                )
            });
            let sampled = sample
                .transition(self.candidates.as_ref())
                .map(|mut payload| {
                    payload["supervisor"] = json!(env.token);
                    (EventKind::CandidatesSampled, payload)
                });
            let records: Vec<_> = hold.into_iter().chain(sampled).collect();
            if records.is_empty() {
                return Ok(());
            }
            // Recompute on the next pass if the service is unavailable.
            // Replaying an old sample could supersede a newer decision.
            env.queue.record_queue_events(records)?;
            match hold_log {
                Some((EventKind::ClaimHeld, message)) => warn!("{message}"),
                Some(_) => info!("claims resume: the queue-wide hold ended"),
                None if sample.end.reason != self.candidates.and_then(|last| last.end.reason) => {
                    if let Some(reason) = sample.end.reason {
                        tracing::debug!(admission_reason = reason.name(), "new claims wait");
                    }
                }
                None => {}
            }
            self.candidates = Some(sample);
            Ok(())
        });
        if let Err(error) = result {
            warn!(error = %format_args!("{error:#}"), "the claim decision could not be recorded: {error:#}");
        }
    }
}
