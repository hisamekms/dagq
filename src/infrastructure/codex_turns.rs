//! The output of a headless Codex turn (`codex exec --json` and `codex exec
//! resume --json`, ADR-t813-1) read into the runtime's provider-neutral
//! [`TurnSignal`]s and [`TurnResult`]. The shapes are those measured with
//! codex-cli 0.155.1 (docs/plans/headless-worker-spike.md): `thread.started`
//! names the thread (the session the next turns resume), `turn.started`
//! opens the turn, `item.started` / `item.completed` carry its items
//! (`command_execution` with `command`, `exit_code` and `aggregated_output`,
//! `agent_message` with `text`, `error` with `message`), and the turn ends
//! with `turn.completed` (its `usage`) or `turn.failed` (its `error`); a
//! top-level `error` says an API error, retried or final. The kinds of
//! failure are not structured: they are read from the messages. Nothing is
//! written while a command runs, so a silence does not mean a stuck turn.
//! A command the project's rules refuse is not in the JSONL at all, only a
//! `Rejected(` line on stderr.

use serde_json::Value;

use crate::application::{Exit, TurnReader};
use crate::domain::tokens::TokenUsage;
use crate::domain::turn::{TurnFailure, TurnResult, TurnSignal, shortened};

/// How long a text of the agent's or a message is kept.
const TEXT_KEPT: usize = 400;

/// How long a refused command is kept in `permission_denials`.
const COMMAND_KEPT: usize = 120;

/// Reads one turn's JSONL.
#[derive(Debug, Default)]
pub struct CodexTurnReader {
    thread_id: Option<String>,
    /// The `turn.completed` event.
    completed: Option<Value>,
    /// The `turn.failed` error's message.
    failed: Option<String>,
    /// The last top-level or item error's message.
    last_error: Option<String>,
    /// An error said the login failed.
    authentication: Option<String>,
    /// An error said the usage limit or a rate limit was hit.
    usage_limit: Option<String>,
    /// The model did something: the thread has a conversation.
    answered: bool,
    last_text: Option<String>,
    /// The commands the sandbox refused.
    denials: Vec<String>,
}

/// What a message of Codex's says went wrong, by its words: `None` for one
/// that names no kind (a retry, a transport fallback).
pub fn classify(message: &str) -> Option<TurnFailure> {
    let lower = message.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|word| lower.contains(word));
    if any(&[
        "401 unauthorized",
        "auth error",
        "invalid_api_key",
        "incorrect api key",
        "missing bearer",
        "not logged in",
        "codex login",
        "token expired",
        "refresh token",
    ]) {
        return Some(TurnFailure::Authentication);
    }
    if any(&[
        "status 429",
        "429 too many requests",
        "too many requests",
        "rate limit",
        "rate_limit",
        "usage limit",
        "usage_limit",
        "insufficient_quota",
        "exceeded your current quota",
    ]) {
        return Some(TurnFailure::UsageLimit);
    }
    if lower.contains("model")
        && any(&[
            "not found",
            "not_found",
            "does not exist",
            "not supported",
            "unsupported",
            "invalid",
            "unknown",
        ])
    {
        return Some(TurnFailure::Model);
    }
    if any(&[
        "sandbox",
        "operation not permitted",
        "seatbelt",
        "cannot get process list",
        "rejected(",
    ]) {
        return Some(TurnFailure::Sandbox);
    }
    None
}

