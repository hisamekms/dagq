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
    super::{
        UPDATE_E2E_PASSED, UPDATE_EVENT_KINDS, UPDATE_FAILED, UPDATE_INSTALLED, e2e_quarantine,
    },
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
    /// The e2e gate a build of main passes before it is put in place
    /// (ADR-t963-1 decision 1).
    pub e2e: E2eGateStats,
    /// The failures of a person's `dagq install` (`update_failed` with
    /// `source: "install"`, ADR-0073 decision 14) by `stage`: no job's,
    /// so they count under [`INSTALL_FAILED`] in `by_kind` and not in
    /// `failed_by_stage`.
    pub install_failed_by_stage: BTreeMap<String, i64>,
}

/// How the e2e gate of the automatic update went in a window: the
/// `update_e2e_passed` and the `update_failed` at the `e2e` stage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct E2eGateStats {
    pub passed: i64,
    /// Failed ones, those that ran past their timeout and those that could
    /// not start included.
    pub failed: i64,
    pub timed_out: i64,
    /// The seconds of the e2e that ran (`secs`), summed and the longest.
    pub secs_total: i64,
    pub secs_max: i64,
    /// Each e2e test the gates of the window saw fail, or that the latest
    /// gate's `.config/e2e-quarantine.toml` marks, by name (ADR-t1165-1
    /// decision 4, task 1166).
    pub tests: BTreeMap<String, E2eTestStats>,
}

/// How one e2e test fared in the gates of a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct E2eTestStats {
    /// The gates it failed in (the first run's failures: the rerun's
    /// `tests`, or `failed_tests` of a gate that did not rerun).
    pub failed: i64,
    /// Those whose rerun it passed (`flaky`).
    pub passed_on_rerun: i64,
    /// Those whose rerun it failed too under a mark that held
    /// (`quarantined` of a gate whose rerun finished).
    pub quarantined: i64,
    /// Those that failed at the `e2e` stage with it neither flaky nor
    /// quarantined: it failed the gate.
    pub failed_gate: i64,
    /// The gates right before the window's end it failed the rerun of in a
    /// row, counted as the gate counts them for its mark
    /// ([`e2e_quarantine::failures_in_a_row`]) over the window's gates.
    pub failures_in_a_row: i64,
    /// The gate events it failed in, the newest first, at most
    /// [`TEST_EVENT_IDS`].
    pub event_ids: Vec<EventId>,
    /// Whether the window's latest gate read a mark for it, and the task
    /// that mark names.
    pub marked: bool,
    pub mark_task: Option<i64>,
}

/// How many of a test's gate events [`E2eTestStats::event_ids`] keeps.
pub const TEST_EVENT_IDS: usize = 5;

/// The names in the array `key` of `value`.
fn names<'a>(value: &'a Value, key: &str) -> Vec<&'a str> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

impl E2eGateStats {
    /// Count the tests of one gate event (`update_e2e_passed`, or
    /// `update_failed` at the `e2e` stage).
    fn add_tests(&mut self, event: &RunEvent) {
        let payload = &event.payload;
        let rerun = payload.get("rerun").filter(|rerun| rerun.is_object());
        let failed = match rerun {
            Some(rerun) => names(rerun, "tests"),
            None => names(payload, "failed_tests"),
        };
        // A rerun past its timeout or that could not start told nothing of
        // its tests, so no mark let them through.
        let rerun_told =
            rerun.is_some_and(|rerun| rerun["timed_out"] != true && rerun.get("error").is_none());
        let flaky = names(payload, "flaky");
        let quarantined = names(payload, "quarantined");
        let gate_failed = event.kind == UPDATE_FAILED;
        for name in failed {
            let test = self.tests.entry(name.to_owned()).or_default();
            test.failed += 1;
            test.event_ids.insert(0, event.id);
            test.event_ids.truncate(TEST_EVENT_IDS);
            let held = rerun_told && quarantined.contains(&name);
            if flaky.contains(&name) {
                test.passed_on_rerun += 1;
            } else if held {
                test.quarantined += 1;
            } else if gate_failed {
                test.failed_gate += 1;
            }
        }
    }

    /// Mark the tests the latest gate's marks name, and count each test's
    /// failures in a row over `gates` (oldest first).
    fn finish_tests(&mut self, gates: &[&RunEvent]) {
        let marks = gates
            .last()
            .and_then(|event| event.payload.get("quarantine"))
            .and_then(|quarantine| quarantine.get("marks"))
            .and_then(Value::as_array);
        for mark in marks.into_iter().flatten() {
            let Some(name) = mark.get("name").and_then(Value::as_str) else {
                continue;
            };
            let test = self.tests.entry(name.to_owned()).or_default();
            test.marked = true;
            test.mark_task = mark.get("task").and_then(Value::as_i64);
        }
        for (name, test) in &mut self.tests {
            test.failures_in_a_row =
                e2e_quarantine::failures_in_a_row(gates.iter().rev().copied(), name) as i64;
        }
    }
}

