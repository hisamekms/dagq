//! The steps of a headless Codex turn (ADR-t1662-3 decision 1), collected
//! from the same JSONL its [`super::codex_turns`] reads:
//!
//! - a model's turn: from `turn.started`, or the end of the item before, to
//!   the start of the next item that runs something or the completion of
//!   one the model wrote (`agent_message`, `reasoning`), and from the last
//!   of them to `turn.completed` / `turn.failed`. The JSONL does not name
//!   the model: it is the session's.
//! - a tool's call: an item that runs something (`command_execution`,
//!   `mcp_tool_call`, `web_search`, `file_change`), from its
//!   `item.started` to its `item.completed` (one with no start read lasts
//!   0). A command keeps its shape (of the script its shell runs), its
//!   `exit_code` and its `aggregated_output`'s bytes.
//! - a subagent: a `collab_tool_call` item; one whose `tool` is `wait`
//!   waits for the agents it started, and is a wait.
//! - a wait: a top-level `error` that says the stream is retried
//!   (`Reconnecting…`, `retrying`) until the next line.
//!
//! The JSONL carries no time: each line is at the time the wrapper read
//! it. Nothing of what was said, a command's text or output, or a usage is
//! kept.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::application::StepReader;
use crate::domain::command_shape::{Subcommands, shape_value_with};
use crate::domain::session_step::{Step, StepKind};

/// The items that run something: a tool's call.
const CALL_ITEMS: [&str; 4] = [
    "command_execution",
    "mcp_tool_call",
    "web_search",
    "file_change",
];

/// The item of a subagent.
const SUBAGENT_ITEM: &str = "collab_tool_call";

/// The items the model writes.
const MODEL_ITEMS: [&str; 2] = ["agent_message", "reasoning"];

/// Collects the steps of one Codex turn.
#[derive(Debug, Default)]
pub struct CodexStepReader {
    /// The session's model.
    model: Option<String>,
    /// Since when the model has worked with no item running.
    asked: Option<i64>,
    /// The items started and not completed, by id: when, and the item.
    running: BTreeMap<String, (i64, Value)>,
    /// A retried stream's wait, since when.
    retrying: Option<i64>,
    /// The thread `thread.started` named.
    session: Option<String>,
    /// The subcommands its commands' shapes keep in plain.
    known: Subcommands,
}

impl CodexStepReader {
    /// The reader of a turn of a session of `model`.
    pub fn new(model: Option<&str>) -> Self {
        Self {
            model: model.map(str::to_owned),
            ..Self::default()
        }
    }

    /// The reader whose commands' shapes keep `known`'s subcommands in
    /// plain.
    #[must_use]
    pub fn knowing(mut self, known: Subcommands) -> Self {
        self.known = known;
        self
    }

    /// The model's turn from `asked` to `at`, which `asked` no longer
    /// waits on.
    fn model_turn(&mut self, at: i64) -> Option<Step> {
        let since = self.asked.take()?;
        let mut step = Step::new(0, StepKind::ModelTurn, since, (at - since).max(0));
        step.model = self.model.clone();
        Some(step)
    }

    fn started(&mut self, item: &Value, at: i64, steps: &mut Vec<Step>) {
        let kind = item["type"].as_str().unwrap_or_default();
        if !(CALL_ITEMS.contains(&kind) || kind == SUBAGENT_ITEM) {
            return;
        }
        steps.extend(self.model_turn(at));
        let id = item["id"].as_str().unwrap_or_default().to_owned();
        self.running.insert(id, (at, item.clone()));
    }

    fn completed(&mut self, item: &Value, at: i64, steps: &mut Vec<Step>) {
        let kind = item["type"].as_str().unwrap_or_default();
        if MODEL_ITEMS.contains(&kind) {
            if self.running.is_empty() {
                steps.extend(self.model_turn(at));
                self.asked = Some(at);
            }
            return;
        }
        if !(CALL_ITEMS.contains(&kind) || kind == SUBAGENT_ITEM) {
            return;
        }
        let id = item["id"].as_str().unwrap_or_default();
        let started = match self.running.remove(id) {
            Some((started, _)) => started,
            // An item with no start read (a file's change) lasts 0.
            None => {
                steps.extend(self.model_turn(at));
                at
            }
        };
        steps.push(item_step(item, (started, at), true, &self.known));
        if self.running.is_empty() {
            self.asked = Some(at);
        }
    }
}

