//! The output of a headless Claude Code turn (`claude -p --output-format
//! stream-json --verbose`, ADR-t813-1) read into the runtime's
//! provider-neutral [`TurnSignal`]s and [`TurnResult`]. The shapes are
//! those measured with Claude Code 2.1.283 (docs/plans/headless-worker-spike.md):
//! `system/init` names the session, model and permission mode; `assistant`
//! and `user` carry the conversation; `system/api_retry` a retried API
//! error; `rate_limit_event` the usage window; `result` the turn's end,
//! with `is_error`, `num_turns`, `duration_ms`, `total_cost_usd`, `usage`
//! and `permission_denials`. While a tool runs the stream carries a
//! heartbeat (`tool_progress`) every 30 seconds.

use serde_json::Value;

use crate::application::{Exit, TurnReader};
use crate::domain::queue_hold::Wall;
use crate::domain::tokens::TokenUsage;
use crate::domain::turn::{TurnFailure, TurnResult, TurnSignal, shortened};
use crate::infrastructure::claude::job_wall;

/// The permission mode a headless worker's turn is started in: tools that
/// need a person's permission are refused rather than waited on, and the
/// refusals are listed in the result's `permission_denials`.
pub const HEADLESS_PERMISSION_MODE: &str = "auto";

/// How long a text of the agent's or a message is kept.
const TEXT_KEPT: usize = 400;

/// Reads one turn's stream.
#[derive(Debug, Default)]
pub struct ClaudeTurnReader {
    session_id: Option<String>,
    /// The model `system/init` named.
    model: Option<String>,
    /// The result event, the last of the stream.
    result: Option<Value>,
    /// The stream said the login failed (a retried 401, or the agent's
    /// synthetic `authentication_failed`).
    authentication: Option<String>,
    /// The stream said the usage limit was hit.
    usage_limit: Option<String>,
    /// The error of an assistant message the CLI made up itself.
    assistant_error: Option<String>,
    /// A real answer of the model: the session has a conversation.
    answered: bool,
    last_text: Option<String>,
}

impl ClaudeTurnReader {
    fn text(value: &Value) -> Option<String> {
        value.as_str().map(|text| shortened(text, TEXT_KEPT))
    }

    fn system(&mut self, event: &Value) -> Vec<TurnSignal> {
        match event["subtype"].as_str() {
            Some("init") => {
                if let Some(id) = event["session_id"].as_str() {
                    self.session_id = Some(id.to_owned());
                }
                if let Some(model) = event["model"].as_str() {
                    self.model = Some(model.to_owned());
                }
                vec![TurnSignal::Started {
                    session_id: self.session_id.clone(),
                    model: self.model.clone(),
                    permission_mode: event["permissionMode"].as_str().map(str::to_owned),
                }]
            }
            // A 401 is retried for minutes before the turn fails: the
            // first retry says it already.
            Some("api_retry")
                if event["error"] == "authentication_failed" || event["error_status"] == 401 =>
            {
                let message = format!(
                    "the API refused the login (status {}, {})",
                    event["error_status"],
                    event["error"].as_str().unwrap_or("authentication_failed")
                );
                self.authentication = Some(message.clone());
                vec![TurnSignal::Unusable(TurnFailure::Authentication, message)]
            }
            _ => Vec::new(),
        }
    }

