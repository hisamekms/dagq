//! The steps of a headless Claude Code turn (ADR-t1662-3 decision 1),
//! collected from the same stream-json its [`super::claude_turns`] reads:
//!
//! - a model's turn: from the request (the turn's `system/init`, or the
//!   result that let the model go on: the last `tool_result` of the
//!   top-level agent's tools in the foreground) to the last `assistant`
//!   event of its message; a message of another id after it is another
//!   turn. A subagent's messages (with a `parent_tool_use_id`) are its own,
//!   not the top-level model's.
//! - a tool's call: from its `tool_use` to its `tool_result`, a subagent's
//!   tools included; `Bash` keeps its command's shape, whether it ran in
//!   the background, its exit (0, or the `Exit code N` an error says) and
//!   its output's bytes. The `Agent` (`Task`) tool is a subagent; a tool
//!   that only waits (for a wakeup, a monitor, a background output) is a
//!   wait.
//! - a wait: a retried API error (`system/api_retry`) until the next line.
//!
//! The stream carries no time: each line is at the time the wrapper read
//! it, late by up to its poll's interval. Nothing of what was said, a
//! tool's input or output, or a usage is kept.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::application::StepReader;
use crate::domain::command_shape::{Subcommands, shape_value_with};
use crate::domain::session_step::{Step, StepKind};

/// The tools that start a subagent.
const SUBAGENT_TOOLS: [&str; 2] = ["Agent", "Task"];

/// The tools that only wait (as `domain::worktime` counts them).
const WAIT_TOOLS: [&str; 4] = ["ScheduleWakeup", "Monitor", "TaskOutput", "BashOutput"];

/// A tool's call whose result was not read yet.
#[derive(Debug)]
struct Call {
    started: i64,
    name: String,
    /// Whether the top-level agent called it (not a subagent).
    top: bool,
    command: Option<String>,
    background: Option<bool>,
}

/// A model's message being read: from when the model was asked, and the
/// last of its events.
#[derive(Debug)]
struct Message {
    id: Option<String>,
    started: i64,
    last: i64,
    model: Option<String>,
}

/// Collects the steps of one Claude turn.
#[derive(Debug, Default)]
pub struct ClaudeStepReader {
    /// The model `system/init` named, or the session's.
    model: Option<String>,
    /// Since when the top-level model was asked and has not answered.
    asked: Option<i64>,
    message: Option<Message>,
    calls: BTreeMap<String, Call>,
    /// A retried API error's wait, since when.
    retrying: Option<i64>,
    /// The session `system/init` named.
    session: Option<String>,
    /// The subcommands its commands' shapes keep in plain.
    known: Subcommands,
}

impl ClaudeStepReader {
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

    /// End the model's message being read: its turn.
    fn end_message(&mut self) -> Option<Step> {
        let message = self.message.take()?;
        let mut step = Step::new(
            0,
            StepKind::ModelTurn,
            message.started,
            (message.last - message.started).max(0),
        );
        step.model = message.model.or_else(|| self.model.clone());
        Some(step)
    }

    fn assistant(&mut self, event: &Value, at: i64, steps: &mut Vec<Step>) {
        let message = &event["message"];
        let top = event["parent_tool_use_id"].is_null();
        if top && message["model"] != "<synthetic>" && event["error"].is_null() {
            let id = message["id"].as_str().map(str::to_owned);
            match &mut self.message {
                Some(open) if open.id.is_some() && open.id == id => open.last = at,
                _ => {
                    let since = self
                        .message
                        .as_ref()
                        .map(|open| open.last)
                        .or(self.asked.take())
                        .unwrap_or(at);
                    steps.extend(self.end_message());
                    self.message = Some(Message {
                        id,
                        started: since,
                        last: at,
                        model: message["model"].as_str().map(str::to_owned),
                    });
                }
            }
        }
        for item in message["content"].as_array().into_iter().flatten() {
            if item["type"] != "tool_use" {
                continue;
            }
            let Some(id) = item["id"].as_str() else {
                continue;
            };
            let name = item["name"].as_str().unwrap_or("tool").to_owned();
            let input = &item["input"];
            let bash = name == "Bash";
            self.calls.insert(
                id.to_owned(),
                Call {
                    started: at,
                    command: bash
                        .then(|| input["command"].as_str().map(str::to_owned))
                        .flatten(),
                    background: bash.then(|| input["run_in_background"] == true),
                    name,
                    top,
                },
            );
        }
    }

