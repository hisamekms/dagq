//! `parallel`, `max_waiting`, `runtime_planners` and `claim_spacing` read
//! again from `[supervisor]` of the main checkout's `dagq.toml` each pass
//! (task 698, task 941, ADR-t1479-1), for the values the flags did not
//! give.

use crate::domain::EventKind;
use anyhow::Result;
use tracing::{info, warn};

use super::stages::ClaimState;
use crate::application::ports::{RunLog, SupervisorRegistry};
use crate::domain::LeaseToken;
use crate::domain::light_slots::LightChanges;
use crate::domain::slot_limits::{SlotLimits, supervisor_config_change};

fn names(light: &LightChanges) -> Vec<&str> {
    light
        .values()
        .iter()
        .map(|change| change.as_str())
        .collect()
}

impl ClaimState {
    /// Read `[supervisor]` again, recording a change on `queue` and in
    /// `token`'s registration. Values that differ from those in use
    /// replace them from this pass on, are written to the registration and
    /// recorded as `supervisor_config_changed`. A file that cannot be read
    /// or holds invalid values keeps those in use, warned of once per
    /// error; so does a missing file, which may only be a checkout
    /// rewriting it. Runs over a lowered `parallel` or waits over a lowered
    /// `max_waiting` go on; no new run is claimed or waits until they are
    /// under it. Planners over a lowered `runtime_planners` go on too; no
    /// new one is opened until they are under it. A changed
    /// `claim_spacing` spaces the next claim from the queue's latest one.
    /// `light_changes` (ADR-t1591-1) has no flag: it is read with every
    /// flag given too, and a change takes effect at the next claim.
    pub(super) fn reread_limits(
        &mut self,
        queue: &(impl SupervisorRegistry + RunLog + ?Sized),
        token: &LeaseToken,
    ) -> Result<()> {
        let Some(read) = self.supervisor_file.clone() else {
            return Ok(());
        };
        let config = match read() {
            Ok(Some(config)) => {
                self.supervisor_error = None;
                config
            }
            Ok(None) => {
                self.supervisor_error = None;
                return Ok(());
            }
            Err(error) => {
                let message = format!("{error:#}");
                if self.supervisor_error.as_ref() != Some(&message) {
                    warn!(error = %message, "[supervisor] of dagq.toml not read: {message}; keeping parallel {}, max_waiting {}, runtime_planners {} and claim_spacing {}", self.parallel, self.max_waiting, self.limits.runtime_planners.value, self.limits.claim_spacing.value);
                    self.supervisor_error = Some(message);
                }
                return Ok(());
            }
        };
        let to = SlotLimits::resolve(self.slot_flags, &config);
        let light = config.light_changes();
        let Some(mut payload) =
            supervisor_config_change((self.limits, &self.light_changes), (to, &light))
        else {
            return Ok(());
        };
        if light != self.light_changes {
            info!(
                "[supervisor] light_changes of dagq.toml changed: {:?} -> {:?}",
                names(&self.light_changes),
                names(&light)
            );
            self.light_changes = light;
        }
        if to == self.limits {
            payload["supervisor"] = serde_json::json!(token);
            queue.record_queue_event(EventKind::SupervisorConfigChanged, payload)?;
            return Ok(());
        }
        info!(
            "[supervisor] of dagq.toml changed: parallel {} -> {}, max_waiting {} -> {}, runtime_planners {} -> {}, claim_spacing {} -> {}",
            self.limits.parallel.value,
            to.parallel.value,
            self.limits.max_waiting.value,
            to.max_waiting.value,
            self.limits.runtime_planners.value,
            to.runtime_planners.value,
            self.limits.claim_spacing.value,
            to.claim_spacing.value
        );
        queue.set_slot_limits(token, to)?;
        self.limits = to;
        self.parallel = to.parallel.value;
        self.max_waiting = to.max_waiting.value;
        payload["supervisor"] = serde_json::json!(token);
        queue.record_queue_event(EventKind::SupervisorConfigChanged, payload)?;
        Ok(())
    }
}
