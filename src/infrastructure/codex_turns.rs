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
//! `Rejected(` line on stderr. The JSONL does not name the model either:
//! it is read from the thread's rollout, which Codex writes under its home
//! (`sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`), whose
//! `turn_context` records carry the `model` of each turn. The tokens of a
//! turn are counted from the rollouts too ([`rollout_usage`]): the
//! `token_usage_record` of each response, a child thread's (in a rollout of
//! its own) included, which `turn.completed`'s thread total leaves out
//! (docs/design/execution-tokens.md). The rollouts are only read.

use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::application::{Exit, TurnReader};
use crate::domain::tokens::{
    ExecutionContext, ROLLOUT_MISSING, ROLLOUT_TURN_MISSING, ROLLOUT_UNREADABLE, RolloutUsage,
    TOKEN_USAGE_RECORD_MISSING, TokenSource, TokenUsage, TurnContext, UsageRecord,
};
use crate::domain::turn::{TurnCommand, TurnFailure, TurnResult, TurnSignal, shortened};
use crate::domain::verify_failure::failed_tests;
use crate::domain::worktime::SHELL_ITEM;

/// How long a text of the agent's or a message is kept.
const TEXT_KEPT: usize = 400;

/// How long a refused command is kept in `permission_denials`.
const COMMAND_KEPT: usize = 120;

/// How many of the latest day directories of `sessions` a thread's rollout
/// is looked for in: a resumed thread's rollout stays in the directory of
/// the day it started.
const ROLLOUT_DAYS: usize = 31;

/// How much earlier than the reader was made a `turn_context` may be
/// stamped and still be the turn's: the rollout's times are Codex's clock
/// on the same host, at millisecond (a stub's at second) precision.
const TURN_CONTEXT_SKEW_MILLIS: i64 = 2_000;

/// Reads one turn's JSONL.
#[derive(Debug, Default)]
pub struct CodexTurnReader {
    /// Codex's `sessions` directory, where the thread's rollout names the
    /// model; `None` when Codex's home is not known.
    sessions: Option<PathBuf>,
    /// When the turn started (unix milliseconds): a `turn_context` stamped
    /// before it is an earlier turn's. `None`: any is taken.
    since: Option<i64>,
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
    /// When the lines read now were read ([`TurnReader::stamp`]).
    at: Option<i64>,
    /// The commands and tools the turn ran, in the order they started.
    commands: Vec<TurnCommand>,
}

/// The items of a turn that run something whose time the work breakdown
/// counts: a shell command, and a tool of a server.
const TIMED_ITEMS: [&str; 2] = [SHELL_ITEM, "mcp_tool_call"];

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

/// The [`ROLLOUT_DAYS`] latest day directories of `sessions`
/// (`YYYY/MM/DD`), the latest first.
fn day_dirs(sessions: &Path) -> Vec<PathBuf> {
    let children = |dir: &Path| -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort_unstable_by(|a, b| b.cmp(a));
        dirs
    };
    children(sessions)
        .iter()
        .flat_map(|year| children(year))
        .flat_map(|month| children(&month))
        .take(ROLLOUT_DAYS)
        .collect()
}

/// The rollout of `thread` among the [`day_dirs`] of `sessions`, the
/// latest first.
fn rollout(sessions: &Path, thread: &str) -> Option<PathBuf> {
    let suffix = format!("-{thread}.jsonl");
    day_dirs(sessions).into_iter().find_map(|day| {
        fs::read_dir(&day)
            .ok()?
            .filter_map(|entry| Some(entry.ok()?.path()))
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
            })
    })
}

/// The model the last turn of `thread` ran on: the `model` of the last
/// `turn_context` of its rollout under `sessions`, among those stamped at
/// `since` (unix milliseconds, less [`TURN_CONTEXT_SKEW_MILLIS`]) or later
/// when it is given, so that a turn that failed before Codex wrote its own
/// is not given an earlier turn's; else why none was read.
pub fn rollout_model(sessions: &Path, thread: &str, since: Option<i64>) -> Result<String, String> {
    let path = rollout(sessions, thread)
        .ok_or_else(|| format!("no rollout of thread {thread} under {}", sessions.display()))?;
    let file =
        fs::File::open(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| line.contains("\"turn_context\""))
        .filter_map(|line| {
            let record: Value = serde_json::from_str(&line).ok()?;
            (record["type"] == "turn_context" && stamped_since(&record["timestamp"], since))
                .then(|| record["payload"]["model"].as_str().map(str::to_owned))?
        })
        .last()
        .ok_or_else(|| match since {
            Some(_) => format!(
                "the rollout {} names no model for this turn",
                path.display()
            ),
            None => format!("the rollout {} names no model", path.display()),
        })
}