    fn user(&mut self, event: &Value, at: i64, steps: &mut Vec<Step>) {
        let mut ended = Vec::new();
        for item in event["message"]["content"].as_array().into_iter().flatten() {
            if item["type"] != "tool_result" {
                continue;
            }
            if let Some(call) = item["tool_use_id"]
                .as_str()
                .and_then(|id| self.calls.remove(id))
            {
                ended.push((call, item));
            }
        }
        // A result of the top-level agent's tool ends the message that
        // called it.
        let ended_top = ended.iter().any(|(call, _)| call.top);
        if ended_top {
            steps.extend(self.end_message());
        }
        for (call, item) in ended {
            let output = output_text(&item["content"]);
            let is_error = item["is_error"] == true;
            steps.push(call_step(
                call,
                at,
                Some((output.as_str(), is_error)),
                &self.known,
            ));
        }
        // The model goes on once the top-level agent's foreground tools
        // returned.
        if ended_top && !self.calls.values().any(|call| call.top) {
            self.asked = Some(at);
        }
    }
}

/// The text of a `tool_result`'s content: a string, or its text items.
fn output_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The exit a `Bash` result says: 0 for one that is no error, `N` for an
/// error that starts `Exit code N`, else none.
fn exit_code(output: &str, is_error: bool) -> Option<i64> {
    if !is_error {
        return Some(0);
    }
    output
        .strip_prefix("Exit code ")?
        .split(|c: char| !c.is_ascii_digit() && c != '-')
        .next()?
        .parse()
        .ok()
}

/// The step of `call`, ended at `at` with its result (its output and
/// whether it is an error), `None` when the turn ended first.
fn call_step(call: Call, at: i64, result: Option<(&str, bool)>, known: &Subcommands) -> Step {
    let kind = if SUBAGENT_TOOLS.contains(&call.name.as_str()) {
        StepKind::Subagent
    } else if WAIT_TOOLS.contains(&call.name.as_str()) {
        StepKind::Wait
    } else {
        StepKind::ToolCall
    };
    let mut step = Step::new(0, kind, call.started, (at - call.started).max(0));
    step.background = call.background;
    if let Some(command) = &call.command {
        step.command_shape = shape_value_with(command, known);
        step.exit_code = result.and_then(|(output, is_error)| exit_code(output, is_error));
    }
    step.output_bytes = result.map(|(output, _)| output.len() as i64);
    step.tool = Some(call.name);
    step
}

