//! The supervisor's nudge of an inbox without a watcher (ADR-t906-1
//! decision 1 (3)): while no `watch --role inbox` watches and an ask has
//! waited for the inbox past [`NUDGE_AFTER_SECS`], the supervisor types one
//! line into the inbox's workspace, and only while its screen looks idle
//! (ADR-t803-1) with nothing typed in its input box. Once more after
//! [`NUDGE_AGAIN_SECS`] without a watcher, then a `cmux notify` to the
//! person, then nothing until a watcher is seen again. Each nudge is
//! claimed as `inbox_nudged` before it is made, keyed by the absence (the
//! watcher's last sighting) and the attempt, so no second supervisor and no
//! process after an exec makes it again.
//!
//! Every pass also records the watcher's state when it changed (task
//! 1021): `inbox_watcher_absent` when it went from alive to absent,
//! `inbox_watcher_returned` the other way, the first state judged on a
//! queue without either. The KPI of how long an ask waits to be seen
//! (`ask_seen_wait`) reads them, as the watchers' records do not last.

use super::*;
use crate::application::{
    inbox_watcher::{self, InboxWatcher, WatcherState},
    naming::repository_name,
    screen_idle::{self, MarkerState, ScreenIdle, ScreenProbe},
};
use crate::domain::event_kind::{EventKind as Kind, INBOX_NUDGED};

/// Seconds an ask waits for the inbox without a watcher before the first
/// nudge: from the later of its opening and the watcher's last sighting.
pub(super) const NUDGE_AFTER_SECS: i64 = 300;

/// Seconds after a nudge before the next one of the same absence.
pub(super) const NUDGE_AGAIN_SECS: i64 = 600;

/// The nudges typed into the inbox in one absence; the one after them is
/// the `cmux notify`, the last.
pub(super) const TYPED_NUDGES: i64 = 2;

/// The newest `inbox_nudged` events read for the current absence.
const NUDGES_READ: usize = 20;

/// Where the inbox's screen spans and the supervisor's input stamp are
/// kept, under the queue's directory: the inbox has no idle marker, so the
/// marker's path only names them.
const INBOX_DIR: &str = "inbox";

/// The next nudge of an absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Nudge {
    /// Type the line into the inbox; `attempt` from 1.
    Type { attempt: i64 },
    /// Tell the person by `cmux notify`: the last.
    Notify { attempt: i64 },
}

impl Nudge {
    fn attempt(self) -> i64 {
        match self {
            Nudge::Type { attempt } | Nudge::Notify { attempt } => attempt,
        }
    }

    fn action(self) -> &'static str {
        match self {
            Nudge::Type { .. } => "typed",
            Nudge::Notify { .. } => "notified",
        }
    }
}

/// The absence the nudges of `watcher` are keyed by: its last sighting, 0
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

/// The nudge due at `now` for `watcher`, with asks open for the inbox
/// since `opened` and the nudges of this absence made so far (`attempt`,
/// `at`): none while a watcher is alive, no ask waited long enough, the
/// last nudge was less than [`NUDGE_AGAIN_SECS`] ago, or the notify was
/// sent. A typed nudge not made within twice the interval is skipped for
/// the notify.
pub(super) fn next_nudge(
    watcher: &InboxWatcher,
    opened: &[i64],
    made: &[(i64, i64)],
    now: i64,
) -> Option<Nudge> {
    if watcher.state == WatcherState::Alive || waiting(opened, absence(watcher), now) == 0 {
        return None;
    }
    let Some(&(attempt, at)) = made.iter().max_by_key(|(attempt, _)| *attempt) else {
        return Some(Nudge::Type { attempt: 1 });
    };
    if attempt > TYPED_NUDGES || now - at < NUDGE_AGAIN_SECS {
        return None;
    }
    // A typed nudge that could not be made over two intervals (the screen
    // never idle with an empty box, or the last line stuck in it) gives way
    // to the notify, so the person is still told.
    if now - at >= 2 * NUDGE_AGAIN_SECS {
        return Some(Nudge::Notify {
            attempt: TYPED_NUDGES + 1,
        });
    }
    let attempt = attempt + 1;
    Some(if attempt > TYPED_NUDGES {
        Nudge::Notify { attempt }
    } else {
        Nudge::Type { attempt }
    })
}

