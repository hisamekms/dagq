//! The runtime's backstop for an inbox without a watcher (ADR-t1433-5
//! decision 1 (3)): while no `watch --role inbox` watches and an ask has
//! waited for the inbox past [`NUDGE_AFTER_SECS`], the supervisor records
//! `inbox_nudged` once for the absence and, when `host.toml` has a
//! `[push]`, sends one message through it saying so and how many asks
//! wait (nothing of their content). It reads no screen, types into no
//! terminal and calls no cmux (ADR-t1433-1). The record is claimed before
//! the push is queued, keyed by the absence (the watcher's last sighting),
//! so no second supervisor and no process after an exec makes it again.
//!
//! Every pass also records the watcher's state when it changed (task
//! 1021): `inbox_watcher_absent` when it went from alive to absent,
//! `inbox_watcher_returned` the other way, the first state judged on a
//! queue without either. The KPI of how long an ask waits to be seen
//! (`ask_seen_wait`) reads them, as the watchers' records do not last.

use super::*;
use crate::application::inbox_watcher::{self, InboxWatcher, WatcherState};
use crate::domain::event_kind::EventKind as Kind;
use crate::domain::kpi::push::inbox_watch_message;

/// Seconds an ask waits for the inbox without a watcher before the
/// backstop: from the later of its opening and the watcher's last sighting.
pub(super) const NUDGE_AFTER_SECS: i64 = 300;

/// The one attempt of an absence: older binaries numbered their typed
/// lines and their notify from 1, so a nudge one of them made in the same
/// absence is not made again.
const ATTEMPT: i64 = 1;

/// `action` of an `inbox_nudged` that queued the `[push]` message.
pub(super) const PUSHED: &str = "pushed";

/// `action` of an `inbox_nudged` without a `[push]`: the event alone.
pub(super) const RECORDED: &str = "recorded";

/// The absence the backstop of `watcher` is keyed by: its last sighting, 0
/// for a watcher never seen.
pub(super) fn absence(watcher: &InboxWatcher) -> i64 {
    watcher.last_seen_at.unwrap_or(0)
}

/// The asks of `opened` (their `created_at`) that waited without a watcher
/// absent since `absent_since` past [`NUDGE_AFTER_SECS`] at `now`.
fn waiting(opened: &[i64], absent_since: i64, now: i64) -> usize {
    opened
        .iter()
        .filter(|&&created| now - created.max(absent_since) >= NUDGE_AFTER_SECS)
        .count()
}

/// Whether the backstop is due at `now` for `watcher`, with asks open for
/// the inbox since `opened`: not while a watcher is alive nor before an
/// ask waited long enough. Whether it was made already in this absence is
/// the claim's to say.
pub(super) fn nudge_due(watcher: &InboxWatcher, opened: &[i64], now: i64) -> bool {
    watcher.state != WatcherState::Alive && waiting(opened, absence(watcher), now) > 0
}