impl StepReader for ClaudeStepReader {
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
        match (event["type"].as_str(), event["subtype"].as_str()) {
            (Some("system"), Some("init")) => {
                if let Some(model) = event["model"].as_str() {
                    self.model = Some(model.to_owned());
                }
                if let Some(session) = event["session_id"].as_str() {
                    self.session = Some(session.to_owned());
                }
                self.asked.get_or_insert(at_ms);
            }
            (Some("system"), Some("api_retry")) => self.retrying = Some(at_ms),
            (Some("assistant"), _) => self.assistant(&event, at_ms, &mut steps),
            (Some("user"), _) => self.user(&event, at_ms, &mut steps),
            (Some("result"), _) => steps.extend(self.end_message()),
            _ => {}
        }
        steps
    }

    fn finish(&mut self, at_ms: i64) -> Vec<Step> {
        let mut steps: Vec<Step> = self.end_message().into_iter().collect();
        if let Some(since) = self.retrying.take() {
            steps.push(Step::new(0, StepKind::Wait, since, (at_ms - since).max(0)));
        }
        for (_, call) in std::mem::take(&mut self.calls) {
            steps.push(call_step(call, at_ms, None, &self.known));
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

    fn lines(reader: &mut ClaudeStepReader, events: &[(i64, Value)]) -> Vec<Step> {
        events
            .iter()
            .flat_map(|(at, event)| reader.line(&event.to_string(), *at))
            .collect()
    }

    fn assistant(id: &str, content: Value) -> Value {
        json!({"type": "assistant", "parent_tool_use_id": null,
               "message": {"id": id, "model": "claude-opus-5-5", "content": content,
                           "usage": {"input_tokens": 900, "output_tokens": 40}}})
    }

    fn result(id: &str, content: Value, is_error: bool) -> Value {
        json!({"type": "user", "parent_tool_use_id": null,
               "message": {"content": [{"type": "tool_result", "tool_use_id": id,
                                        "content": content, "is_error": is_error}]}})
    }

    /// The representative stream: a model's turn that calls `Bash`, its
    /// result, a subagent with a tool of its own, a wait on a background
    /// output, a retried API error and the last answer. Each is a step at
    /// its offset with what it ran, and none keeps what was said, a
    /// command's text or tokens.
    #[test]
    fn a_claude_stream_makes_the_steps_of_its_turn() {
        let mut reader = ClaudeStepReader::new(Some("session-model")).knowing(
            crate::domain::command_shape::Subcommands::common()
                .keyed(Some(crate::domain::command_shape::DigestKey::test())),
        );
        let command = "cargo test --locked --test it runtime_claim:: 2>&1 | tail -n 5";
        let mut steps = lines(
            &mut reader,
            &[
                (
                    0,
                    json!({"type": "system", "subtype": "init", "session_id": "s", "model": "claude-opus-5-5"}),
                ),
                (
                    100,
                    assistant(
                        "m1",
                        json!([{"type": "text", "text": "I will run the secret plan"}]),
                    ),
                ),
                (
                    300,
                    assistant(
                        "m1",
                        json!([{"type": "tool_use", "id": "t1", "name": "Bash",
                    "input": {"command": command, "description": "run the tests"}}]),
                    ),
                ),
                (
                    5_300,
                    result("t1", json!("test result: ok. 3 passed"), false),
                ),
                (
                    5_500,
                    assistant(
                        "m2",
                        json!([
                    {"type": "tool_use", "id": "t2", "name": "Agent", "input": {"prompt": "review the secret"}},
                    {"type": "tool_use", "id": "t3", "name": "Bash",
                     "input": {"command": "sleep 60", "run_in_background": true}}]),
                    ),
                ),
                (
                    5_600,
                    result(
                        "t3",
                        json!("Command running in background with ID: b1"),
                        false,
                    ),
                ),
                (
                    6_000,
                    json!({"type": "assistant", "parent_tool_use_id": "t2",
                    "message": {"id": "sub1", "model": "claude-haiku-5-5", "content": [
                        {"type": "tool_use", "id": "t4", "name": "Read", "input": {"file_path": "/secret"}}]}}),
                ),
                (
                    6_500,
                    json!({"type": "user", "parent_tool_use_id": "t2",
                    "message": {"content": [{"type": "tool_result", "tool_use_id": "t4", "content": [{"type": "text", "text": "abcd"}]}]}}),
                ),
                (
                    9_000,
                    result("t2", json!([{"type": "text", "text": "looks fine"}]), false),
                ),
                (
                    9_200,
                    assistant(
                        "m3",
                        json!([{"type": "tool_use", "id": "t5", "name": "BashOutput", "input": {"bash_id": "b1"}}]),
                    ),
                ),
                (12_000, result("t5", json!("Exit code 2\nboom"), true)),
                (
                    12_100,
                    json!({"type": "system", "subtype": "api_retry", "attempt": 1, "error_status": 529}),
                ),
                (
                    15_100,
                    assistant("m4", json!([{"type": "text", "text": "done"}])),
                ),
                (
                    15_200,
                    json!({"type": "result", "subtype": "success", "is_error": false,
                    "usage": {"input_tokens": 7}, "total_cost_usd": 0.5}),
                ),
            ],
        );
        steps.extend(reader.finish(16_000));
        // The session it named is the one a job's steps go as.
        assert_eq!(reader.session().as_deref(), Some("s"));
        let shape = |step: &Step| step.command_shape.clone();
        let mut expected = Vec::new();
        let model = |at: i64, until: i64| {
            let mut step = Step::new(0, StepKind::ModelTurn, at, until - at);
            step.model = Some("claude-opus-5-5".to_owned());
            step
        };
        expected.push(model(0, 300));
        let mut bash = Step::new(0, StepKind::ToolCall, 300, 5_000);
        bash.tool = Some("Bash".to_owned());
        bash.command_shape = Some(crate::domain::command_shape::shape_value(command));
        bash.exit_code = Some(0);
        bash.background = Some(false);
        bash.output_bytes = Some(25);
        expected.push(bash);
        expected.push(model(5_300, 5_500));
        let mut background = Step::new(0, StepKind::ToolCall, 5_500, 100);
        background.tool = Some("Bash".to_owned());
        background.command_shape = Some(crate::domain::command_shape::shape_value("sleep 60"));
        background.exit_code = Some(0);
        background.background = Some(true);
        background.output_bytes = Some(41);
        expected.push(background);
        let mut read = Step::new(0, StepKind::ToolCall, 6_000, 500);
        read.tool = Some("Read".to_owned());
        read.output_bytes = Some(4);
        expected.push(read);
        let mut subagent = Step::new(0, StepKind::Subagent, 5_500, 3_500);
        subagent.tool = Some("Agent".to_owned());
        subagent.output_bytes = Some(10);
        expected.push(subagent);
        expected.push(model(9_000, 9_200));
        let mut wait = Step::new(0, StepKind::Wait, 9_200, 2_800);
        wait.tool = Some("BashOutput".to_owned());
        wait.output_bytes = Some(16);
        expected.push(wait);
        expected.push(Step::new(0, StepKind::Wait, 12_100, 3_000));
        expected.push(model(12_000, 15_100));
        assert_eq!(steps, expected);
        assert_eq!(
            shape(&steps[1]).unwrap()["parts"][0]["args"][3],
            json!({"word": digest("it")})
        );
        // Nothing said, no command's text, no input or output and no
        // tokens.
        let text = serde_json::to_string(&steps).unwrap();
        for kept_out in [
            "secret",
            "plan",
            "review",
            "runtime_claim",
            "tail -n",
            "passed",
            "boom",
            "looks",
            "done\"",
            "usage",
            "token",
            "cost",
        ] {
            assert!(!text.contains(kept_out), "{kept_out} in {text}");
        }
    }

    /// A tool still running and a message still read when the turn ends
    /// are ended there; a call with no `Exit code` in its error has no
    /// exit.
    #[test]
    fn what_is_open_at_the_turns_end_is_ended_there() {
        let mut reader = ClaudeStepReader::new(None).knowing(
            crate::domain::command_shape::Subcommands::common()
                .keyed(Some(crate::domain::command_shape::DigestKey::test())),
        );
        let mut steps = lines(
            &mut reader,
            &[
                (0, json!({"type": "system", "subtype": "init"})),
                (
                    50,
                    assistant(
                        "m1",
                        json!([
                    {"type": "tool_use", "id": "a", "name": "Bash", "input": {"command": "false"}},
                    {"type": "tool_use", "id": "b", "name": "Bash", "input": {"command": "cargo build"}}]),
                    ),
                ),
                (80, result("a", json!("Error: interrupted"), true)),
            ],
        );
        steps.extend(reader.finish(1_000));
        let kinds: Vec<_> = steps
            .iter()
            .map(|s| (s.kind, s.offset_ms, s.duration_ms, s.exit_code))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (StepKind::ModelTurn, 0, 50, None),
                (StepKind::ToolCall, 50, 30, None),
                (StepKind::ToolCall, 50, 950, None),
            ]
        );
        // The model's name is its message's.
        assert_eq!(steps[0].model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(steps[2].output_bytes, None);
        assert_eq!(exit_code("Exit code 127\nnot found", true), Some(127));
    }
}
