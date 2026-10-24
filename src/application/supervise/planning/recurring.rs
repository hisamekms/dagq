//! The recurring drafts (ADR-0072 decision 2): each pass that tends the
//! runtime's planners reads `[[recurring]]` of the main checkout's
//! `dagq.toml` again and registers the draft of each key whose interval
//! came, which the draft planners then take like any draft the runtime
//! registered. A file that cannot be read registers none that pass and is
//! warned of once per error; it is never a question to a person.

use super::*;
use crate::domain::recurring::RecurringOutcome;

impl PlanningState {
    /// Register the draft of each `[[recurring]]` key that is due.
    pub(in crate::application::supervise) fn register_recurring_drafts(
        &mut self,
        env: &mut PlanningEnv<'_>,
    ) {
        let Some(read) = self.recurring_file.clone() else {
            return;
        };
        let items = match read() {
            Ok(items) => {
                self.recurring_error = None;
                items
            }
            Err(error) => {
                let text = format!("{error:#}");
                if self.recurring_error.as_deref() != Some(text.as_str()) {
                    warn!(error = %text, "[[recurring]] of dagq.toml not read: {text}; no recurring draft is registered until it is");
                    self.recurring_error = Some(text);
                }
                return;
            }
        };
        for item in &items {
            match env.queue.register_recurring_draft(item) {
                Ok(RecurringOutcome::Registered { task_id }) => {
                    info!(task_id = %task_id, "recurring {}: its interval of {} days came; registered draft task {task_id} for a planner of the runtime's", item.key, item.interval_days);
                }
                Ok(RecurringOutcome::Open { .. } | RecurringOutcome::NotDue { .. }) => {}
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "recurring {}: its draft could not be registered: {error:#}", item.key);
                }
            }
        }
    }
}
