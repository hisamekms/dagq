//! The thresholds of the stalled-session checks (ADR-0043 decision 4): the
//! `[stall]` table of the repository's `dagq.toml`, each a positive number
//! of seconds, with the defaults a person chose (2026-09-25). A test may
//! set a threshold below a second ([`StallConfig::with_millis`]); the
//! checks compare each as a [`Duration`] ([`StallConfig::threshold`]).
use std::{collections::HashMap, time::Duration};

use serde::Serialize;
use serde_json::Value;

use super::RunEvent;

/// Receipt-less idle before the nudge, and again before the `stalled` ask.
pub const DEFAULT_IDLE_WITHOUT_RECEIPT_SECS: i64 = 20 * 60;
/// How long a sent text may wait to be taken up, and again after the Enter.
pub const DEFAULT_SEND_CONFIRM_SECS: i64 = 60;
/// Background work running this long is a `long_background` alert.
pub const DEFAULT_BACKGROUND_ALERT_SECS: i64 = 30 * 60;
/// A process of the run that makes no progress this long is an
/// `idle_process` alert (task 469).
pub const DEFAULT_IDLE_PROCESS_SECS: i64 = 30 * 60;
/// How long a session without a fresh idle marker must look idle on its
/// screen before it is taken for idle (ADR-t803-1).
pub const DEFAULT_SCREEN_IDLE_SECS: i64 = 2 * 60;
/// A headless worker's turn with no line of output this long is stopped
/// (ADR-t813-1 decision 9): Claude's stream has a heartbeat every 30 s
/// while a tool runs.
pub const DEFAULT_TURN_SILENCE_SECS: i64 = 15 * 60;
/// A headless worker's turn running this long is stopped.
pub const DEFAULT_TURN_LIMIT_SECS: i64 = 4 * 60 * 60;

/// The event the supervisor records with the values it loaded at start.
pub const STALL_CONFIG_LOADED: &str = super::event_kind::STALL_CONFIG_LOADED;

/// One background task an idle marker lists as `running`: its ID and what
/// it runs, as the agent described it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BackgroundTask {
    pub id: String,
    pub description: String,
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StallConfig {
    pub idle_without_receipt_secs: i64,
    pub send_confirm_secs: i64,
    pub background_alert_secs: i64,
    pub idle_process_secs: i64,
    pub screen_idle_secs: i64,
    pub turn_silence_secs: i64,
    pub turn_limit_secs: i64,
    /// The thresholds a test set below a second (task 1045), which the
    /// checks use in place of the seconds; `[stall]` never sets them.
    #[serde(skip)]
    pub millis: StallMillis,
}

/// The thresholds of a [`StallConfig`] set in milliseconds, by the
/// position of their key in [`StallConfig::KEYS`]: none unless a test
/// sets one ([`StallConfig::with_millis`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StallMillis([Option<u64>; 7]);

impl Default for StallConfig {
    fn default() -> Self {
        Self {
            idle_without_receipt_secs: DEFAULT_IDLE_WITHOUT_RECEIPT_SECS,
            send_confirm_secs: DEFAULT_SEND_CONFIRM_SECS,
            background_alert_secs: DEFAULT_BACKGROUND_ALERT_SECS,
            idle_process_secs: DEFAULT_IDLE_PROCESS_SECS,
            screen_idle_secs: DEFAULT_SCREEN_IDLE_SECS,
            turn_silence_secs: DEFAULT_TURN_SILENCE_SECS,
            turn_limit_secs: DEFAULT_TURN_LIMIT_SECS,
            millis: StallMillis::default(),
        }
    }
}

impl StallConfig {
    /// The setting names of the `[stall]` table.
    pub const KEYS: [&str; 7] = [
        "idle_without_receipt_secs",
        "send_confirm_secs",
        "background_alert_secs",
        "idle_process_secs",
        "screen_idle_secs",
        "turn_silence_secs",
        "turn_limit_secs",
    ];

    /// The setting `key` set to `secs` (and no longer in milliseconds);
    /// `None` for a key the table does not have.
    pub fn set(&mut self, key: &str, secs: i64) -> Option<()> {
        *self.field(key)? = secs;
        self.millis.0[Self::index(key)?] = None;
        Some(())
    }

    /// The setting `key` set to `millis` milliseconds, for a test that
    /// waits less than a second: the checks wait `millis`
    /// ([`Self::threshold`]), and its seconds, which events record, are
    /// `millis` rounded up to a whole second. `None` for a key the table
    /// does not have.
    pub fn set_millis(&mut self, key: &str, millis: u64) -> Option<()> {
        *self.field(key)? = i64::try_from(millis.div_ceil(1000).max(1)).unwrap_or(i64::MAX);
        self.millis.0[Self::index(key)?] = Some(millis);
        Some(())
    }