    fn assistant(&mut self, event: &Value) -> Vec<TurnSignal> {
        let message = &event["message"];
        if let Some(error) = event["error"].as_str() {
            self.assistant_error = Some(error.to_owned());
            let text = message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|item| Self::text(&item["text"]))
                .unwrap_or_else(|| error.to_owned());
            if error == "authentication_failed" {
                self.authentication = Some(text.clone());
                return vec![TurnSignal::Unusable(TurnFailure::Authentication, text)];
            }
            if matches!(error, "rate_limit" | "billing_error") {
                self.usage_limit = Some(text.clone());
                return vec![TurnSignal::Unusable(TurnFailure::UsageLimit, text)];
            }
            return vec![TurnSignal::Said(text)];
        }
        if message["model"] != "<synthetic>" {
            self.answered = true;
        }
        let mut signals = Vec::new();
        for item in message["content"].as_array().into_iter().flatten() {
            match item["type"].as_str() {
                Some("text") => {
                    if let Some(text) = Self::text(&item["text"]) {
                        self.last_text = Some(text.clone());
                        signals.push(TurnSignal::Said(text));
                    }
                }
                Some("tool_use") => {
                    let name = item["name"].as_str().unwrap_or("tool");
                    let input = &item["input"];
                    let detail = ["command", "file_path", "pattern", "description"]
                        .iter()
                        .find_map(|key| input[*key].as_str())
                        .map(|detail| format!(" {}", shortened(detail, 120)))
                        .unwrap_or_default();
                    signals.push(TurnSignal::Tool(format!("{name}{detail}")));
                }
                _ => (),
            }
        }
        signals
    }

    /// Why a turn that failed failed. Past the stream's own signs, the
    /// result's text and stderr are read for a login or a usage limit as
    /// every Claude job's output is (`claude::job_wall`, task 438), so the
    /// worker and the jobs tell them by the same words.
    fn failure(&self, result: Option<&Value>, message: &str, stderr: &str) -> TurnFailure {
        let status = result.and_then(|r| r["api_error_status"].as_i64());
        if self.authentication.is_some() || status == Some(401) {
            return TurnFailure::Authentication;
        }
        if self.usage_limit.is_some() || status == Some(429) {
            return TurnFailure::UsageLimit;
        }
        match job_wall(&format!("{message}\n{stderr}")) {
            Some(Wall::Authentication) => return TurnFailure::Authentication,
            Some(Wall::UsageLimit) => return TurnFailure::UsageLimit,
            None => (),
        }
        let lower = message.to_ascii_lowercase();
        if status == Some(404)
            || (lower.contains("model")
                && [
                    "not found",
                    "not_found",
                    "does not exist",
                    "invalid",
                    "unknown",
                ]
                .iter()
                .any(|what| lower.contains(what)))
        {
            return TurnFailure::Model;
        }
        TurnFailure::Other
    }
}

/// Whether a `rate_limit_event` stops the turn: the condition Claude Code
/// (2.1.285) itself uses. A `rejected` window paid for by overage comes with
/// `isUsingOverage` or `overageInUse` true, and the turn goes on.
fn usage_limit_hit(info: &Value) -> bool {
    info["status"] == "rejected" && info["isUsingOverage"] != true && info["overageInUse"] != true
}

