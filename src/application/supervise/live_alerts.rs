//! 観測と分析's record of `stats`' judgments of now (docs/design/
//! measurement.md "今の判定の記録"): on a pass at most every
//! `LoopSettings::live_alert_interval`, the supervisor reads the whole
//! queue's inputs as `stats` does, judges them with the same functions and
//! writes what its stream records ([`LiveRecorder`]). The runtime's control
//! never reads these records.

use super::*;
use crate::application::stats::{LiveSources, live_inputs};
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
        env: &mut PassEnv<'_>,
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

/// The whole queue's inputs at `now` and the keys of the alerts judged on
/// them, or why they could not be read. The thresholds are the
/// supervisors' (`stall_config_loaded`), which every supervisor records as
/// it starts.
fn observe(env: &PassEnv<'_>, signals: &dyn AgentSignals, now: i64) -> Observation {
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