    /// [`Self::set_millis`] of `key`, which must be one of [`Self::KEYS`].
    #[must_use]
    pub fn with_millis(mut self, key: &str, millis: u64) -> Self {
        self.set_millis(key, millis)
            .unwrap_or_else(|| panic!("{key} is not a [stall] setting"));
        self
    }

    /// How long the setting `key` is: its milliseconds when a test set
    /// them, else its seconds (a value that is not positive is zero).
    /// `None` for a key the table does not have.
    pub fn threshold(&self, key: &str) -> Option<Duration> {
        let mut copy = *self;
        let secs = *copy.field(key)?;
        Some(match self.millis_of(key) {
            Some(millis) => Duration::from_millis(millis),
            None => Duration::from_secs(u64::try_from(secs).unwrap_or(0)),
        })
    }

    /// The milliseconds a test set `key` to, if it did.
    fn millis_of(&self, key: &str) -> Option<u64> {
        self.millis.0[Self::index(key)?]
    }

    fn index(key: &str) -> Option<usize> {
        Self::KEYS.iter().position(|k| *k == key)
    }

    fn field(&mut self, key: &str) -> Option<&mut i64> {
        Some(match key {
            "idle_without_receipt_secs" => &mut self.idle_without_receipt_secs,
            "send_confirm_secs" => &mut self.send_confirm_secs,
            "background_alert_secs" => &mut self.background_alert_secs,
            "idle_process_secs" => &mut self.idle_process_secs,
            "screen_idle_secs" => &mut self.screen_idle_secs,
            "turn_silence_secs" => &mut self.turn_silence_secs,
            "turn_limit_secs" => &mut self.turn_limit_secs,
            _ => return None,
        })
    }

    fn known(&self, key: &str) -> Duration {
        self.threshold(key).unwrap_or_default()
    }

    /// `idle_without_receipt_secs` as a [`Duration`].
    pub fn idle_without_receipt(&self) -> Duration {
        self.known("idle_without_receipt_secs")
    }

    /// `send_confirm_secs` as a [`Duration`].
    pub fn send_confirm(&self) -> Duration {
        self.known("send_confirm_secs")
    }

    /// `background_alert_secs` as a [`Duration`].
    pub fn background_alert(&self) -> Duration {
        self.known("background_alert_secs")
    }

    /// `idle_process_secs` as a [`Duration`].
    pub fn idle_process(&self) -> Duration {
        self.known("idle_process_secs")
    }

    /// `screen_idle_secs` as a [`Duration`].
    pub fn screen_idle(&self) -> Duration {
        self.known("screen_idle_secs")
    }

    /// The limits a headless worker's turns are held to.
    pub fn turn_limits(&self) -> super::turn::TurnLimits {
        super::turn::TurnLimits {
            silence_secs: self.turn_silence_secs,
            limit_secs: self.turn_limit_secs,
            silence_ms: self.millis_of("turn_silence_secs"),
            limit_ms: self.millis_of("turn_limit_secs"),
        }
    }

    /// The values of the latest `stall_config_loaded` in `events`: what the
    /// supervisor that recorded it runs with. A value missing from the
    /// payload keeps its default.
    pub fn loaded(events: &[RunEvent]) -> Option<Self> {
        let event = events
            .iter()
            .rev()
            .find(|event| event.kind == STALL_CONFIG_LOADED)?;
        let mut config = Self::default();
        for key in Self::KEYS {
            if let Some(secs) = event
                .payload
                .get(key)
                .and_then(Value::as_i64)
                .filter(|&secs| secs > 0)
            {
                config.set(key, secs);
            }
        }
        Some(config)
    }
}

/// The file the agent's `Stop` hook appends each idle marker to, one line
/// per marker (`<unix seconds>\t<marker>`), next to the marker: the
/// history a background task's first appearance is read from (the marker
/// itself is replaced each turn and carries no start time). A marker that
/// lists no running task replaces the log, which so holds only the current
/// streak (task 422).
pub const IDLE_LOG: &str = "idle.log";