/// The line typed into the inbox, before the language's instruction.
pub(super) fn nudge_text(asks: usize) -> String {
    format!(
        "dagq: {asks} open ask(s) wait for the inbox and no `dagq watch --role inbox` is running. Run `dagq status --role inbox`, then start the watch in the background as the dagq-inbox skill says."
    )
}

impl Supervisor<'_> {
    /// Nudge the inbox when it is due. An error before a nudge is claimed is
    /// logged and looked at again on a later pass; a nudge claimed and not
    /// delivered is recorded as failed and counts as made. Neither holds up
    /// anything else.
    /// Records a change of the inbox's watcher, then nudges the inbox
    /// when `nudge` and it is due.
    pub(super) fn inbox_nudge_pass(&mut self, nudge: bool) {
        let now = self.generators.clock.now();
        let watcher = inbox_watcher::judge_with(
            &inbox_watcher::read(&*self.files, &inbox_watcher::dir(&self.layout.db)),
            &*self.processes,
            now,
        );
        if let Err(error) = self.record_watcher_change(&watcher, now) {
            warn!(error = %format_args!("{error:#}"), "the change of the inbox's watcher could not be recorded: {error:#}");
        }
        if !nudge || watcher.state == WatcherState::Alive {
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
        let absent_since = absence(watcher);
        let made: Vec<(i64, i64)> = self
            .queue
            .latest_events_of(INBOX_NUDGED, NUDGES_READ)?
            .iter()
            .filter(|event| event.payload["absent_since"].as_i64() == Some(absent_since))
            .filter_map(|event| {
                Some((
                    event.payload["attempt"].as_i64()?,
                    event.payload["at"].as_i64()?,
                ))
            })
            .collect();
        let Some(nudge) = next_nudge(watcher, &opened, &made, now) else {
            return Ok(());
        };
        // A workspace not recorded or closed gets nothing.
        let Some(workspace) = self.queue.session_workspace(SessionRole::Inbox)? else {
            return Ok(());
        };
        if !self.cmux.exists(&workspace)? {
            return Ok(());
        }
        let marker = self.inbox_marker();
        if matches!(nudge, Nudge::Type { .. }) && !self.inbox_idle(&workspace, &marker, now)? {
            return Ok(());
        }
        let waited = waiting(&opened, absent_since, now);
        let payload = json!({
            "absent_since": absent_since,
            "attempt": nudge.attempt(),
            "action": nudge.action(),
            "at": now,
            "workspace_id": workspace,
            "open_asks": opened.len(),
            "waiting_asks": waited,
            "absent_secs": watcher.absent_secs,
        });
        if !self.queue.claim_inbox_nudge(payload)? {
            return Ok(());
        }
        let delivered = match nudge {
            Nudge::Type { .. } => {
                if let Err(error) = screen_idle::record_supervisor_input(&*self.files, &marker) {
                    warn!(error = %error, "the stamp of the line typed into the inbox could not be written: {error}");
                }
                let text = with_instruction(
                    nudge_text(waited),
                    self.verifier.language().as_ref(),
                );
                match submit_input(self.cmux, self.signals, &workspace, Input::Text(&text)) {
                    Ok((Submission::Submitted(_), _)) => Ok(()),
                    Ok((submission, _)) => Err(anyhow!(
                        "the line did not leave the input box: {}",
                        match submission {
                            Submission::Dialog(_) => "a dialog came up",
                            Submission::Stuck(_) => "it stayed in the box",
                            _ => "it was not sent",
                        }
                    )),
                    Err(error) => Err(error),
                }
            }
            Nudge::Notify { .. } => self.cmux.notify(
                &format!(
                    "[{}] inbox has no watch",
                    repository_name(&self.layout.main_checkout)
                ),
                &format!(
                    "{waited} open ask(s) wait; the inbox did not start `dagq watch --role inbox` after it was nudged."
                ),
                Some(&workspace),
            ),
        };
        match delivered {
            Ok(()) => info!(
                "the inbox without a watcher was nudged ({}, attempt {}) in workspace {workspace}",
                nudge.action(),
                nudge.attempt()
            ),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the inbox's nudge ({}, attempt {}) failed in workspace {workspace}: {error:#}", nudge.action(), nudge.attempt());
                self.queue.record_queue_event(
                    EventKind::InboxNudgeFailed,
                    json!({
                        "absent_since": absent_since,
                        "attempt": nudge.attempt(),
                        "action": nudge.action(),
                        "workspace_id": workspace,
                        "error": format!("{error:#}"),
                    }),
                )?;
            }
        }
        Ok(())
    }

    /// The path that names the inbox's screen spans and the supervisor's
    /// input stamp; its directory is made as it can be.
    fn inbox_marker(&self) -> PathBuf {
        let dir = self
            .layout
            .db
            .parent()
            .map_or_else(|| PathBuf::from(INBOX_DIR), |parent| parent.join(INBOX_DIR));
        if let Err(error) = self.files.create_dir_all(&dir) {
            warn!(error = %error, "{} could not be made: {error}", dir.display());
        }
        dir.join("idle.json")
    }

    /// Whether the inbox's screen looks idle over `[stall].screen_idle_secs`
    /// (ADR-t803-1) and its input box holds nothing typed: a person typing
    /// or the agent at work gets no line.
    fn inbox_idle(&self, workspace: &str, marker: &Path, now: i64) -> Result<bool> {
        let probe = ScreenProbe {
            cmux: self.cmux,
            signals: self.signals,
            files: &*self.files,
            mode: ScreenIdle::Record(&self.screen_spans),
            threshold: self.stall.screen_idle(),
        };
        let last_input = unix_millis(screen_idle::last_input(
            &*self.files,
            marker,
            SystemTime::UNIX_EPOCH,
        ));
        if probe
            .infer(
                workspace,
                marker,
                MarkerState::Missing,
                now.saturating_mul(1000),
                last_input,
            )
            .is_none()
        {
            return Ok(false);
        }
        let Ok(screen) = self.cmux.capture(workspace) else {
            return Ok(false);
        };
        Ok(self.signals.input_ready(&screen) && self.signals.input_empty(&screen))
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
        assert_eq!(next_nudge(&alive, &[0], &[], 10_000), None);
        let watcher = absent(Some(1_000));
        assert_eq!(next_nudge(&watcher, &[], &[], 10_000), None);
        // An ask opened long ago counts from the watcher's last sighting.
        assert_eq!(
            next_nudge(&watcher, &[0], &[], 1_000 + NUDGE_AFTER_SECS - 1),
            None
        );
        assert_eq!(
            next_nudge(&watcher, &[0], &[], 1_000 + NUDGE_AFTER_SECS),
            Some(Nudge::Type { attempt: 1 })
        );
        // One opened later counts from its opening.
        assert_eq!(
            next_nudge(&watcher, &[1_200], &[], 1_300 + NUDGE_AFTER_SECS - 101),
            None
        );
        // A watcher never seen: from the ask's opening.
        assert_eq!(
            next_nudge(&absent(None), &[50], &[], 50 + NUDGE_AFTER_SECS),
            Some(Nudge::Type { attempt: 1 })
        );
        assert_eq!(absence(&absent(None)), 0);
    }

    #[test]
    fn nudges_twice_then_notifies_once_then_stops() {
        let watcher = absent(Some(0));
        let now = 10_000;
        assert_eq!(
            next_nudge(&watcher, &[0], &[(1, now - NUDGE_AGAIN_SECS + 1)], now),
            None
        );
        assert_eq!(
            next_nudge(&watcher, &[0], &[(1, now - NUDGE_AGAIN_SECS)], now),
            Some(Nudge::Type { attempt: 2 })
        );
        assert_eq!(
            next_nudge(&watcher, &[0], &[(1, 0), (2, now - NUDGE_AGAIN_SECS)], now),
            Some(Nudge::Notify { attempt: 3 })
        );
        assert_eq!(
            next_nudge(&watcher, &[0], &[(3, 0), (1, 0), (2, 0)], now),
            None
        );
        // A second line not typed within two intervals gives way to the
        // notify, which ends the absence's nudges.
        assert_eq!(
            next_nudge(&watcher, &[0], &[(1, now - 2 * NUDGE_AGAIN_SECS)], now),
            Some(Nudge::Notify { attempt: 3 })
        );
        assert_eq!(
            next_nudge(&watcher, &[0], &[(1, 0), (3, now - 1)], now),
            None
        );
        assert_eq!(Nudge::Notify { attempt: 3 }.action(), "notified");
        assert_eq!(Nudge::Type { attempt: 1 }.action(), "typed");
        assert!(nudge_text(3).contains("3 open ask(s)"));
    }
}