/// Whether a record stamped `stamp` (an RFC 3339 time) is of a turn that
/// started at `since` (unix milliseconds, less
/// [`TURN_CONTEXT_SKEW_MILLIS`]) or later; any is when `since` is `None`.
fn stamped_since(stamp: &Value, since: Option<i64>) -> bool {
    since.is_none_or(|since| {
        stamp
            .as_str()
            .and_then(crate::domain::stats::rfc3339_millis)
            .is_some_and(|at| at >= since - TURN_CONTEXT_SKEW_MILLIS)
    })
}

/// What one rollout file says of the session `session`: the turns it says
/// started (`task_started`, with their stamps), its `token_usage_record`s
/// of the session, each with the model of the `turn_context` before it,
/// whether it has any `token_usage_record` at all, and the context of each
/// of its turns: the `token_count`s and top-level `compacted` records after
/// the turn's `task_started` and before the next one, which name no turn.
#[derive(Default)]
struct RolloutFile {
    started: Vec<(Value, String)>,
    records: Vec<UsageRecord>,
    has_records: bool,
    contexts: BTreeMap<String, TurnContext>,
}

fn read_rollout(path: &Path, session: &str) -> Result<RolloutFile, &'static str> {
    let file = fs::File::open(path).map_err(|_| ROLLOUT_UNREADABLE)?;
    let mut read = RolloutFile::default();
    let mut model: Option<String> = None;
    // The root turn the lines read now are of: the last `task_started`.
    let mut turn: Option<String> = None;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|_| ROLLOUT_UNREADABLE)?;
        if ![
            "\"turn_context\"",
            "\"task_started\"",
            "\"token_usage_record\"",
            "\"token_count\"",
            "\"compacted\"",
        ]
        .iter()
        .any(|kind| line.contains(kind))
        {
            continue;
        }
        // A line Codex is still writing is not read.
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let payload = &record["payload"];
        match record["type"].as_str() {
            Some("turn_context") => model = payload["model"].as_str().map(str::to_owned),
            Some("event_msg") if payload["type"] == "task_started" => {
                turn = payload["turn_id"].as_str().map(str::to_owned);
                if let Some(turn) = &turn {
                    read.started
                        .push((record["timestamp"].clone(), turn.clone()));
                    let context = read.contexts.entry(turn.clone()).or_default();
                    context.window = payload["model_context_window"].as_i64().or(context.window);
                }
            }
            Some("event_msg") if payload["type"] == "token_count" => {
                let Some(turn) = &turn else { continue };
                let info = &payload["info"];
                let context = read.contexts.entry(turn.clone()).or_default();
                context.peak = context
                    .peak
                    .max(info["last_token_usage"]["input_tokens"].as_i64());
                context.window = info["model_context_window"].as_i64().or(context.window);
            }
            Some("compacted") => {
                if let Some(turn) = &turn {
                    read.contexts.entry(turn.clone()).or_default().compactions += 1;
                }
            }
            Some("token_usage_record") => {
                read.has_records = true;
                let (Some(thread), Some(tokens)) = (
                    payload["thread_id"].as_str(),
                    turn_tokens(&payload["usage"]),
                ) else {
                    continue;
                };
                if payload["session_id"].as_str().unwrap_or(thread) != session {
                    continue;
                }
                let Some(root_turn) = payload["root_turn_id"]
                    .as_str()
                    .or_else(|| payload["turn_id"].as_str())
                else {
                    continue;
                };
                read.records.push(UsageRecord {
                    thread_id: thread.to_owned(),
                    root_turn_id: root_turn.to_owned(),
                    response_id: payload["response_id"].as_str().map(str::to_owned),
                    model: model.clone(),
                    tokens,
                });
            }
            _ => {}
        }
    }
    Ok(read)
}

/// The rollouts of the child threads of `thread` under `sessions`: those
/// but `root` whose `session_meta` names `thread` as their session, among
/// the files of the [`ROLLOUT_DAYS`] latest day directories written at
/// `since` or later (every one when `since` is `None`). A child thread
/// writes a rollout of its own while the root's turn runs, a child of an
/// earlier turn appending to the one it started.
fn child_rollouts(sessions: &Path, thread: &str, root: &Path, since: Option<i64>) -> Vec<PathBuf> {
    let written_since = |path: &Path| {
        since.is_none_or(|since| {
            fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|at| i64::try_from(at.as_millis()).ok())
                .is_some_and(|at| at >= since - TURN_CONTEXT_SKEW_MILLIS)
        })
    };
    let child_of = |path: &Path| {
        let Ok(file) = fs::File::open(path) else {
            return false;
        };
        let mut first = String::new();
        if BufReader::new(file).read_line(&mut first).is_err() {
            return false;
        }
        serde_json::from_str::<Value>(&first).is_ok_and(|meta| {
            meta["type"] == "session_meta"
                && meta["payload"]["session_id"] == thread
                && meta["payload"]["id"] != thread
        })
    };
    day_dirs(sessions)
        .into_iter()
        .flat_map(|day| {
            fs::read_dir(day)
                .into_iter()
                .flatten()
                .filter_map(|entry| Some(entry.ok()?.path()))
                .collect::<Vec<_>>()
        })
        .filter(|path| {
            path != root
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
                && written_since(path)
                && child_of(path)
        })
        .collect()
}