/// Whether a finished command was refused by the sandbox: it failed with
/// the operating system's refusal (a write outside the writable roots, a
/// signal or a process list outside the sandbox).
fn refused_by_sandbox(item: &Value) -> bool {
    let failed = item["exit_code"].as_i64().is_some_and(|code| code != 0);
    let output = item["aggregated_output"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    failed
        && (output.contains("operation not permitted")
            || output.contains("cannot get process list"))
}

impl CodexTurnReader {
    fn text(value: &Value) -> Option<String> {
        value
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(|text| shortened(text, TEXT_KEPT))
    }

    /// An error's `message`: kept, and the provider unusable when it says
    /// the login or a limit. A retry that Codex goes on with (`Reconnecting…
    /// n/5`) stops the turn only for the login, which a retry does not fix
    /// (a 401 is retried for about 20 seconds): a rate limit it may ride
    /// out, and only its final error holds the queue.
    fn error(&mut self, message: &str) -> Vec<TurnSignal> {
        let message = shortened(message, TEXT_KEPT);
        self.last_error = Some(message.clone());
        let retry = message.starts_with("Reconnecting");
        match classify(&message) {
            Some(TurnFailure::Authentication) => {
                self.authentication = Some(message.clone());
                vec![TurnSignal::Unusable(TurnFailure::Authentication, message)]
            }
            Some(TurnFailure::UsageLimit) if !retry => {
                self.usage_limit = Some(message.clone());
                vec![TurnSignal::Unusable(TurnFailure::UsageLimit, message)]
            }
            _ => vec![TurnSignal::Said(format!("error: {message}"))],
        }
    }

    fn started(item: &Value) -> Vec<TurnSignal> {
        match item["type"].as_str() {
            Some("command_execution") => vec![TurnSignal::Tool(format!(
                "shell {}",
                shortened(item["command"].as_str().unwrap_or(""), COMMAND_KEPT)
            ))],
            _ => Vec::new(),
        }
    }

    fn completed_item(&mut self, item: &Value) -> Vec<TurnSignal> {
        match item["type"].as_str() {
            Some("error") => self.error(item["message"].as_str().unwrap_or("error")),
            Some("agent_message") => {
                self.answered = true;
                match Self::text(&item["text"]) {
                    Some(text) => {
                        self.last_text = Some(text.clone());
                        vec![TurnSignal::Said(text)]
                    }
                    None => Vec::new(),
                }
            }
            Some("command_execution") => {
                self.answered = true;
                if refused_by_sandbox(item) {
                    self.denials.push(format!(
                        "sandbox: {}",
                        shortened(item["command"].as_str().unwrap_or(""), COMMAND_KEPT)
                    ));
                }
                Vec::new()
            }
            Some("file_change") => {
                self.answered = true;
                let paths: Vec<&str> = item["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|change| change["path"].as_str())
                    .collect();
                vec![TurnSignal::Tool(format!("edit {}", paths.join(" ")))]
            }
            Some(_) => {
                // Reasoning, a web search, a tool of a server: the model
                // answered.
                self.answered = true;
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// The last part of `stderr`, at most [`TEXT_KEPT`] bytes on a
    /// character boundary; `None` when it is empty.
    fn tail(stderr: &str) -> Option<String> {
        let tail = stderr.trim();
        (!tail.is_empty()).then(|| {
            let start = tail.len().saturating_sub(TEXT_KEPT);
            let start = (start..=tail.len())
                .find(|at| tail.is_char_boundary(*at))
                .unwrap_or(tail.len());
            tail[start..].to_owned()
        })
    }
}

impl TurnReader for CodexTurnReader {
    fn line(&mut self, line: &str) -> Vec<TurnSignal> {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        match event["type"].as_str() {
            Some("thread.started") => {
                if let Some(id) = event["thread_id"].as_str() {
                    self.thread_id = Some(id.to_owned());
                }
                vec![TurnSignal::Started {
                    session_id: self.thread_id.clone(),
                    model: None,
                    permission_mode: None,
                }]
            }
            Some("item.started") => Self::started(&event["item"]),
            Some("item.completed") => self.completed_item(&event["item"]),
            Some("turn.completed") => {
                self.completed = Some(event);
                Vec::new()
            }
            Some("turn.failed") => {
                let message = event["error"]["message"]
                    .as_str()
                    .unwrap_or("the turn failed")
                    .to_owned();
                self.failed = Some(shortened(&message, TEXT_KEPT));
                self.error(&message)
            }
            Some("error") => self.error(event["message"].as_str().unwrap_or("error")),
            _ => Vec::new(),
        }
    }

    fn heartbeats(&self) -> bool {
        false
    }

    fn finish(&mut self, exit: Option<&Exit>, stderr: &str) -> TurnResult {
        // A command the project's rules refused is only on stderr.
        for line in stderr.lines().filter(|line| line.contains("Rejected(")) {
            self.denials
                .push(format!("rules: {}", shortened(line, COMMAND_KEPT)));
        }
        let exited_well = exit.is_some_and(|exit| exit.success);
        let is_error = self.completed.is_none()
            || self.failed.is_some()
            || self.authentication.is_some()
            || self.usage_limit.is_some()
            || !exited_well;
        let message = if is_error {
            self.failed
                .clone()
                .or_else(|| self.authentication.clone())
                .or_else(|| self.usage_limit.clone())
                .or_else(|| self.last_error.clone())
                .or_else(|| Self::tail(stderr))
                .or_else(|| self.last_text.clone())
        } else {
            self.last_text.clone()
        };
        let failure = is_error.then(|| {
            if self.authentication.is_some() {
                TurnFailure::Authentication
            } else if self.usage_limit.is_some() {
                TurnFailure::UsageLimit
            } else {
                message
                    .as_deref()
                    .and_then(classify)
                    .unwrap_or(TurnFailure::Other)
            }
        });
        TurnResult {
            result_seen: self.completed.is_some() || self.failed.is_some(),
            is_error,
            failure,
            message,
            session_id: self.thread_id.clone(),
            session_created: self.answered,
            num_turns: None,
            duration_ms: None,
            cost_usd: None,
            usage: self
                .completed
                .as_ref()
                .map_or(Value::Null, |event| event["usage"].clone()),
            tokens: self
                .completed
                .as_ref()
                .and_then(|event| turn_tokens(&event["usage"])),
            // `turn.completed` carries the thread's total so far, a
            // resumed thread's earlier turns included.
            tokens_cumulative: true,
            permission_denials: std::mem::take(&mut self.denials),
            session_missing: stderr.contains("no rollout found"),
        }
    }
}

/// The tokens of the thread so far from the `usage` of a `turn.completed`
/// (the thread's running total, not the turn's own), in the kinds of
/// Claude's: `input_tokens` counts the cached input too, so
/// `input` is the rest and `cache_read` is `cached_input_tokens`;
/// `cache_creation` is `cache_write_input_tokens`; `output` is
/// `output_tokens`, which `reasoning_output_tokens` is a part of. Codex
/// gives no cost. `None` when the input and output counts are not numbers.
fn turn_tokens(usage: &Value) -> Option<TokenUsage> {
    let count = |key: &str| usage[key].as_i64();
    let cache_read = count("cached_input_tokens").unwrap_or(0);
    Some(TokenUsage {
        input: (count("input_tokens")? - cache_read).max(0),
        output: count("output_tokens")?,
        cache_read,
        cache_creation: count("cache_write_input_tokens").unwrap_or(0),
        messages: 1,
        cost_usd: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn exit(code: i32) -> Exit {
        Exit {
            success: code == 0,
            code: Some(code),
            signal: None,
            description: format!("exit status: {code}"),
        }
    }

    fn read(lines: &[Value]) -> (CodexTurnReader, Vec<TurnSignal>) {
        let mut reader = CodexTurnReader::default();
        let mut signals = reader.line("Reading additional input from stdin...");
        for line in lines {
            signals.extend(reader.line(&line.to_string()));
        }
        (reader, signals)
    }

    #[test]
    fn a_turn_that_completed_names_its_thread_and_usage() {
        let usage = json!({"input_tokens": 30, "cached_input_tokens": 20, "output_tokens": 5, "reasoning_output_tokens": 2});
        let (mut reader, signals) = read(&[
            json!({"type": "thread.started", "thread_id": "th-1"}),
            json!({"type": "turn.started"}),
            json!({"type": "item.completed", "item": {"id": "i0", "type": "reasoning", "text": "thinking"}}),
            json!({"type": "item.started", "item": {"id": "i1", "type": "command_execution", "command": "cargo test", "status": "in_progress"}}),
            json!({"type": "item.completed", "item": {"id": "i1", "type": "command_execution", "command": "cargo test", "exit_code": 0, "aggregated_output": "ok", "status": "completed"}}),
            json!({"type": "item.completed", "item": {"id": "i2", "type": "file_change", "changes": [{"path": "a.rs", "kind": "update"}]}}),
            json!({"type": "item.completed", "item": {"id": "i3", "type": "agent_message", "text": "done"}}),
            json!({"type": "turn.completed", "usage": usage}),
        ]);
        assert_eq!(
            signals,
            [
                TurnSignal::Started {
                    session_id: Some("th-1".into()),
                    model: None,
                    permission_mode: None,
                },
                TurnSignal::Tool("shell cargo test".into()),
                TurnSignal::Tool("edit a.rs".into()),
                TurnSignal::Said("done".into()),
            ]
        );
        assert!(!reader.heartbeats());
        let result = reader.finish(Some(&exit(0)), "");
        assert!(result.result_seen);
        assert!(!result.is_error);
        assert_eq!(result.failure, None);
        assert_eq!(result.message.as_deref(), Some("done"));
        assert_eq!(result.session_id.as_deref(), Some("th-1"));
        assert!(result.session_created);
        assert_eq!(result.usage, usage);
        assert_eq!(
            result.tokens.map(|tokens| tokens.payload()),
            Some(
                json!({"input": 10, "output": 5, "cache_read": 20, "cache_creation": 0, "messages": 1})
            )
        );
        assert!(result.permission_denials.is_empty());
    }

    #[test]
    fn a_login_that_failed_is_unusable_at_its_first_error() {
        let (mut reader, signals) = read(&[
            json!({"type": "thread.started", "thread_id": "th-2"}),
            json!({"type": "error", "message": "Reconnecting... 2/5 (unexpected status 401 Unauthorized: Missing bearer or basic authentication in header)"}),
        ]);
        assert!(matches!(
            signals.last(),
            Some(TurnSignal::Unusable(TurnFailure::Authentication, _))
        ));
        let result = reader.finish(None, "");
        assert!(result.is_error);
        assert_eq!(result.failure, Some(TurnFailure::Authentication));
        assert!(!result.session_created);
        assert_eq!(result.session_id.as_deref(), Some("th-2"));
    }

    #[test]
    fn failures_are_classified_by_their_messages() {
        for (message, failure) in [
            (
                "unexpected status 429 Too Many Requests: You've hit your usage limit. Try again at 3pm.",
                Some(TurnFailure::UsageLimit),
            ),
            (
                "unexpected status 401 Unauthorized: Incorrect API key provided, auth error code: invalid_api_key",
                Some(TurnFailure::Authentication),
            ),
            (
                "unexpected status 400 Bad Request: The 'gpt-x' model is not supported",
                Some(TurnFailure::Model),
            ),
            (
                "exec_command failed: sandbox denied the write",
                Some(TurnFailure::Sandbox),
            ),
            ("stream disconnected before completion", None),
            ("unexpected status 500: request id: req_429ab", None),
        ] {
            assert_eq!(classify(message), failure, "{message}");
        }
        // The last error of a turn that failed names its kind.
        let (mut reader, signals) = read(&[
            json!({"type": "thread.started", "thread_id": "th-3"}),
            json!({"type": "error", "message": "Reconnecting... 1/5 (stream disconnected)"}),
            json!({"type": "turn.failed", "error": {"message": "unexpected status 400 Bad Request: model 'nope' does not exist"}}),
        ]);
        assert!(
            signals
                .iter()
                .all(|signal| !matches!(signal, TurnSignal::Unusable(..)))
        );
        let result = reader.finish(Some(&exit(1)), "");
        assert!(result.result_seen);
        assert_eq!(result.failure, Some(TurnFailure::Model));
        assert!(result.message.unwrap().contains("does not exist"));
        // A rate limit Codex retries does not stop the turn; its final
        // error does.
        let (_, signals) = read(&[
            json!({"type": "error", "message": "Reconnecting... 2/5 (unexpected status 429 Too Many Requests: rate limit)"}),
        ]);
        assert!(
            signals
                .iter()
                .all(|signal| !matches!(signal, TurnSignal::Unusable(..)))
        );
        // A limit in `turn.failed` is unusable too.
        let (mut reader, signals) =
            read(&[json!({"type": "turn.failed", "error": {"message": "rate limit reached"}})]);
        assert!(matches!(
            signals.last(),
            Some(TurnSignal::Unusable(TurnFailure::UsageLimit, _))
        ));
        assert_eq!(
            reader.finish(Some(&exit(1)), "").failure,
            Some(TurnFailure::UsageLimit)
        );
    }

    #[test]
    fn a_turn_without_its_end_fails_on_what_it_left() {
        // Killed or exited without turn.completed: stderr says why.
        let (mut reader, _) = read(&[json!({"type": "thread.started", "thread_id": "th-4"})]);
        let result = reader.finish(
            Some(&exit(1)),
            "Error: thread/resume failed: no rollout found for thread id th-4\n",
        );
        assert!(result.is_error);
        assert!(!result.result_seen);
        assert_eq!(result.failure, Some(TurnFailure::Other));
        assert!(result.message.unwrap().contains("no rollout found"));
        assert!(result.session_missing);
        // A completed turn whose process failed is an error too.
        let (mut reader, _) = read(&[json!({"type": "turn.completed", "usage": {}})]);
        let result = reader.finish(Some(&exit(2)), "");
        assert!(result.is_error);
        assert_eq!(result.failure, Some(TurnFailure::Other));
        // A usage without counts has no tokens.
        assert_eq!(result.tokens, None);
        // An item error that names no limit is only said.
        let mut reader = CodexTurnReader::default();
        assert_eq!(
            reader.line(&json!({"type": "item.completed", "item": {"type": "error", "message": "Falling back from WebSockets"}}).to_string()),
            [TurnSignal::Said("error: Falling back from WebSockets".into())]
        );
        assert!(
            reader
                .line(&json!({"type": "item.completed", "item": {}}).to_string())
                .is_empty()
        );
        assert!(
            reader
                .line(&json!({"type": "turn.started"}).to_string())
                .is_empty()
        );
        assert!(
            reader
                .line(&json!({"type": "item.started", "item": {"type": "reasoning"}}).to_string())
                .is_empty()
        );
        assert!(reader.line(&json!({"type": "item.completed", "item": {"type": "agent_message", "text": " "}}).to_string()).is_empty());
        let result = reader.finish(None, "");
        assert_eq!(
            result.message.as_deref(),
            Some("Falling back from WebSockets")
        );
    }

    #[test]
    fn refusals_of_the_sandbox_and_the_rules_are_denials() {
        let (mut reader, _) = read(&[
            json!({"type": "item.completed", "item": {"type": "command_execution", "command": "pkill -f cargo", "exit_code": 3, "aggregated_output": "pkill: Cannot get process list"}}),
            json!({"type": "item.completed", "item": {"type": "command_execution", "command": "touch ~/x", "exit_code": 1, "aggregated_output": "touch: /Users/u/x: Operation not permitted"}}),
            json!({"type": "item.completed", "item": {"type": "command_execution", "command": "false", "exit_code": 1, "aggregated_output": ""}}),
            json!({"type": "item.completed", "item": {"type": "agent_message", "text": "blocked"}}),
            json!({"type": "turn.completed", "usage": {"input_tokens": 1}}),
        ]);
        let result = reader.finish(
            Some(&exit(0)),
            "ERROR codex_core::tools::router: error=exec_command failed: CreateProcess { message: \"Rejected(\\\"`/bin/zsh -lc 'killall x'` rejected: dagq\\\")\" }\nother\n",
        );
        assert!(!result.is_error);
        assert_eq!(
            result.permission_denials.len(),
            3,
            "{:?}",
            result.permission_denials
        );
        assert_eq!(result.permission_denials[0], "sandbox: pkill -f cargo");
        assert!(result.permission_denials[2].starts_with("rules: "));
    }
}