/// The step of `item`, from `started` to `at`; `ended` when its completion
/// was read (else the turn ended first, and it has no exit or output).
fn item_step(item: &Value, (started, at): (i64, i64), ended: bool, known: &Subcommands) -> Step {
    let kind = item["type"].as_str().unwrap_or_default();
    let step_kind = match (kind, item["tool"].as_str()) {
        (SUBAGENT_ITEM, Some("wait")) => StepKind::Wait,
        (SUBAGENT_ITEM, _) => StepKind::Subagent,
        _ => StepKind::ToolCall,
    };
    let mut step = Step::new(0, step_kind, started, (at - started).max(0));
    step.tool = Some(kind.to_owned());
    if kind == "command_execution" {
        step.command_shape = item["command"]
            .as_str()
            .and_then(|command| shape_value_with(command, known));
        if ended {
            step.exit_code = item["exit_code"].as_i64();
            step.output_bytes = item["aggregated_output"]
                .as_str()
                .map(|output| output.len() as i64);
        }
    }
    step
}

/// Whether a top-level `error`'s `message` says the stream is retried.
fn retried(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("reconnecting") || lower.contains("retrying")
}

impl StepReader for CodexStepReader {
    fn session(&self) -> Option<String> {
        self.session.clone()
    }

    fn line(&mut self, line: &str, at_ms: i64) -> Vec<Step> {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let mut steps = Vec::new();
        if let Some(since) = self.retrying.take() {
            steps.push(Step::new(0, StepKind::Wait, since, (at_ms - since).max(0)));
        }
        match event["type"].as_str() {
            Some("thread.started") => {
                self.session = event["thread_id"].as_str().map(str::to_owned);
            }
            Some("turn.started") => {
                self.asked.get_or_insert(at_ms);
            }
            Some("item.started") => self.started(&event["item"], at_ms, &mut steps),
            Some("item.completed") => self.completed(&event["item"], at_ms, &mut steps),
            Some("turn.completed" | "turn.failed") => {
                if self.asked.is_some_and(|since| at_ms > since) {
                    steps.extend(self.model_turn(at_ms));
                }
                self.asked = None;
            }
            Some("error") if event["message"].as_str().is_some_and(retried) => {
                self.retrying = Some(at_ms);
            }
            _ => {}
        }
        steps
    }