/// What the rollouts under `sessions` say of the Execution of `thread`
/// that started at `since` (unix milliseconds; `None`: every turn of the
/// thread): the root turns its rollout says started since then
/// (`task_started`), and the `token_usage_record`s of the thread's session
/// made in them, its child threads' included. Else why they cannot be
/// counted: [`ROLLOUT_MISSING`], [`ROLLOUT_UNREADABLE`],
/// [`TOKEN_USAGE_RECORD_MISSING`] or [`ROLLOUT_TURN_MISSING`].
pub fn rollout_usage(
    sessions: &Path,
    thread: &str,
    since: Option<i64>,
) -> Result<RolloutUsage, &'static str> {
    let path = rollout(sessions, thread).ok_or(ROLLOUT_MISSING)?;
    let root = read_rollout(&path, thread)?;
    if !root.has_records {
        return Err(TOKEN_USAGE_RECORD_MISSING);
    }
    let turns: Vec<String> = root
        .started
        .into_iter()
        .filter(|(stamp, _)| stamped_since(stamp, since))
        .map(|(_, turn)| turn)
        .collect();
    if turns.is_empty() {
        return Err(ROLLOUT_TURN_MISSING);
    }
    let mut records = root.records;
    for child in child_rollouts(sessions, thread, &path, since) {
        records.extend(read_rollout(&child, thread)?.records);
    }
    records.retain(|record| turns.contains(&record.root_turn_id));
    let mut contexts = root.contexts;
    contexts.retain(|turn, _| turns.contains(turn));
    Ok(RolloutUsage {
        thread_id: thread.to_owned(),
        turns,
        records,
        contexts,
    })
}

impl CodexTurnReader {
    /// A reader that looks for the model in the rollouts under `sessions`
    /// (`None`: Codex's home is not known, and no model is read), in the
    /// `turn_context` of the turn that starts now.
    pub fn reading(sessions: Option<PathBuf>) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|since| i64::try_from(since.as_millis()).ok());
        Self::reading_since(sessions, now)
    }

    /// [`Self::reading`] for a turn that started at `since` (unix
    /// milliseconds; `None`: any `turn_context`).
    pub fn reading_since(sessions: Option<PathBuf>, since: Option<i64>) -> Self {
        Self {
            sessions,
            since,
            ..Self::default()
        }
    }

    /// The model of the turn, or why none was read.
    fn model(&self) -> Result<String, String> {
        let thread = self.thread_id.as_deref().ok_or("Codex named no thread")?;
        let sessions = self
            .sessions
            .as_deref()
            .ok_or("Codex's home is not known (neither CODEX_HOME nor HOME is set)")?;
        rollout_model(sessions, thread, self.since)
    }

    /// What the rollouts say of the turn's tokens, or why they cannot be
    /// counted.
    fn rollout_usage(&self) -> Result<RolloutUsage, &'static str> {
        let thread = self.thread_id.as_deref().ok_or(ROLLOUT_MISSING)?;
        let sessions = self.sessions.as_deref().ok_or(ROLLOUT_MISSING)?;
        rollout_usage(sessions, thread, self.since)
    }

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

    fn started(&mut self, item: &Value) -> Vec<TurnSignal> {
        if let Some(tool) = item["type"]
            .as_str()
            .filter(|tool| TIMED_ITEMS.contains(tool))
        {
            self.commands.push(TurnCommand {
                id: item["id"].as_str().unwrap_or_default().to_owned(),
                tool: tool.to_owned(),
                command: item["command"].as_str().map(str::to_owned),
                started: self.at,
                ..TurnCommand::default()
            });
        }
        match item["type"].as_str() {
            Some("command_execution") => vec![TurnSignal::Tool(format!(
                "shell {}",
                shortened(item["command"].as_str().unwrap_or(""), COMMAND_KEPT)
            ))],
            _ => Vec::new(),
        }
    }

    /// The end of a command or tool `item` of [`TIMED_ITEMS`]: the one
    /// its start began, else one whose start was not read.
    fn ended(&mut self, item: &Value) {
        let Some(tool) = item["type"]
            .as_str()
            .filter(|tool| TIMED_ITEMS.contains(tool))
        else {
            return;
        };
        let id = item["id"].as_str().unwrap_or_default();
        let at = match self
            .commands
            .iter()
            .position(|command| command.ended.is_none() && command.id == id)
        {
            Some(at) => at,
            None => {
                self.commands.push(TurnCommand {
                    id: id.to_owned(),
                    tool: tool.to_owned(),
                    ..TurnCommand::default()
                });
                self.commands.len() - 1
            }
        };
        let command = &mut self.commands[at];
        command.ended = self.at;
        if let Some(text) = item["command"].as_str() {
            command.command = Some(text.to_owned());
        }
        command.exit_code = item["exit_code"].as_i64();
        command.status = item["status"].as_str().map(str::to_owned);
        command.failed_tests = item["aggregated_output"]
            .as_str()
            .map(|output| failed_tests(output).names)
            .unwrap_or_default();
    }

    fn completed_item(&mut self, item: &Value) -> Vec<TurnSignal> {
        self.ended(item);
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
            Some("item.started") => self.started(&event["item"]),
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

    fn stamp(&mut self, at: i64) {
        self.at = Some(at);
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
        // Codex's output does not name the model: its rollout does.
        let model = self.model();
        // The thread's running total, which the turn's tokens fall back to
        // when its rollouts cannot be counted.
        let tokens = self
            .completed
            .as_ref()
            .and_then(|event| turn_tokens(&event["usage"]));
        let rollout = self.rollout_usage();
        // Why the rollouts could not be counted is said even when the
        // fallback could not be read either: the rollouts are what the
        // tokens are counted from.
        let (tokens_source, tokens_reason) = match (&rollout, &tokens) {
            (Ok(_), _) => (Some(TokenSource::UsageRecord), None),
            (Err(why), Some(_)) => (Some(TokenSource::ThreadUsage), Some(*why)),
            (Err(why), None) => (None, Some(*why)),
        };
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
            tokens_source,
            tokens_reason,
            tokens,
            // `turn.completed` carries the thread's total so far, a
            // resumed thread's earlier turns included.
            tokens_cumulative: true,
            tokens_by_model: Vec::new(),
            children: None,
            // The rollout's context comes with its tokens; without it, why
            // it could not be read.
            context: match &rollout {
                Ok(rollout) => rollout.tokens(&[]).context,
                Err(why) => ExecutionContext::unmeasured(why),
            },
            rollout: rollout.ok(),
            cost_cumulative: false,
            permission_denials: std::mem::take(&mut self.denials),
            session_missing: stderr.contains("no rollout found"),
            model: model.as_ref().ok().cloned(),
            model_unknown: model.err(),
            commands: Some(std::mem::take(&mut self.commands)),
        }
    }
}