impl TurnReader for ClaudeTurnReader {
    fn line(&mut self, line: &str) -> Vec<TurnSignal> {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        match event["type"].as_str() {
            Some("system") => self.system(&event),
            Some("assistant") => self.assistant(&event),
            Some("rate_limit_event") if usage_limit_hit(&event["rate_limit_info"]) => {
                let info = &event["rate_limit_info"];
                let message = format!(
                    "the {} usage limit was hit (resets at {})",
                    info["rateLimitType"].as_str().unwrap_or("usage"),
                    info["resetsAt"]
                );
                self.usage_limit = Some(message.clone());
                vec![TurnSignal::Unusable(TurnFailure::UsageLimit, message)]
            }
            Some("result") => {
                if let Some(id) = event["session_id"].as_str() {
                    self.session_id = Some(id.to_owned());
                }
                self.result = Some(event);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn heartbeats(&self) -> bool {
        true
    }

    fn finish(&mut self, exit: Option<&Exit>, stderr: &str) -> TurnResult {
        let result = self.result.as_ref();
        let exited_well = exit.is_some_and(|exit| exit.success);
        let is_error = result.is_none_or(|r| r["is_error"].as_bool().unwrap_or(true))
            || self.authentication.is_some()
            || self.usage_limit.is_some()
            || !exited_well;
        let message = result
            .and_then(|r| {
                Self::text(&r["result"]).or_else(|| {
                    r["errors"].as_array().map(|errors| {
                        shortened(
                            &errors
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("; "),
                            TEXT_KEPT,
                        )
                    })
                })
            })
            .or_else(|| self.authentication.clone())
            .or_else(|| self.usage_limit.clone())
            .or_else(|| {
                let tail = stderr.trim();
                (!tail.is_empty()).then(|| {
                    let start = tail.len().saturating_sub(TEXT_KEPT);
                    let start = (start..=tail.len())
                        .find(|at| tail.is_char_boundary(*at))
                        .unwrap_or(tail.len());
                    tail[start..].to_owned()
                })
            })
            .or_else(|| self.last_text.clone());
        let failure =
            is_error.then(|| self.failure(result, message.as_deref().unwrap_or(""), stderr));
        TurnResult {
            result_seen: result.is_some(),
            is_error,
            failure,
            message,
            session_id: self.session_id.clone(),
            session_created: self.answered,
            num_turns: result.and_then(|r| r["num_turns"].as_u64()),
            duration_ms: result.and_then(|r| r["duration_ms"].as_u64()),
            cost_usd: result.and_then(|r| r["total_cost_usd"].as_f64()),
            usage: result.map_or(Value::Null, |r| r["usage"].clone()),
            permission_denials: result
                .and_then(|r| r["permission_denials"].as_array())
                .into_iter()
                .flatten()
                .map(|denial| denial["tool_name"].as_str().unwrap_or("unknown").to_owned())
                .collect(),
            tokens: result.and_then(|r| turn_tokens(&r["usage"], r["total_cost_usd"].as_f64())),
            tokens_cumulative: false,
            // Claude's session is the run's: a missing one is started
            // by `turn_session_exists` instead.
            session_missing: false,
            model: self.model.clone(),
            model_unknown: self
                .model
                .is_none()
                .then(|| "the stream named no model (no system/init)".to_owned()),
        }
    }
}

/// The tokens of a turn from the `usage` of its `result` (every call of
/// the turn together) and its `total_cost_usd`: the same kinds as a
/// transcript's message (`input_tokens` without the cache,
/// `cache_read_input_tokens`, `cache_creation_input_tokens`,
/// `output_tokens`); `None` when the input and output counts are not
/// numbers.
fn turn_tokens(usage: &Value, cost_usd: Option<f64>) -> Option<TokenUsage> {
    let count = |key: &str| usage[key].as_i64();
    Some(TokenUsage {
        input: count("input_tokens")?,
        output: count("output_tokens")?,
        cache_read: count("cache_read_input_tokens").unwrap_or(0),
        cache_creation: count("cache_creation_input_tokens").unwrap_or(0),
        messages: 1,
        cost_usd,
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

    fn read(lines: &[Value]) -> (ClaudeTurnReader, Vec<TurnSignal>) {
        let mut reader = ClaudeTurnReader::default();
        let mut signals = reader.line("not json");
        for line in lines {
            signals.extend(reader.line(&line.to_string()));
        }
        (reader, signals)
    }

    fn init(mode: &str) -> Value {
        json!({"type": "system", "subtype": "init", "session_id": "s1", "model": "claude-sonnet-5", "permissionMode": mode})
    }

    #[test]
    fn a_turn_that_succeeded_is_read_with_its_usage_and_denials() {
        let (mut reader, signals) = read(&[
            init("auto"),
            json!({"type": "assistant", "message": {"model": "claude-sonnet-5", "content": [
                {"type": "text", "text": "Working on it."},
                {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}},
                {"type": "tool_use", "name": "Glob", "input": {}}
            ]}}),
            json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed"}}),
            json!({"type": "system", "subtype": "task_started"}),
            json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 3,
                "duration_ms": 1200, "total_cost_usd": 0.04, "session_id": "s1",
                "result": "Done.", "usage": {"input_tokens": 10, "output_tokens": 5},
                "permission_denials": [{"tool_name": "Bash", "tool_use_id": "t1", "tool_input": {}}]}),
        ]);
        assert!(reader.heartbeats());
        assert_eq!(
            signals,
            [
                TurnSignal::Started {
                    session_id: Some("s1".into()),
                    model: Some("claude-sonnet-5".into()),
                    permission_mode: Some("auto".into()),
                },
                TurnSignal::Said("Working on it.".into()),
                TurnSignal::Tool("Bash cargo test".into()),
                TurnSignal::Tool("Glob".into()),
            ]
        );
        let result = reader.finish(Some(&exit(0)), "");
        assert!(result.result_seen && !result.is_error && result.session_created);
        assert_eq!(result.failure, None);
        assert_eq!(result.message.as_deref(), Some("Done."));
        assert_eq!(result.session_id.as_deref(), Some("s1"));
        assert_eq!(result.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(result.model_unknown, None);
        assert_eq!(
            (result.num_turns, result.duration_ms, result.cost_usd),
            (Some(3), Some(1200), Some(0.04))
        );
        assert_eq!(result.usage["output_tokens"], 5);
        assert_eq!(
            result.tokens.map(|tokens| tokens.payload()),
            Some(
                json!({"input": 10, "output": 5, "cache_read": 0, "cache_creation": 0,
                        "messages": 1, "cost_usd": 0.04})
            )
        );
        assert_eq!(result.permission_denials, ["Bash"]);
    }

    #[test]
    fn a_login_that_ran_out_is_said_at_once() {
        // Not logged in: the CLI makes the answer up.
        let (mut reader, signals) = read(&[
            init("auto"),
            json!({"type": "assistant", "error": "authentication_failed", "message": {"model": "<synthetic>", "content": [{"type": "text", "text": "Not logged in · Please run /login"}]}}),
            json!({"type": "result", "subtype": "success", "is_error": true, "api_error_status": null, "result": "Not logged in · Please run /login", "num_turns": 1}),
        ]);
        assert!(signals.contains(&TurnSignal::Unusable(
            TurnFailure::Authentication,
            "Not logged in · Please run /login".into()
        )));
        let result = reader.finish(Some(&exit(1)), "");
        assert_eq!(result.failure, Some(TurnFailure::Authentication));
        assert!(!result.session_created);
        // An invalid key: the first retry of the 401 says it.
        let (mut reader, signals) = read(&[
            init("auto"),
            json!({"type": "system", "subtype": "api_retry", "attempt": 1, "error_status": 401, "error": "authentication_failed"}),
        ]);
        assert!(matches!(
            signals.last(),
            Some(TurnSignal::Unusable(TurnFailure::Authentication, _))
        ));
        // Stopped before its result.
        let result = reader.finish(None, "");
        assert!(result.is_error && !result.result_seen);
        assert_eq!(result.failure, Some(TurnFailure::Authentication));
        assert!(result.message.unwrap().contains("401"));
        // No init, no model: why is said.
        let result = ClaudeTurnReader::default().finish(None, "");
        assert_eq!(result.model, None);
        assert!(result.model_unknown.unwrap().contains("no model"));
    }

    #[test]
    fn a_rejected_window_paid_for_by_overage_does_not_stop_the_turn() {
        for info in [
            json!({"status": "rejected", "rateLimitType": "five_hour", "isUsingOverage": true, "overageStatus": "allowed"}),
            json!({"status": "rejected", "rateLimitType": "five_hour", "overageInUse": true}),
            json!({"status": "allowed_warning", "rateLimitType": "seven_day", "isUsingOverage": false}),
        ] {
            let (mut reader, signals) = read(&[
                init("auto"),
                json!({"type": "rate_limit_event", "rate_limit_info": info}),
                json!({"type": "result", "is_error": false, "session_id": "s"}),
            ]);
            assert!(
                !signals
                    .iter()
                    .any(|signal| matches!(signal, TurnSignal::Unusable(..))),
                "{info}: {signals:?}"
            );
            let result = reader.finish(Some(&exit(0)), "");
            assert!(!result.is_error, "{info}");
            assert_eq!(result.failure, None, "{info}");
        }
        let (mut reader, signals) = read(&[
            init("auto"),
            json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "seven_day", "isUsingOverage": false, "overageInUse": false, "overageStatus": "rejected", "overageDisabledReason": "out_of_credits"}}),
        ]);
        assert!(matches!(
            signals.last(),
            Some(TurnSignal::Unusable(TurnFailure::UsageLimit, message)) if message.contains("seven_day")
        ));
        assert_eq!(
            reader.finish(None, "").failure,
            Some(TurnFailure::UsageLimit)
        );
    }