    fn finish(&mut self, at_ms: i64) -> Vec<Step> {
        let mut steps = Vec::new();
        if self.asked.is_some_and(|since| at_ms > since) {
            steps.extend(self.model_turn(at_ms));
        }
        self.asked = None;
        if let Some(since) = self.retrying.take() {
            steps.push(Step::new(0, StepKind::Wait, since, (at_ms - since).max(0)));
        }
        for (_, (started, item)) in std::mem::take(&mut self.running) {
            steps.push(item_step(&item, (started, at_ms), false, &self.known));
        }
        steps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn digest(word: &str) -> String {
        crate::domain::command_shape::DigestKey::test().digest(word)
    }
    use serde_json::json;

    fn item(kind: &str, phase: &str, item: Value) -> Value {
        let mut item = item;
        item["type"] = json!(kind);
        json!({"type": format!("item.{phase}"), "item": item})
    }

    /// The representative JSONL: reasoning, a command and its result in a
    /// login shell, a subagent and the wait for it, a retried stream, a
    /// file's change and the last message. Each is a step at its offset
    /// with what it ran, and none keeps what was said, a command's text or
    /// output, or tokens.
    #[test]
    fn a_codex_stream_makes_the_steps_of_its_turn() {
        let mut reader = CodexStepReader::new(Some("gpt-5.5-codex"))
            .knowing(crate::domain::command_shape::Subcommands::rust());
        let command = "/bin/zsh -lc 'cargo test --locked --test it runtime_claim::'";
        let events = [
            (0, json!({"type": "thread.started", "thread_id": "th"})),
            (10, json!({"type": "turn.started"})),
            (
                400,
                item(
                    "reasoning",
                    "completed",
                    json!({"id": "r", "text": "the secret plan"}),
                ),
            ),
            (
                500,
                item(
                    "command_execution",
                    "started",
                    json!({"id": "c1", "command": command, "status": "in_progress"}),
                ),
            ),
            (
                4_500,
                item(
                    "command_execution",
                    "completed",
                    json!({"id": "c1", "command": command,
                "aggregated_output": "ok 12345", "exit_code": 101, "status": "failed"}),
                ),
            ),
            (
                4_800,
                item(
                    "collab_tool_call",
                    "started",
                    json!({"id": "a1", "tool": "spawn_agent", "prompt": "review the secret"}),
                ),
            ),
            (
                4_900,
                item(
                    "collab_tool_call",
                    "completed",
                    json!({"id": "a1", "tool": "spawn_agent"}),
                ),
            ),
            (
                5_000,
                item(
                    "collab_tool_call",
                    "started",
                    json!({"id": "a2", "tool": "wait"}),
                ),
            ),
            (
                9_000,
                item(
                    "collab_tool_call",
                    "completed",
                    json!({"id": "a2", "tool": "wait"}),
                ),
            ),
            (
                9_100,
                json!({"type": "error", "message": "Reconnecting... 1/5 (stream disconnected)"}),
            ),
            (
                11_100,
                item(
                    "file_change",
                    "completed",
                    json!({"id": "f", "changes": [{"path": "/secret.rs"}]}),
                ),
            ),
            (
                11_500,
                item(
                    "agent_message",
                    "completed",
                    json!({"id": "m", "text": "done"}),
                ),
            ),
            (
                11_600,
                json!({"type": "turn.completed", "usage": {"input_tokens": 900, "output_tokens": 3}}),
            ),
        ];
        let mut steps: Vec<Step> = events
            .iter()
            .flat_map(|(at, event)| reader.line(&event.to_string(), *at))
            .collect();
        steps.extend(reader.finish(12_000));
        // The thread it named is the session a job's steps go as.
        assert_eq!(reader.session().as_deref(), Some("th"));
        let model = |at: i64, until: i64| {
            let mut step = Step::new(0, StepKind::ModelTurn, at, until - at);
            step.model = Some("gpt-5.5-codex".to_owned());
            step
        };
        let tool = |kind: StepKind, name: &str, at: i64, until: i64| {
            let mut step = Step::new(0, kind, at, until - at);
            step.tool = Some(name.to_owned());
            step
        };
        let mut shell = tool(StepKind::ToolCall, "command_execution", 500, 4_500);
        shell.command_shape = crate::domain::command_shape::shape_value_with(
            command,
            &crate::domain::command_shape::Subcommands::rust(),
        );
        shell.exit_code = Some(101);
        shell.output_bytes = Some(8);
        assert_eq!(
            steps,
            vec![
                model(10, 400),
                model(400, 500),
                shell,
                model(4_500, 4_800),
                tool(StepKind::Subagent, "collab_tool_call", 4_800, 4_900),
                model(4_900, 5_000),
                tool(StepKind::Wait, "collab_tool_call", 5_000, 9_000),
                Step::new(0, StepKind::Wait, 9_100, 2_000),
                model(9_000, 11_100),
                tool(StepKind::ToolCall, "file_change", 11_100, 11_100),
                model(11_100, 11_500),
                model(11_500, 11_600),
            ]
        );
        // The login shell's script is the shape.
        assert_eq!(
            steps[2].command_shape.as_ref().unwrap()["parts"][0]["program"],
            "cargo"
        );
        assert_eq!(
            steps[2].command_shape.as_ref().unwrap()["parts"][0]["args"][3],
            json!({"word": digest("it")})
        );
        let text = serde_json::to_string(&steps).unwrap();
        for kept_out in [
            "secret",
            "plan",
            "review",
            "runtime_claim",
            "zsh",
            "12345",
            "Reconnecting",
            "done\"",
            "usage",
            "token",
            "spawn_agent",
        ] {
            assert!(!text.contains(kept_out), "{kept_out} in {text}");
        }
    }

    /// A command still running when the turn is stopped ends there with no
    /// exit and no output.
    #[test]
    fn a_command_running_at_the_turns_end_is_ended_there() {
        let mut reader = CodexStepReader::new(None).knowing(
            crate::domain::command_shape::Subcommands::common()
                .keyed(Some(crate::domain::command_shape::DigestKey::test())),
        );
        for (at, event) in [
            (0, json!({"type": "turn.started"})),
            (
                100,
                item(
                    "command_execution",
                    "started",
                    json!({"id": "c", "command": "sleep 100"}),
                ),
            ),
        ] {
            assert!(reader.line(&event.to_string(), at).len() <= 1, "{event}");
        }
        let steps = reader.finish(3_000);
        assert_eq!(steps.len(), 1, "{steps:?}");
        assert_eq!(
            (
                steps[0].kind,
                steps[0].offset_ms,
                steps[0].duration_ms,
                steps[0].exit_code,
                steps[0].output_bytes
            ),
            (StepKind::ToolCall, 100, 2_900, None, None)
        );
        assert_eq!(
            steps[0].command_shape,
            Some(crate::domain::command_shape::shape_value("sleep 100"))
        );
    }
}