/// The tokens of the thread so far from the `usage` of a `turn.completed`
/// (the thread's running total, not the turn's own), or of one response
/// from that of a `token_usage_record`, in the kinds of
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
    use crate::domain::tokens::ExecutionTokens;
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

    /// A rollout as Codex writes it: `session_meta`, then a `turn_context`
    /// for each turn.
    fn write_rollout(sessions: &Path, day: &str, thread: &str, models: &[&str]) {
        let dir = sessions.join(day);
        fs::create_dir_all(&dir).unwrap();
        let mut text = json!({"type": "session_meta", "payload": {"id": thread}}).to_string();
        // The turns a minute apart.
        for (minute, model) in models.iter().enumerate() {
            text.push('\n');
            text.push_str(
                &json!({"timestamp": format!("2026-09-28T01:{minute:02}:00.000Z"),
                        "type": "turn_context", "payload": {"model": model, "effort": "medium"}})
                .to_string(),
            );
            text.push('\n');
            text.push_str(
                &json!({"type": "event_msg", "payload": {"model": "not this"}}).to_string(),
            );
        }
        fs::write(
            dir.join(format!("rollout-2026-09-28T01-13-10-{thread}.jsonl")),
            text,
        )
        .unwrap();
    }

    #[test]
    fn the_model_of_a_turn_is_read_from_its_thread_s_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        write_rollout(
            &sessions,
            "2026/09/27",
            "th-1",
            &["gpt-6-astra", "gpt-6-nova"],
        );
        write_rollout(&sessions, "2026/09/28", "th-2", &[]);
        fs::create_dir_all(sessions.join("2026/09/29")).unwrap();
        // The last turn's, from an earlier day's rollout (a resumed thread).
        assert_eq!(
            rollout_model(&sessions, "th-1", None).as_deref(),
            Ok("gpt-6-nova")
        );
        let (mut reader, _) = {
            let mut reader = CodexTurnReader::reading_since(Some(sessions.clone()), None);
            let signals =
                reader.line(&json!({"type": "thread.started", "thread_id": "th-1"}).to_string());
            (reader, signals)
        };
        let result = reader.finish(Some(&exit(0)), "");
        assert_eq!(result.model.as_deref(), Some("gpt-6-nova"));
        assert_eq!(result.model_unknown, None);
        // Otherwise the result says why there is none.
        assert!(
            rollout_model(&sessions, "th-2", None)
                .unwrap_err()
                .contains("names no model")
        );
        assert!(
            rollout_model(&sessions, "th-3", None)
                .unwrap_err()
                .contains("no rollout of thread th-3")
        );
        let mut reader = CodexTurnReader::reading_since(Some(sessions.clone()), None);
        let result = reader.finish(None, "");
        assert_eq!(result.model, None);
        assert_eq!(
            result.model_unknown.as_deref(),
            Some("Codex named no thread")
        );
        // A turn reads only the `turn_context` written since it started:
        // one that failed before Codex wrote its own gets no earlier turn's.
        let at = |text: &str| crate::domain::stats::rfc3339_millis(text).unwrap();
        assert_eq!(
            rollout_model(&sessions, "th-1", Some(at("2026-09-28T01:01:00.500Z"))).as_deref(),
            Ok("gpt-6-nova")
        );
        assert_eq!(
            rollout_model(&sessions, "th-1", Some(at("2026-09-28T01:00:01Z"))).as_deref(),
            Ok("gpt-6-nova")
        );
        assert!(
            rollout_model(&sessions, "th-1", Some(at("2026-09-28T01:05:00Z")))
                .unwrap_err()
                .contains("no model for this turn")
        );
        // A reader made now takes none of these old ones.
        let mut reader = CodexTurnReader::reading(Some(sessions.clone()));
        reader.line(&json!({"type": "thread.started", "thread_id": "th-1"}).to_string());
        assert!(
            reader
                .finish(None, "")
                .model_unknown
                .unwrap()
                .contains("for this turn")
        );
        let mut reader = CodexTurnReader::reading(None);
        reader.line(&json!({"type": "thread.started", "thread_id": "th-1"}).to_string());
        assert!(
            reader
                .finish(None, "")
                .model_unknown
                .unwrap()
                .contains("home is not known")
        );
        // A missing directory has no rollout.
        assert!(rollout_model(&dir.path().join("none"), "th-1", None).is_err());
    }

    /// A turn of a rollout: its id, its time and its responses' ids and
    /// input tokens.
    type RolloutTurn<'a> = (&'a str, &'a str, &'a [(&'a str, i64)]);

    /// A rollout of the newer Codex: the session's meta, then for each
    /// turn its `task_started`, its `turn_context` and a
    /// `token_usage_record` per response, at the turn's time.
    fn usage_rollout(path: &Path, thread: &str, session: &str, turns: &[RolloutTurn]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut lines =
            vec![json!({"type": "session_meta", "payload": {"id": thread, "session_id": session}})];
        for (turn, at, responses) in turns {
            if thread == session {
                lines.push(json!({"timestamp": at, "type": "event_msg",
                    "payload": {"type": "task_started", "turn_id": turn, "root_turn_id": turn}}));
            }
            lines.push(json!({"timestamp": at, "type": "turn_context", "payload": {"model": format!("gpt-{thread}")}}));
            for (response, input) in *responses {
                lines.push(json!({"timestamp": at, "type": "token_usage_record", "payload": {
                    "thread_id": thread, "turn_id": format!("{thread}-{turn}"), "session_id": session,
                    "root_turn_id": turn, "response_id": response,
                    "usage": {"input_tokens": input, "cached_input_tokens": input / 2, "cache_write_input_tokens": 0,
                              "output_tokens": 3, "reasoning_output_tokens": 1, "total_tokens": input + 3},
                    // The running totals are not what is summed.
                    "turn_token_usage": {"input_tokens": 999_999, "output_tokens": 999_999},
                    "thread_token_usage": {"input_tokens": 999_999, "output_tokens": 999_999}}}));
            }
        }
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(path, text.join("\n") + "\n").unwrap();
    }

    fn at(text: &str) -> Option<i64> {
        crate::domain::stats::rfc3339_millis(text)
    }

    /// The tokens of an Execution are the `token_usage_record`s of the root
    /// turns that started since it did, its child threads' (in their own
    /// rollouts, under the root's session) included and each response
    /// once; another session's are not counted, nor an earlier turn's.
    #[test]
    fn a_turns_tokens_are_its_rollouts_records_with_its_child_threads() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let day = sessions.join("2026/10/08");
        usage_rollout(
            &day.join("rollout-2026-10-08T01-00-00-root.jsonl"),
            "root",
            "root",
            &[
                ("t1", "2020-01-01T01:00:00.000Z", &[("r1", 100)]),
                (
                    "t2",
                    "2020-01-01T02:00:00.000Z",
                    &[("r2", 40), ("r2", 40), ("r3", 20)],
                ),
            ],
        );
        usage_rollout(
            &day.join("rollout-2026-10-08T02-00-05-child.jsonl"),
            "child",
            "root",
            &[("t2", "2020-01-01T02:00:05.000Z", &[("r2", 10)])],
        );
        usage_rollout(
            &day.join("rollout-2026-10-08T02-00-06-other.jsonl"),
            "other",
            "other",
            &[("t2", "2020-01-01T02:00:06.000Z", &[("r9", 1_000)])],
        );
        let line = |value: Value| value.to_string();
        let mut reader =
            CodexTurnReader::reading_since(Some(sessions.clone()), at("2020-01-01T02:00:00.000Z"));
        reader.line(&line(
            json!({"type": "thread.started", "thread_id": "root"}),
        ));
        reader.line(&line(
            json!({"type": "turn.completed", "usage": {"input_tokens": 160, "output_tokens": 9}}),
        ));
        let result = reader.finish(Some(&exit(0)), "");
        assert_eq!(result.tokens_source, Some(TokenSource::UsageRecord));
        assert_eq!(result.tokens_reason, None);
        let own = result.rollout.as_ref().unwrap().tokens(&[]);
        // 40 + 20 of the root, 10 of the child; the cached half apart.
        assert_eq!(
            own.tokens.as_ref().map(TokenUsage::payload),
            Some(
                json!({"input": 35, "output": 9, "cache_read": 35, "cache_creation": 0, "messages": 1})
            )
        );
        assert_eq!(own.children, Some(1));
        assert_eq!(own.turns, ["t2"]);
        let models: Vec<(&str, i64)> = own
            .by_model
            .iter()
            .map(|m| (m.model.as_str(), m.input))
            .collect();
        assert_eq!(models, [("gpt-root", 30), ("gpt-child", 5)]);
        // The thread's total is kept for a later turn that falls back.
        assert_eq!(result.tokens.map(|t| t.input), Some(160));
        // Every turn, when the Execution's start is not known.
        let all = rollout_usage(&sessions, "root", None).unwrap();
        assert_eq!(all.turns, ["t1", "t2"]);
        assert_eq!(
            all.tokens(&[]).tokens.map(|t| t.input + t.cache_read),
            Some(170)
        );
    }

    /// Each Execution of a thread that is resumed counts only its own
    /// turns: by the turns that started since it did, and, when the start
    /// cannot tell them apart (a wrapper that took over or started again),
    /// by the turns the run's events say were counted.
    #[test]
    fn a_resumed_thread_counts_no_earlier_executions_turns_again() {
        use crate::domain::{RunEvent, event_kind::TURN_FINISHED, turn::counted_rollout_turns};
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let path = sessions.join("2026/10/08/rollout-2026-10-08T01-00-00-th.jsonl");
        let line = |value: Value| value.to_string();
        let execution = |since: Option<i64>| {
            let mut reader = CodexTurnReader::reading_since(Some(sessions.clone()), since);
            reader.line(&line(json!({"type": "thread.started", "thread_id": "th"})));
            reader.finish(Some(&exit(0)), "").rollout.unwrap()
        };
        let mut events: Vec<RunEvent> = Vec::new();
        let record = |tokens: &ExecutionTokens, events: &mut Vec<RunEvent>| {
            let mut payload = json!({"session_id": "th"});
            tokens.record(&mut payload);
            events.push(RunEvent {
                id: crate::domain::EventId::new(events.len() as i64 + 1),
                task_id: None,
                goal_id: None,
                run_id: None,
                kind: TURN_FINISHED.to_owned(),
                payload,
                created_at: String::new(),
                actor: None,
            });
        };
        let input =
            |tokens: &ExecutionTokens| tokens.tokens.as_ref().map(|t| t.input + t.cache_read);
        // The first Execution.
        usage_rollout(
            &path,
            "th",
            "th",
            &[("t1", "2020-01-01T01:00:00.000Z", &[("r1", 100)])],
        );
        let first =
            execution(at("2020-01-01T01:00:00.000Z")).tokens(&counted_rollout_turns(&events, "th"));
        assert_eq!(input(&first), Some(100));
        record(&first, &mut events);
        // The resume appends its turn to the same rollout.
        usage_rollout(
            &path,
            "th",
            "th",
            &[
                ("t1", "2020-01-01T01:00:00.000Z", &[("r1", 100)]),
                ("t2", "2020-01-01T03:00:00.000Z", &[("r2", 60)]),
            ],
        );
        let second =
            execution(at("2020-01-01T03:00:00.000Z")).tokens(&counted_rollout_turns(&events, "th"));
        assert_eq!(input(&second), Some(60));
        assert_eq!(second.turns, ["t2"]);
        // A start that does not tell the turns apart: the events do, read
        // by whichever wrapper reads them.
        let again = execution(None).tokens(&counted_rollout_turns(&events, "th"));
        assert_eq!(input(&again), Some(60));
        record(&second, &mut events);
        let third = execution(None).tokens(&counted_rollout_turns(&events, "th"));
        assert_eq!(input(&third), Some(0), "nothing new: a measured 0");
    }

    /// A rollout that cannot be counted leaves the turn's tokens to its
    /// `turn.completed`'s thread total, and says why; without one either,
    /// nothing is counted.
    #[test]
    fn tokens_fall_back_to_the_thread_total_when_the_rollout_cannot_be_counted() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let day = sessions.join("2026/10/08");
        fs::create_dir_all(&day).unwrap();
        // An older Codex's rollout, without token_usage_record.
        write_rollout(&sessions, "2026/10/08", "old", &["gpt-6-astra"]);
        // A rollout that cannot be read (a directory by its name).
        fs::create_dir_all(day.join("rollout-2026-10-08T00-00-00-broken.jsonl")).unwrap();
        // A rollout whose turns all started before the Execution did.
        usage_rollout(
            &day.join("rollout-2026-10-08T00-00-00-early.jsonl"),
            "early",
            "early",
            &[("t1", "2020-01-01T00:00:00.000Z", &[("r1", 10)])],
        );
        let line = |value: Value| value.to_string();
        let read = |sessions: Option<PathBuf>, thread: &str, completed: bool| {
            let mut reader =
                CodexTurnReader::reading_since(sessions, at("2020-01-01T05:00:00.000Z"));
            reader.line(&line(
                json!({"type": "thread.started", "thread_id": thread}),
            ));
            if completed {
                reader.line(&line(json!({"type": "turn.completed", "usage": {"input_tokens": 30, "cached_input_tokens": 20, "output_tokens": 5}})));
            }
            reader.finish(Some(&exit(0)), "")
        };
        for (sessions, thread, why) in [
            (Some(sessions.clone()), "none", ROLLOUT_MISSING),
            (None, "old", ROLLOUT_MISSING),
            (Some(sessions.clone()), "broken", ROLLOUT_UNREADABLE),
            (Some(sessions.clone()), "old", TOKEN_USAGE_RECORD_MISSING),
            (Some(sessions.clone()), "early", ROLLOUT_TURN_MISSING),
        ] {
            let result = read(sessions.clone(), thread, true);
            assert_eq!(result.rollout, None, "{thread}");
            assert_eq!(
                result.tokens_source,
                Some(TokenSource::ThreadUsage),
                "{thread}"
            );
            assert_eq!(result.tokens_reason, Some(why), "{thread}");
            // Its context is not measured either, with the same reason.
            assert_eq!(
                result.context,
                ExecutionContext::unmeasured(why),
                "{thread}"
            );
            assert!(result.tokens_cumulative);
            assert_eq!(
                result.tokens.map(|tokens| tokens.payload()),
                Some(
                    json!({"input": 10, "output": 5, "cache_read": 20, "cache_creation": 0, "messages": 1})
                )
            );
            let result = read(sessions, thread, false);
            assert_eq!(
                (result.tokens, result.tokens_source),
                (None, None),
                "{thread}"
            );
            assert_eq!(result.tokens_reason, Some(why), "{thread}");
        }
    }

    /// An Execution's context is read from its root thread's rollout, in
    /// its own root turns: the largest `last_token_usage.input_tokens` of
    /// their `token_count`s, the `model_context_window` and their top-level
    /// `compacted` records. A resumed thread's earlier Execution's
    /// compactions in the same rollout and a child thread's (in its own
    /// rollout) are not counted; an Execution without any is a measured 0.
    #[test]
    fn an_executions_context_is_its_own_root_turns_in_the_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let day = sessions.join("2026/10/08");
        fs::create_dir_all(&day).unwrap();
        let started = |turn: &str, at: &str| {
            json!({"timestamp": at, "type": "event_msg", "payload": {"type": "task_started",
                "turn_id": turn, "root_turn_id": turn, "model_context_window": 258_400}})
        };
        let count = |at: &str, input: i64| {
            json!({"timestamp": at, "type": "event_msg", "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": 9_999_999},
                "last_token_usage": {"input_tokens": input, "cached_input_tokens": input / 2},
                "model_context_window": 258_400}}})
        };
        let compacted = |at: &str| {
            json!({"timestamp": at, "type": "compacted",
                "payload": {"message": "", "replacement_history": []}})
        };
        let record = |turn: &str, at: &str, response: &str| {
            json!({"timestamp": at, "type": "token_usage_record", "payload": {
                "thread_id": "root", "turn_id": turn, "session_id": "root", "root_turn_id": turn,
                "response_id": response, "usage": {"input_tokens": 10, "output_tokens": 1}}})
        };
        let write = |name: &str, lines: &[Value]| {
            let text: Vec<String> = lines.iter().map(Value::to_string).collect();
            fs::write(day.join(name), text.join("\n") + "\n").unwrap();
        };
        let (one, two) = ("2020-01-01T01:00:00.000Z", "2020-01-01T03:00:00.000Z");
        let read = |since: &str| {
            let mut reader = CodexTurnReader::reading_since(Some(sessions.clone()), at(since));
            reader.line(&json!({"type": "thread.started", "thread_id": "root"}).to_string());
            reader.finish(Some(&exit(0)), "")
        };
        // The first Execution: a large call and two compactions.
        let mut root = vec![
            json!({"type": "session_meta", "payload": {"id": "root", "session_id": "root"}}),
            started("t1", one),
            count(one, 240_000),
            record("t1", one, "r1"),
            compacted(one),
            compacted(one),
        ];
        write("rollout-2026-10-08T01-00-00-root.jsonl", &root);
        let first = read(one);
        assert_eq!(
            first.context,
            ExecutionContext {
                peak: Some(240_000),
                window: Some(258_400),
                compactions: Some(2),
                reason: None,
            }
        );
        // The resume goes on in the same rollout: one compaction.
        root.extend([
            started("t2", two),
            count(two, 90_000),
            record("t2", two, "r2"),
            compacted(two),
            count(two, 130_000),
        ]);
        write("rollout-2026-10-08T01-00-00-root.jsonl", &root);
        // A child thread of the resume, with a larger call and its own
        // compaction, in its own rollout.
        write(
            "rollout-2026-10-08T03-00-05-child.jsonl",
            &[
                json!({"type": "session_meta", "payload": {"id": "child", "session_id": "root"}}),
                started("c1", two),
                count(two, 250_000),
                compacted(two),
            ],
        );
        let resumed = read(two);
        assert_eq!(
            resumed.context,
            ExecutionContext {
                peak: Some(130_000),
                window: Some(258_400),
                compactions: Some(1),
                reason: None,
            }
        );
        let mut payload = json!({});
        resumed.rollout.unwrap().tokens(&[]).record(&mut payload);
        assert_eq!(
            (
                &payload["peak_context"],
                &payload["context_window"],
                &payload["compactions"]
            ),
            (&json!(130_000), &json!(258_400), &json!(1))
        );
        // An Execution that compacted nothing is a measured 0.
        write(
            "rollout-2026-10-08T05-00-00-quiet.jsonl",
            &[
                json!({"type": "session_meta", "payload": {"id": "quiet", "session_id": "quiet"}}),
                started("q1", "2020-01-01T05:00:00.000Z"),
                count("2020-01-01T05:00:00.000Z", 50_000),
                record("q1", "2020-01-01T05:00:00.000Z", "r9"),
            ],
        );
        let mut reader =
            CodexTurnReader::reading_since(Some(sessions.clone()), at("2020-01-01T05:00:00.000Z"));
        reader.line(&json!({"type": "thread.started", "thread_id": "quiet"}).to_string());
        assert_eq!(
            reader.finish(Some(&exit(0)), "").context,
            ExecutionContext::measured(Some(50_000), Some(258_400), 0)
        );
    }

    /// The reader keeps each command and tool the turn ran with when the
    /// wrapper read its start and end (task 1354): a command whose start
    /// was not read has none, one that did not end has no end, and the
    /// tests a command's output names as failed are kept with it.
    #[test]
    fn a_turns_commands_carry_when_their_lines_were_read() {
        let mut reader = CodexTurnReader::default();
        let at = |reader: &mut CodexTurnReader, millis: i64, line: Value| {
            reader.stamp(millis);
            reader.line(&line.to_string());
        };
        at(
            &mut reader,
            1_000,
            json!({"type": "thread.started", "thread_id": "th-1"}),
        );
        at(
            &mut reader,
            1_000,
            json!({"type": "item.started", "item": {"id": "c1", "type": "command_execution", "command": "cargo test", "status": "in_progress"}}),
        );
        at(
            &mut reader,
            9_000,
            json!({"type": "item.completed", "item": {"id": "c1", "type": "command_execution", "command": "cargo test", "exit_code": 101,
            "aggregated_output": "failures:\n    a::b\n\ntest result: FAILED. 1 passed; 1 failed\n", "status": "failed"}}),
        );
        at(
            &mut reader,
            10_000,
            json!({"type": "item.completed", "item": {"id": "c2", "type": "command_execution", "command": "git status", "exit_code": 0, "aggregated_output": "", "status": "completed"}}),
        );
        at(
            &mut reader,
            11_000,
            json!({"type": "item.started", "item": {"id": "t1", "type": "mcp_tool_call", "server": "s", "tool": "t"}}),
        );
        at(
            &mut reader,
            12_000,
            json!({"type": "item.completed", "item": {"id": "i9", "type": "agent_message", "text": "hi"}}),
        );
        at(
            &mut reader,
            13_000,
            json!({"type": "item.started", "item": {"id": "c3", "type": "command_execution", "command": "sleep 100"}}),
        );
        let commands = reader.finish(None, "").commands.unwrap();
        let got: Vec<_> = commands
            .iter()
            .map(|c| {
                (
                    c.id.as_str(),
                    c.tool.as_str(),
                    c.started,
                    c.ended,
                    c.exit_code,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("c1", SHELL_ITEM, Some(1_000), Some(9_000), Some(101)),
                ("c2", SHELL_ITEM, None, Some(10_000), Some(0)),
                ("t1", "mcp_tool_call", Some(11_000), None, None),
                ("c3", SHELL_ITEM, Some(13_000), None, None),
            ]
        );
        assert_eq!(commands[0].failed_tests, ["a::b"]);
        assert_eq!(commands[0].status.as_deref(), Some("failed"));
        assert_eq!(commands[1].command.as_deref(), Some("git status"));
        // The reader of a job reads none of it, unstamped.
        let (mut unstamped, _) = read(&[
            json!({"type": "item.started", "item": {"id": "c", "type": "command_execution", "command": "ls"}}),
        ]);
        assert_eq!(
            unstamped.finish(None, "").commands.unwrap()[0].started,
            None
        );
    }
}