/// When each background task of the last of `markers` (oldest first, each
/// with its time and the tasks it lists as running) was first listed in
/// the unbroken streak of markers that lead up to it. A marker that does
/// not list a task ends its streak, so a task ID a later session reuses is
/// timed from its own first appearance.
pub fn background_first_seen<I>(markers: I) -> HashMap<String, i64>
where
    I: IntoIterator<Item = (i64, Vec<BackgroundTask>)>,
{
    let mut seen: HashMap<String, i64> = HashMap::new();
    for (at, tasks) in markers {
        seen = tasks
            .into_iter()
            .map(|task| {
                let first = seen.get(&task.id).map_or(at, |&first| first.min(at));
                (task.id, first)
            })
            .collect();
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;
    use serde_json::json;

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    #[test]
    fn the_latest_loaded_config_wins_over_the_defaults() {
        assert_eq!(StallConfig::loaded(&[]), None);
        let events = [
            event(1, STALL_CONFIG_LOADED, json!({"send_confirm_secs": 5})),
            event(
                2,
                STALL_CONFIG_LOADED,
                json!({"idle_without_receipt_secs": 600, "send_confirm_secs": 0}),
            ),
            event(3, "run_claimed", json!({})),
        ];
        assert_eq!(
            StallConfig::loaded(&events),
            Some(StallConfig {
                idle_without_receipt_secs: 600,
                ..StallConfig::default()
            })
        );
        assert_eq!(StallConfig::default().set("other", 1), None);
    }

    /// Task 1045: a test sets a threshold in milliseconds; the checks wait
    /// that long, events keep whole seconds (rounded up), and setting the
    /// seconds again drops the milliseconds. `[stall]`'s seconds are
    /// thresholds of whole seconds.
    #[test]
    fn a_threshold_set_in_milliseconds_stands_in_for_its_seconds() {
        let config = StallConfig::default();
        assert_eq!(config.idle_without_receipt(), Duration::from_secs(20 * 60));
        assert_eq!(config.send_confirm(), Duration::from_secs(60));
        assert_eq!(config.background_alert(), Duration::from_secs(30 * 60));
        assert_eq!(config.idle_process(), Duration::from_secs(30 * 60));
        assert_eq!(config.screen_idle(), Duration::from_secs(2 * 60));
        let short = StallConfig::KEYS
            .iter()
            .fold(config, |config, key| config.with_millis(key, 200));
        for key in StallConfig::KEYS {
            assert_eq!(
                short.threshold(key),
                Some(Duration::from_millis(200)),
                "{key}"
            );
        }
        assert_eq!(short.idle_without_receipt_secs, 1);
        assert_eq!(short.turn_limits().silence(), Duration::from_millis(200));
        assert_eq!(short.turn_limits().limit(), Duration::from_millis(200));
        let json = serde_json::to_value(short).unwrap();
        assert_eq!(json["send_confirm_secs"], 1);
        assert!(json.get("millis").is_none(), "{json}");
        let long = config.with_millis("send_confirm_secs", 2500);
        assert_eq!(long.send_confirm_secs, 3);
        assert_eq!(long.send_confirm(), Duration::from_millis(2500));
        let mut again = long;
        again.set("send_confirm_secs", 2).unwrap();
        assert_eq!(again.send_confirm(), Duration::from_secs(2));
        assert_eq!(
            again,
            StallConfig {
                send_confirm_secs: 2,
                ..config
            }
        );
        let mut config = config;
        assert_eq!(config.set_millis("other", 1), None);
        assert_eq!(config.threshold("other"), None);
        assert_eq!(
            StallConfig {
                screen_idle_secs: -1,
                ..config
            }
            .screen_idle(),
            Duration::ZERO
        );
    }

    #[test]
    #[should_panic(expected = "other is not a [stall] setting")]
    fn with_millis_takes_only_a_setting_of_the_table() {
        let _ = StallConfig::default().with_millis("other", 1);
    }

    fn task(id: &str) -> BackgroundTask {
        BackgroundTask {
            id: id.to_owned(),
            ..BackgroundTask::default()
        }
    }

    #[test]
    fn a_background_task_is_timed_from_the_first_marker_of_its_streak() {
        assert!(background_first_seen([]).is_empty());
        let seen = background_first_seen([
            (10, vec![task("b1")]),
            // b1 ended here: its ID listed again later starts a new streak.
            (20, vec![task("b2")]),
            (30, vec![task("b1"), task("b2")]),
            (40, vec![task("b1"), task("b2"), task("b3")]),
        ]);
        assert_eq!(
            seen,
            HashMap::from([("b1".into(), 30), ("b2".into(), 20), ("b3".into(), 40)])
        );
        // A task the last marker does not list is not running.
        let seen = background_first_seen([(10, vec![task("b1")]), (20, vec![])]);
        assert!(seen.is_empty());
    }

    /// Task 422: the hook drops the lines before a marker that lists no
    /// running task, and the times read from what is left are the same.
    #[test]
    fn the_lines_before_a_marker_without_running_tasks_do_not_count() {
        let whole = vec![
            (10, vec![task("b1")]),
            (20, vec![task("b1"), task("b2")]),
            (30, vec![]),
            (40, vec![task("b2")]),
            (50, vec![task("b1"), task("b2")]),
        ];
        let tail = whole[2..].to_vec();
        assert_eq!(
            background_first_seen(whole),
            background_first_seen(tail.clone())
        );
        assert_eq!(
            background_first_seen(tail),
            HashMap::from([("b1".into(), 50), ("b2".into(), 40)])
        );
    }
}
