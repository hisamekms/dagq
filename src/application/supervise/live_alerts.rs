//! 観測と分析's record of `stats`' judgments of now (docs/design/
//! measurement.md "今の判定の記録"): on a pass at most every
//! `LoopSettings::live_alert_interval`, the supervisor reads the whole
//! queue's inputs as `stats` does, judges them with the same functions and
//! writes what its stream records ([`LiveRecorder`]). The runtime's control
//! never reads these records.

use super::*;
use crate::application::stats::{LiveSources, live_inputs};
use crate::domain::ledger::attributes::reconcile_due;
use crate::domain::live_alerts::{LiveRecorder, Observation, judge};

/// The stream this process records and when it last observed.
#[derive(Debug, Default)]
pub(super) struct LiveAlertWatch {
    recorder: LiveRecorder,
    observed: Option<Instant>,
}

impl LiveAlertWatch {
    /// Observe and record when `interval` passed since the last
    /// observation (the first pass observes at once). A record that cannot
    /// be written is warned of, and the next observation records a
    /// baseline again.
    pub(super) fn pass(
        &mut self,
        env: &mut ObservationEnv<'_>,
        signals: &dyn AgentSignals,
        interval: Duration,
    ) {
        let at = env.generators.clock.monotonic();
        if self
            .observed
            .is_some_and(|observed| at.saturating_duration_since(observed) < interval)
        {
            return;
        }
        self.observed = Some(at);
        let now = env.generators.clock.now();
        let observation = observe(env, signals, now);
        let records = self
            .recorder
            .observe(env.token.as_str(), now * 1000, observation);
        for (kind, payload) in records {
            if let Err(error) = env.queue.record_queue_event(kind, payload) {
                warn!(error = %format_args!("{error:#}"), "the record of the judgments of now could not be written: {error:#}");
                self.recorder.reset();
                return;
            }
        }
    }
}

/// When this process last compared the records of the attributes the
/// measurement folds with the state (docs/design/attribute-records.md).
#[derive(Debug, Default)]
pub(super) struct AttributesBaseline {
    /// The monotonic time of the process's first comparison, from which
    /// the times of the comparisons are counted.
    origin: Option<Instant>,
    /// When the last comparison ran, counted from `origin`.
    checked: Option<Duration>,
}

impl AttributesBaseline {
    /// On the first pass of the process and then every
    /// [`crate::domain::ledger::attributes::RECONCILE_INTERVAL_SECS`], write
    /// the baseline of the tasks, goals and findings whose records do not
    /// give what the state holds (made or changed by a binary before the
    /// record). One that cannot be written is warned of and tried again at
    /// the next interval; the findings' are tried when the tasks' and
    /// goals' could not be.
    pub(super) fn pass(&mut self, env: &mut ObservationEnv<'_>) {
        let now = env.generators.clock.monotonic();
        let at = now.saturating_duration_since(*self.origin.get_or_insert(now));
        let queue = &*env.queue;
        let Some(findings) = self.step(at, || {
            match queue.record_attributes_baseline() {
                Ok(Some((tasks, goals))) => {
                    info!(tasks, goals, "recorded the baseline of the attributes");
                }
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the baseline of the attributes could not be written: {error:#}");
                }
                Ok(None) => {}
            }
            queue.record_findings_baseline()
        }) else {
            return;
        };
        match findings {
            Ok(Some(findings)) => {
                info!(findings, "recorded the baseline of the findings");
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the baseline of the findings could not be written: {error:#}");
            }
            Ok(None) => {}
        }
    }

    /// Run `record` when a comparison is due at `at` (counted from the
    /// first comparison; [`reconcile_due`]), noting `at` as the last
    /// comparison whatever its outcome; none when it is not due.
    fn step<T>(&mut self, at: Duration, record: impl FnOnce() -> Result<T>) -> Option<Result<T>> {
        if !reconcile_due(self.checked.map(|checked| at.saturating_sub(checked))) {
            return None;
        }
        self.checked = Some(at);
        Some(record())
    }
}

/// The whole queue's inputs at `now` and the keys of the alerts judged on
/// them, or why they could not be read. The thresholds are the
/// supervisors' (`stall_config_loaded`), which every supervisor records as
/// it starts.
fn observe(env: &ObservationEnv<'_>, signals: &dyn AgentSignals, now: i64) -> Observation {
    let read = || -> Result<Observation> {
        let events = env.queue.all_events()?;
        let no_file = || Ok(None);
        let sources = LiveSources {
            files: &**env.files,
            signals,
            config_file: &no_file,
        };
        let inputs = live_inputs(&*env.queue, &**env.processes, now, &events, &sources)?;
        let alerts = judge(&events, now, &inputs).keys();
        Ok(Observation::Seen {
            inputs: Box::new(inputs),
            alerts,
        })
    };
    read().unwrap_or_else(|error| Observation::Failed {
        reason: format!("{error:#}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ledger::attributes::RECONCILE_INTERVAL_SECS;

    /// The comparison, given its times as values counted from the first,
    /// runs on the first pass, waits out the interval after it, runs again
    /// at the interval, and after a failure waits the interval again before
    /// it tries once more.
    #[test]
    fn the_comparison_runs_first_then_every_interval_and_retries_a_failure_after_one() {
        let interval = Duration::from_secs(RECONCILE_INTERVAL_SECS);
        let mut baseline = AttributesBaseline::default();
        let mut calls = 0;
        let ok = |calls: &mut i32| {
            *calls += 1;
            Ok::<_, anyhow::Error>(Some((1, 0)))
        };
        assert!(matches!(
            baseline.step(Duration::ZERO, || ok(&mut calls)),
            Some(Ok(Some((1, 0))))
        ));
        assert!(
            baseline
                .step(interval - Duration::from_millis(1), || ok(&mut calls))
                .is_none()
        );
        assert_eq!(calls, 1);
        let failed = baseline.step(interval, || {
            calls += 1;
            Err::<Option<(usize, usize)>, _>(anyhow::anyhow!("locked"))
        });
        assert!(matches!(failed, Some(Err(_))));
        assert_eq!(calls, 2);
        // The failed one is not tried again at once, only after the interval.
        assert!(
            baseline
                .step(interval + Duration::from_millis(1), || ok(&mut calls))
                .is_none()
        );
        assert!(
            baseline
                .step(interval * 2 - Duration::from_millis(1), || ok(&mut calls))
                .is_none()
        );
        assert!(baseline.step(interval * 2, || ok(&mut calls)).is_some());
        assert_eq!(calls, 3);
    }
}
