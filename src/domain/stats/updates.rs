//! The automatic update of the fixed binary (ADR-0073 decision 17, task
//! 496): its `update_*` queue events of the window by kind, the failures by
//! the stage they failed at, the builds installed and apart from them the
//! releases a plugin-only job brought the plugin to, so how often the
//! binary was replaced and how often it failed can be read next to the
//! runs. Derived from `run_events` like the rest of `stats`.
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{
    super::{UPDATE_EVENT_KINDS, UPDATE_FAILED, UPDATE_INSTALLED},
    EventId, RunEvent, TaskId,
    asks::UNKNOWN,
};

/// The steps of the automatic update in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct UpdateStats {
    /// Every `update_*` event.
    pub count: i64,
    /// Those by kind (`update_started`, `update_installed`, ...); a
    /// plugin-only job's `update_installed` (`plugin_only: true`) counts
    /// under [`PLUGIN_ONLY_INSTALLED`] instead, as it replaced no binary.
    pub by_kind: BTreeMap<String, i64>,
    /// The `update_failed` events by `stage` (`build`, `check`, `install`,
    /// `handoff`, `watch`, `plugin`, `interrupted`).
    pub failed_by_stage: BTreeMap<String, i64>,
    /// The build identifiers `update_installed` put in place, oldest first.
    /// A plugin-only job's install is not among them.
    pub installed: Vec<String>,
    /// The releases a plugin-only job brought the installed plugin to
    /// (`update_installed` with `plugin_only: true`), oldest first.
    pub plugin_installed: Vec<String>,
}

/// The `by_kind` key of a plugin-only job's `update_installed`, which
/// brought the plugin to a release without replacing the binary.
pub const PLUGIN_ONLY_INSTALLED: &str = "update_installed_plugin_only";

/// Whether `event` is a plugin-only job's step (`plugin_only: true`).
pub fn plugin_only(event: &RunEvent) -> bool {
    event
        .payload
        .get("plugin_only")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Count the `update_*` events with `after < id <= upto`. They belong to no
/// task, so they count only when `counts` accepts no task (no `--goal`).
pub fn updates(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> UpdateStats {
    let mut stats = UpdateStats::default();
    for event in events.iter().filter(|event| {
        event.id > after
            && event.id <= upto
            && UPDATE_EVENT_KINDS.contains(&event.kind.as_str())
            && counts(event.task_id)
    }) {
        stats.count += 1;
        let plugin_only = event.kind == UPDATE_INSTALLED && plugin_only(event);
        let kind = if plugin_only {
            PLUGIN_ONLY_INSTALLED
        } else {
            event.kind.as_str()
        };
        *stats.by_kind.entry(kind.to_owned()).or_default() += 1;
        let text = |key: &str| event.payload.get(key).and_then(Value::as_str);
        match event.kind.as_str() {
            UPDATE_FAILED => {
                *stats
                    .failed_by_stage
                    .entry(text("stage").unwrap_or(UNKNOWN).to_owned())
                    .or_default() += 1;
            }
            UPDATE_INSTALLED if plugin_only => stats
                .plugin_installed
                .push(text("version").unwrap_or(UNKNOWN).to_owned()),
            UPDATE_INSTALLED => stats
                .installed
                .push(text("version").unwrap_or(UNKNOWN).to_owned()),
            _ => {}
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: "2026-09-26T01:00:00.000Z".to_owned(),
            actor: None,
        }
    }

    /// The steps count by kind, the failures by stage and the installs by
    /// version; other kinds and events outside the window are left out, and
    /// none counts for a goal.
    #[test]
    fn counts_the_steps_by_kind_the_failures_by_stage_and_the_installs() {
        let events = [
            event(1, "update_started", json!({"commit": "a"})),
            event(2, "update_built", json!({"commit": "a"})),
            event(3, "update_installed", json!({"version": "0.4.0-dev+a"})),
            event(4, "update_started", json!({"commit": "b"})),
            event(5, "update_failed", json!({"stage": "build"})),
            event(6, "update_retry", json!({"answer": "retry"})),
            event(7, "update_failed", json!({})),
            event(8, "run_claimed", json!({})),
            event(9, "update_installed", json!({"version": "0.4.0-dev+c"})),
        ];
        let all = updates(&events, EventId::new(0), EventId::new(8), |_| true);
        assert_eq!(all.count, 7);
        assert_eq!(all.by_kind["update_started"], 2);
        assert_eq!(all.by_kind["update_failed"], 2);
        assert!(!all.by_kind.contains_key("run_claimed"));
        assert_eq!(all.failed_by_stage["build"], 1);
        assert_eq!(all.failed_by_stage[UNKNOWN], 1);
        assert_eq!(all.installed, vec!["0.4.0-dev+a".to_owned()]);

        let later = updates(&events, EventId::new(5), EventId::new(9), |_| true);
        assert_eq!(later.count, 3);
        assert_eq!(later.installed, vec!["0.4.0-dev+c".to_owned()]);

        let goal = updates(&events, EventId::new(0), EventId::new(9), |task| {
            task.is_some()
        });
        assert_eq!(goal, UpdateStats::default());
    }

    /// A plugin-only job's `update_installed` counts apart from the binary
    /// installs: under its own `by_kind` key and in `plugin_installed`.
    #[test]
    fn a_plugin_only_install_counts_apart_from_the_binary_installs() {
        let events = [
            event(1, "update_installed", json!({"version": "0.5.0"})),
            event(
                2,
                "update_installed",
                json!({"version": "0.5.0", "plugin_only": true, "plugin": "0.5.0"}),
            ),
            event(
                3,
                "update_installed",
                json!({"version": "0.5.1", "plugin_only": false}),
            ),
        ];
        let stats = updates(&events, EventId::new(0), EventId::new(3), |_| true);
        assert_eq!(stats.count, 3);
        assert_eq!(stats.by_kind["update_installed"], 2);
        assert_eq!(stats.by_kind[PLUGIN_ONLY_INSTALLED], 1);
        assert_eq!(
            stats.installed,
            vec!["0.5.0".to_owned(), "0.5.1".to_owned()]
        );
        assert_eq!(stats.plugin_installed, vec!["0.5.0".to_owned()]);
    }
}