impl E2eGateStats {
    fn add_secs(&mut self, event: &RunEvent) {
        if let Some(secs) = event.payload.get("secs").and_then(Value::as_i64) {
            self.secs_total += secs;
            self.secs_max = self.secs_max.max(secs);
        }
    }
}

/// The `stage` of an `update_failed` whose build failed its e2e gate.
pub const E2E_STAGE: &str = "e2e";

/// The `by_kind` key of a plugin-only job's `update_installed`, which
/// brought the plugin to a release without replacing the binary.
pub const PLUGIN_ONLY_INSTALLED: &str = "update_installed_plugin_only";

/// The `by_kind` key of the `update_failed` of a person's `dagq install`
/// (`source: "install"`), apart from the jobs' failures.
pub const INSTALL_FAILED: &str = "update_failed_install";

/// Whether `event` was written by a person's `dagq install`.
fn by_install(event: &RunEvent) -> bool {
    event.payload.get("source").and_then(Value::as_str) == Some(super::super::INSTALL_SOURCE)
}

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
    let mut gates = Vec::new();
    for event in events.iter().filter(|event| {
        event.id > after
            && event.id <= upto
            && UPDATE_EVENT_KINDS.contains(&event.kind.as_str())
            && counts(event.task_id)
    }) {
        stats.count += 1;
        let plugin_only = event.kind == UPDATE_INSTALLED && plugin_only(event);
        let install = event.kind == UPDATE_FAILED && by_install(event);
        let kind = if plugin_only {
            PLUGIN_ONLY_INSTALLED
        } else if install {
            INSTALL_FAILED
        } else {
            event.kind.as_str()
        };
        *stats.by_kind.entry(kind.to_owned()).or_default() += 1;
        let text = |key: &str| event.payload.get(key).and_then(Value::as_str);
        match event.kind.as_str() {
            UPDATE_FAILED if install => {
                let stage = text("stage").unwrap_or(UNKNOWN);
                *stats
                    .install_failed_by_stage
                    .entry(stage.to_owned())
                    .or_default() += 1;
            }
            UPDATE_FAILED => {
                let stage = text("stage").unwrap_or(UNKNOWN);
                *stats.failed_by_stage.entry(stage.to_owned()).or_default() += 1;
                if stage == E2E_STAGE {
                    stats.e2e.failed += 1;
                    if event.payload.get("timed_out").and_then(Value::as_bool) == Some(true) {
                        stats.e2e.timed_out += 1;
                    }
                    stats.e2e.add_secs(event);
                    stats.e2e.add_tests(event);
                    gates.push(event);
                }
            }
            UPDATE_E2E_PASSED => {
                stats.e2e.passed += 1;
                stats.e2e.add_secs(event);
                stats.e2e.add_tests(event);
                gates.push(event);
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
    stats.e2e.finish_tests(&gates);
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

    /// A person's install's failure counts apart from the jobs': under its
    /// own kind and stages, not in `failed_by_stage`.
    #[test]
    fn a_persons_install_failure_counts_apart_from_the_jobs() {
        let events = [
            event(1, "update_failed", json!({"stage": "watch"})),
            event(
                2,
                "update_failed",
                json!({"stage": "watch", "source": "install"}),
            ),
            event(
                3,
                "update_failed",
                json!({"stage": "handoff", "source": "install"}),
            ),
        ];
        let stats = updates(&events, EventId::new(0), EventId::new(3), |_| true);
        assert_eq!(stats.count, 3);
        assert_eq!(stats.by_kind["update_failed"], 1);
        assert_eq!(stats.by_kind[INSTALL_FAILED], 2);
        assert_eq!(stats.failed_by_stage, BTreeMap::from([("watch".into(), 1)]));
        assert_eq!(
            stats.install_failed_by_stage,
            BTreeMap::from([("handoff".into(), 1), ("watch".into(), 1)])
        );
    }

    /// The e2e gate counts its passes, failures and timeouts, with their
    /// time.
    #[test]
    fn counts_the_e2e_gate_by_outcome_with_its_time() {
        let events = [
            event(1, "update_e2e_passed", json!({"secs": 170})),
            event(2, "update_failed", json!({"stage": "e2e", "secs": 200})),
            event(
                3,
                "update_failed",
                json!({"stage": "e2e", "secs": 1800, "timed_out": true}),
            ),
            event(4, "update_failed", json!({"stage": "e2e"})),
            event(5, "update_failed", json!({"stage": "build"})),
        ];
        let stats = updates(&events, EventId::new(0), EventId::new(5), |_| true);
        assert_eq!(
            stats.e2e,
            E2eGateStats {
                passed: 1,
                failed: 3,
                timed_out: 1,
                secs_total: 2170,
                secs_max: 1800,
                tests: BTreeMap::new(),
            }
        );
        assert_eq!(stats.failed_by_stage["e2e"], 3);
        assert_eq!(stats.by_kind["update_e2e_passed"], 1);
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

    /// The gates' tests count by name: a failure passed on the rerun, one
    /// held by a mark, one that failed the gate, one whose rerun ran past
    /// its timeout (no mark held it), the failures in a row at the window's
    /// end, the newest events first and the marks of the latest gate.
    #[test]
    fn counts_the_e2e_gate_by_test() {
        let marks = json!({"marks": [{"name": "b", "task": 1120}, {"name": "d", "task": 7}]});
        let events = [
            event(
                1,
                "update_e2e_passed",
                json!({"flaky": ["a"], "quarantined": [], "rerun": {"tests": ["a"], "failed": []}}),
            ),
            event(
                2,
                "update_e2e_passed",
                json!({"flaky": ["a"], "quarantined": ["b"],
                    "rerun": {"tests": ["a", "b"], "failed": ["b"]}, "quarantine": marks}),
            ),
            event(
                3,
                "update_failed",
                json!({"stage": "e2e", "failed_tests": ["b", "c"], "flaky": [], "quarantined": ["b"],
                    "rerun": {"tests": ["b", "c"], "failed": ["b", "c"]}, "quarantine": marks}),
            ),
            // No rerun: its failures fail the gate.
            event(
                4,
                "update_failed",
                json!({"stage": "e2e", "failed_tests": ["c"], "timed_out": false}),
            ),
            // The rerun ran past its timeout: no mark held.
            event(
                5,
                "update_failed",
                json!({"stage": "e2e", "failed_tests": ["b"], "flaky": [], "quarantined": ["b"],
                    "rerun": {"tests": ["b"], "failed": ["b"], "timed_out": true}, "quarantine": marks}),
            ),
            event(6, "update_failed", json!({"stage": "build"})),
        ];
        let stats = updates(&events, EventId::new(0), EventId::new(6), |_| true);
        let tests = &stats.e2e.tests;
        assert_eq!(tests.keys().collect::<Vec<_>>(), ["a", "b", "c", "d"]);
        let ids = |ids: &[i64]| ids.iter().map(|id| EventId::new(*id)).collect::<Vec<_>>();
        assert_eq!(
            tests["a"],
            E2eTestStats {
                failed: 2,
                passed_on_rerun: 2,
                event_ids: ids(&[2, 1]),
                ..Default::default()
            }
        );
        assert_eq!(
            tests["b"],
            E2eTestStats {
                failed: 3,
                quarantined: 2,
                failed_gate: 1,
                // Gate 5 told nothing and 4 did not rerun: 3 and 2 count.
                failures_in_a_row: 2,
                event_ids: ids(&[5, 3, 2]),
                marked: true,
                mark_task: Some(1120),
                ..Default::default()
            }
        );
        assert_eq!(
            tests["c"],
            E2eTestStats {
                failed: 2,
                failed_gate: 2,
                failures_in_a_row: 1,
                event_ids: ids(&[4, 3]),
                ..Default::default()
            }
        );
        assert_eq!(
            tests["d"],
            E2eTestStats {
                marked: true,
                mark_task: Some(7),
                ..Default::default()
            }
        );

        // The events keep the newest few; a window without the marked gate
        // marks nothing.
        let many: Vec<RunEvent> = (1..=7)
            .map(|id| {
                event(
                    id,
                    "update_failed",
                    json!({"stage": "e2e", "failed_tests": ["c"]}),
                )
            })
            .collect();
        let stats = updates(&many, EventId::new(0), EventId::new(7), |_| true);
        assert_eq!(stats.e2e.tests["c"].event_ids, ids(&[7, 6, 5, 4, 3]));
        assert_eq!(stats.e2e.tests["c"].failures_in_a_row, 0);
        let later = updates(&events, EventId::new(3), EventId::new(4), |_| true);
        assert_eq!(later.e2e.tests.keys().collect::<Vec<_>>(), ["c"]);
        assert!(!later.e2e.tests["c"].marked);
    }
}