    #[test]
    fn a_usage_limit_and_a_model_are_told_apart_from_other_failures() {
        let (mut reader, signals) = read(&[
            init("auto"),
            json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour", "resetsAt": 1790535600}}),
        ]);
        assert!(matches!(
            signals.last(),
            Some(TurnSignal::Unusable(TurnFailure::UsageLimit, message)) if message.contains("five_hour")
        ));
        assert_eq!(
            reader.finish(None, "").failure,
            Some(TurnFailure::UsageLimit)
        );
        let (mut reader, _) =
            read(&[json!({"type": "result", "is_error": true, "api_error_status": 429})]);
        assert_eq!(
            reader.finish(Some(&exit(1)), "").failure,
            Some(TurnFailure::UsageLimit)
        );
        let (mut reader, _) = read(&[
            json!({"type": "result", "is_error": true, "api_error_status": 404, "result": "API Error: 404"}),
        ]);
        assert_eq!(
            reader.finish(Some(&exit(1)), "").failure,
            Some(TurnFailure::Model)
        );
        let (mut reader, _) = read(&[
            json!({"type": "assistant", "error": "rate_limit", "message": {"model": "<synthetic>", "content": []}}),
        ]);
        assert_eq!(
            reader.finish(Some(&exit(1)), "").failure,
            Some(TurnFailure::UsageLimit)
        );
        let (mut reader, signals) = read(&[
            json!({"type": "assistant", "error": "invalid_request", "message": {"model": "<synthetic>", "content": []}}),
        ]);
        assert_eq!(signals, [TurnSignal::Said("invalid_request".into())]);
        let result = reader.finish(Some(&exit(1)), "error: something broke\n");
        assert_eq!(result.failure, Some(TurnFailure::Other));
        assert_eq!(result.message.as_deref(), Some("error: something broke"));
        // No result, a clean exit: still a failure, with the last text.
        let (mut reader, _) = read(&[
            json!({"type": "assistant", "message": {"model": "m", "content": [{"type": "text", "text": "halfway"}]}}),
        ]);
        let result = reader.finish(Some(&exit(0)), "");
        assert!(result.is_error);
        assert_eq!(result.message.as_deref(), Some("halfway"));
        // Claude Code's words for the usage limit or a login, in the
        // result or on stderr, as a job's output is read (task 438).
        let (mut reader, _) = read(&[
            json!({"type": "result", "is_error": true, "result": "Claude AI usage limit reached|1790535600"}),
        ]);
        assert_eq!(
            reader.finish(Some(&exit(1)), "").failure,
            Some(TurnFailure::UsageLimit)
        );
        let (mut reader, _) = read(&[init("auto")]);
        assert_eq!(
            reader
                .finish(Some(&exit(1)), "Invalid API key · Please run /login\n")
                .failure,
            Some(TurnFailure::Authentication)
        );
        // Failed errors of a result are joined.
        let (mut reader, _) = read(&[
            json!({"type": "result", "subtype": "error_max_turns", "is_error": true, "errors": ["Reached maximum number of turns (1)"]}),
        ]);
        let result = reader.finish(Some(&exit(1)), "");
        assert_eq!(
            result.message.as_deref(),
            Some("Reached maximum number of turns (1)")
        );
    }
}