impl Supervisor<'_> {
    /// Records a change of the inbox's watcher, then makes the backstop
    /// when it is due. An error is logged and looked at again on a later
    /// pass; it holds up nothing else.
    pub(super) fn inbox_nudge_pass(&mut self) {
        let now = self.generators.clock.now();
        let watcher = inbox_watcher::judge_with(
            &inbox_watcher::read(&*self.files, &inbox_watcher::dir(&self.layout.db)),
            &*self.processes,
            now,
        );
        if let Err(error) = self.record_watcher_change(&watcher, now) {
            warn!(error = %format_args!("{error:#}"), "the change of the inbox's watcher could not be recorded: {error:#}");
        }
        if watcher.state == WatcherState::Alive {
            return;
        }
        if let Err(error) = self.nudge_inbox(&watcher, now) {
            warn!(error = %format_args!("{error:#}"), "the inbox without a watcher could not be nudged: {error:#}");
        }
    }

    /// Record the watcher's state unless the latest record holds it: the
    /// queue's write transaction compares, so a second supervisor and the
    /// process after an exec do not record the same change again.
    fn record_watcher_change(&self, watcher: &InboxWatcher, now: i64) -> Result<()> {
        let kind = match watcher.state {
            WatcherState::Alive => Kind::InboxWatcherReturned,
            WatcherState::Absent => Kind::InboxWatcherAbsent,
        };
        let payload = json!({
            "at": now,
            "watching": watcher.watching,
            "last_seen_at": watcher.last_seen_at,
            "absent_secs": watcher.absent_secs,
        });
        if self.queue.record_inbox_watcher_change(kind, payload)? {
            info!("the inbox's watcher is now {}", kind.as_str());
        }
        Ok(())
    }

    /// Claim the backstop of this absence and make it. The claimed
    /// `inbox_nudged` carries `absent_since` and `attempt` (the absence it
    /// is keyed by), `action` ([`PUSHED`] or [`RECORDED`]), `at`,
    /// `open_asks` and `waiting_asks` (counts only), `absent_secs`, and
    /// `push_error` when `[push]` could not be read.
    fn nudge_inbox(&mut self, watcher: &InboxWatcher, now: i64) -> Result<()> {
        let asks = self.queue.asks(AskQuery {
            all: false,
            open: true,
            role: Some(SessionRole::Inbox),
        })?;
        let opened: Vec<i64> = asks
            .iter()
            .filter(|ask| ask.is_open())
            .map(|ask| ask.created_at)
            .collect();
        if !nudge_due(watcher, &opened, now) {
            return Ok(());
        }
        let absent_since = absence(watcher);
        let waited = waiting(&opened, absent_since, now);
        // A `[push]` that cannot be read pushes nothing; the event says so.
        let (push, push_error) = match self
            .observation
            .reports
            .as_ref()
            .map(|port| (port.push_config)())
        {
            None | Some(Ok(None)) => (None, None),
            Some(Ok(Some(config))) => (Some(config), None),
            Some(Err(error)) => (None, Some(format!("{error:#}"))),
        };
        let action = if push.is_some() { PUSHED } else { RECORDED };
        let payload = json!({
            "absent_since": absent_since,
            "attempt": ATTEMPT,
            "action": action,
            "at": now,
            "open_asks": opened.len(),
            "waiting_asks": waited,
            "absent_secs": watcher.absent_secs,
            "push_error": push_error,
        });
        if !self.queue.claim_inbox_nudge(payload)? {
            return Ok(());
        }
        match (push, self.observation.reports.as_ref()) {
            (Some(config), Some(port)) => {
                let message = inbox_watch_message(
                    &port.push_target.name,
                    &port.push_target.queue,
                    absent_since,
                    opened.len(),
                    waited,
                );
                self.observation.queue_pushes(config, vec![message]);
                info!(
                    "no watch runs in the inbox while {waited} ask(s) wait: recorded, and sent through [push]"
                );
            }
            _ => info!(
                "no watch runs in the inbox while {waited} ask(s) wait: recorded (no [push] in host.toml)"
            ),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absent(last_seen_at: Option<i64>) -> InboxWatcher {
        InboxWatcher {
            state: WatcherState::Absent,
            watching: 0,
            last_seen_at,
            absent_secs: None,
            grace_secs: inbox_watcher::END_GRACE_SECS,
        }
    }

    #[test]
    fn no_nudge_while_a_watcher_is_alive_or_no_ask_waited_long_enough() {
        let alive = InboxWatcher {
            state: WatcherState::Alive,
            ..absent(Some(1_000))
        };
        assert!(!nudge_due(&alive, &[0], 10_000));
        let watcher = absent(Some(1_000));
        assert!(!nudge_due(&watcher, &[], 10_000));
        // An ask opened long ago counts from the watcher's last sighting.
        assert!(!nudge_due(&watcher, &[0], 1_000 + NUDGE_AFTER_SECS - 1));
        assert!(nudge_due(&watcher, &[0], 1_000 + NUDGE_AFTER_SECS));
        // One opened later counts from its opening.
        assert!(!nudge_due(&watcher, &[1_200], 1_200 + NUDGE_AFTER_SECS - 1));
        assert!(nudge_due(&watcher, &[1_200], 1_200 + NUDGE_AFTER_SECS));
        // A watcher never seen: from the ask's opening.
        assert!(nudge_due(&absent(None), &[50], 50 + NUDGE_AFTER_SECS));
        assert_eq!(absence(&absent(None)), 0);
        assert_eq!(
            waiting(&[0, 1_200, 1_250], 1_000, 1_200 + NUDGE_AFTER_SECS),
            2
        );
    }
}
