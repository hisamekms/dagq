//! How much of a run's work went through the resource broker ([Broker]
//! tool use, goal 59's (2)): with the broker's tools handed to a worker
//! (`preferred`, and the attempts `required` refuses), a `PreToolUse` hook
//! of its settings appends the name of each built-in file or command tool
//! it calls ([`DIRECT_TOOLS`]) to [`DIRECT_TOOLS_LOG`] in the run dir, one
//! name a line and nothing else (no path, content or command). When the
//! run ends, the supervisor counts those lines and the broker's audit lines
//! of the run by op ([`ToolUsage::count`]) and records them as the run's
//! `broker_tool_use`; `show` puts the latest one on the run
//! ([`latest_tool_use`]). A run without the broker's tools (`disabled`, or
//! `broker_unavailable`) has no hook and records none.
//!
//! [Broker]: ../../docs/design/broker.md

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use super::{RunEvent, RunId, event_kind};

/// The file in the run dir the hook appends to. Outside `<run dir>/broker`,
/// which the revoke of the run's token removes.
pub const DIRECT_TOOLS_LOG: &str = "broker-direct-tools.log";

/// Claude Code's built-in tools that do what the broker's tools do (read,
/// write and edit files, list them, run commands), in the order of their
/// names: the hook's matchers, one each, and the only names counted.
pub const DIRECT_TOOLS: [&str; 9] = [
    "Bash",
    "Edit",
    "Glob",
    "Grep",
    "LS",
    "MultiEdit",
    "NotebookEdit",
    "Read",
    "Write",
];

/// The audit's op of the broker's health, which no tool calls.
const HEALTH_OP: &str = "health";

/// A run's calls through the broker and around it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ToolUsage {
    /// The broker's audit lines of the run with an op, health aside,
    /// refused ones too.
    pub brokered: u64,
    pub brokered_by_op: BTreeMap<String, u64>,
    /// The built-in tools' calls the hook saw.
    pub direct: u64,
    pub direct_by_tool: BTreeMap<String, u64>,
}

impl ToolUsage {
    /// Count `direct_log` (the hook's lines; a name that is not one of
    /// [`DIRECT_TOOLS`], which the worker could write there, is not
    /// counted) and the audit `entries` of `run` (the broker's lines as
    /// it wrote them; another run's, the health and a line without an op
    /// are not).
    pub fn count(direct_log: &str, entries: &[Value], run: &RunId) -> Self {
        let mut usage = Self::default();
        for name in direct_log.lines().map(str::trim) {
            if DIRECT_TOOLS.contains(&name) {
                usage.direct += 1;
                *usage.direct_by_tool.entry(name.to_owned()).or_default() += 1;
            }
        }
        for entry in entries {
            if entry["run_id"].as_str() != Some(run.as_str()) {
                continue;
            }
            match entry["op"].as_str() {
                Some(op) if op != HEALTH_OP => {
                    usage.brokered += 1;
                    *usage.brokered_by_op.entry(op.to_owned()).or_default() += 1;
                }
                _ => {}
            }
        }
        usage
    }

    /// The payload of `broker_tool_use`.
    pub fn payload(&self) -> Value {
        json!(self)
    }
}

/// Whether an ended run's counts are recorded with the revoke of its
/// tokens: when the revoke retired a mark (`revoked` is `Some(true)`; a
/// later pass over another of its marks revokes none and records nothing
/// more). When the revoke failed (`None`), only when no mark of the run is
/// left (`marks_left` is `Some(false)`): it failed after retiring them,
/// removing a file, and no pass sees the run again. With marks left, or
/// marks that could not be read (`None`), the next pass tries the revoke
/// and records then.
pub fn records_tool_use(revoked: Option<bool>, marks_left: Option<bool>) -> bool {
    match revoked {
        Some(revoked) => revoked,
        None => marks_left == Some(false),
    }
}

/// The payload of the latest `broker_tool_use` of `run` among `events`.
pub fn latest_tool_use(events: &[RunEvent], run: &RunId) -> Option<Value> {
    events
        .iter()
        .rev()
        .find(|event| {
            event.kind == event_kind::BROKER_TOOL_USE && event.run_id.as_ref() == Some(run)
        })
        .map(|event| event.payload.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, EventKind};

    fn run(id: &str) -> RunId {
        RunId::new(id).unwrap()
    }

    #[test]
    fn counts_the_known_tools_and_the_runs_ops_but_not_the_health() {
        let log = "Read\nRead\nBash\n\n  Edit \nrm -rf /\nmcp__dagq-broker__read_file\n";
        let entries = [
            json!({"run_id": "r1", "op": "fs.read", "result": "ok"}),
            json!({"run_id": "r1", "op": "fs.read", "result": "workspace_violation"}),
            json!({"run_id": "r1", "op": "process.exec", "result": "ok"}),
            json!({"run_id": "r1", "op": "health", "result": "ok"}),
            json!({"run_id": "r1", "op": null, "result": "invalid_request"}),
            json!({"run_id": "r2", "op": "git.commit", "result": "ok"}),
            json!({"run_id": null, "op": "fs.write", "result": "unauthorized"}),
        ];
        let usage = ToolUsage::count(log, &entries, &run("r1"));
        assert_eq!(
            usage.payload(),
            json!({
                "brokered": 3,
                "brokered_by_op": {"fs.read": 2, "process.exec": 1},
                "direct": 4,
                "direct_by_tool": {"Bash": 1, "Edit": 1, "Read": 2},
            })
        );
    }

    #[test]
    fn counts_are_recorded_once_and_not_lost_by_a_failed_revoke() {
        // A revoke that retired a mark records; one that found none (a
        // later mark of the same run) does not, whatever is left.
        assert!(records_tool_use(Some(true), None));
        assert!(!records_tool_use(Some(false), None));
        assert!(!records_tool_use(Some(false), Some(false)));
        // A failed revoke records only when no mark of the run is left.
        assert!(records_tool_use(None, Some(false)));
        assert!(!records_tool_use(None, Some(true)));
        assert!(!records_tool_use(None, None));
    }

    #[test]
    fn nothing_counted_is_zero() {
        assert_eq!(
            ToolUsage::count("", &[], &run("r1")).payload(),
            json!({"brokered": 0, "brokered_by_op": {}, "direct": 0, "direct_by_tool": {}})
        );
    }

    #[test]
    fn the_latest_tool_use_of_the_run_wins() {
        let event = |id: i64, run_id: &str, kind: &str, direct: u64| RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: Some(run(run_id)),
            kind: kind.to_owned(),
            payload: json!({"direct": direct}),
            created_at: String::new(),
            actor: None,
        };
        let events = [
            event(1, "r1", event_kind::BROKER_TOOL_USE, 1),
            event(2, "r1", event_kind::BROKER_TOOL_USE, 2),
            event(3, "r2", event_kind::BROKER_TOOL_USE, 3),
            event(4, "r1", EventKind::BrokerTokenRevoked.as_str(), 4),
        ];
        assert_eq!(
            latest_tool_use(&events, &run("r1")),
            Some(json!({"direct": 2}))
        );
        assert_eq!(latest_tool_use(&events, &run("r3")), None);
    }
}
