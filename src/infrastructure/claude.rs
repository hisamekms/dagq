//! What Claude Code writes that the runtime reads ([`AgentSignals`]): the
//! input its `Stop` hook writes to the idle marker (ADR-0016) and the
//! output of a headless job that failed at a wall (task 438). The formats
//! are Claude Code's own; the runtime reads no screen of it.

use serde_json::Value;

use super::adapters::ClaudeCode;
use crate::{
    application::{AgentSignals, IdleHook},
    domain::{headless_job::JobFailure, queue_hold::Wall, stall::BackgroundTask},
};

/// How Claude Code reports a login that ran out or was never there (task
/// 266): its error line, under the tool call or the prompt it failed, starts
/// with one of these. Text that merely contains them (a grep, a test's
/// output, this file on the screen) does not count.
const AUTH_ERRORS: &[&str] = &[
    "API Error: 401",
    "Invalid API key",
    "OAuth token has expired",
    "OAuth token revoked",
];

/// How Claude Code reports that the account reached its usage limit
/// (task 438): its error line, under the tool call or the prompt it
/// stopped, or a headless job's output line, starts with one of these, or
/// reads `<window> limit reached · resets <when>` ([`usage_limit_line`]).
const USAGE_LIMITS: &[&str] = &[
    "Claude AI usage limit reached",
    "Claude usage limit reached",
    "You've hit your limit",
    "You've hit your usage limit",
    "You've reached your usage limit",
    "Credit balance is too low",
];

/// The longest window name before `limit reached` (`5-hour`, `Opus
/// weekly`, ...): a longer head is a sentence of the work.
const LIMIT_WINDOW_CHARS: usize = 24;

/// The wall a headless job's output (its stdout and stderr, a `claude -p`
/// JSON result's `result` too) shows it stopped at, if any (task 438):
/// the first line that is [`auth_line`] or [`usage_limit_line`]. Read only
/// from a job that failed, whose output is Claude Code's, not the work's.
pub fn job_wall(output: &str) -> Option<Wall> {
    output.lines().find_map(|line| {
        let parsed: Option<Value> = serde_json::from_str(line.trim()).ok();
        let fields: Vec<String> = parsed
            .iter()
            .flat_map(|value| ["result", "error", "message"].map(|key| value.get(key).cloned()))
            .flatten()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        std::iter::once(line)
            .chain(fields.iter().flat_map(|field| field.lines()))
            .find_map(|text| {
                let text = text.trim().trim_start_matches(['⎿', '⏺', ' ']);
                let text = text.strip_prefix("Error: ").unwrap_or(text);
                if auth_line(text) {
                    Some(Wall::Authentication)
                } else if usage_limit_line(text) {
                    Some(Wall::UsageLimit)
                } else {
                    None
                }
            })
    })
}

/// Why a Claude headless job failed, from its output, in the classes
/// shared by every provider (ADR-t1063-1 decision 4): a login that ran out
/// is `authentication`, the usage limit `usage_limit` ([`job_wall`]), and
/// anything else `other`. An executable that is not there or does not start
/// never gets to write output: the start's error says that
/// ([`crate::application::job_start_failure`]).
pub fn job_failure(output: &str) -> JobFailure {
    job_wall(output).map_or(JobFailure::Other, JobFailure::of_wall)
}

fn auth_line(line: &str) -> bool {
    AUTH_ERRORS.iter().any(|error| line.starts_with(error)) && line.contains("/login")
}

/// Whether a line is Claude Code's report of the usage limit: it starts
/// with one of [`USAGE_LIMITS`], or it is `<window> limit reached ∙ resets
/// ...` (or `·`) with a short window name.
fn usage_limit_line(line: &str) -> bool {
    if USAGE_LIMITS.iter().any(|limit| line.starts_with(limit)) {
        return true;
    }
    let Some((window, rest)) = line.split_once("limit reached") else {
        return false;
    };
    let rest = rest.trim_start();
    window.chars().count() <= LIMIT_WINDOW_CHARS
        && window
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | ' '))
        && (rest.starts_with('∙') || rest.starts_with('·'))
        && rest.contains("reset")
}

/// The idle marker is the input of Claude Code's `Stop` hook, as JSON.
/// Claude Code lists the background tasks of the turn in `background_tasks`,
/// and a `/exit` sent while one is `running` stops at its "Background work
/// is running" dialog, which stays until someone answers it. When the work
/// ends, Claude Code takes the turn up again and the hook writes a new
/// marker. A hook input without `background_tasks` (an older Claude Code),
/// or one that is not JSON, counts as idle.
pub fn idle_hook(content: &[u8]) -> IdleHook {
    let hook: Value = serde_json::from_slice(content).unwrap_or(Value::Null);
    let text = |task: &Value, name: &str| task[name].as_str().unwrap_or_default().to_owned();
    let background_tasks: Vec<BackgroundTask> = hook
        .get("background_tasks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|task| task["status"] == "running")
        .map(|task| BackgroundTask {
            id: text(task, "id"),
            description: text(task, "description"),
            command: text(task, "command"),
        })
        .collect();
    let field = |name: &'static str| (name, hook.get(name).cloned().unwrap_or(Value::Null));
    IdleHook {
        background_running: !background_tasks.is_empty(),
        background_tasks,
        evidence: vec![
            field("hook_event_name"),
            field("session_id"),
            field("stop_hook_active"),
        ],
    }
}

impl AgentSignals for ClaudeCode {
    fn job_failure(&self, output: &str) -> JobFailure {
        job_failure(output)
    }

    fn idle_hook(&self, content: &[u8]) -> IdleHook {
        idle_hook(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn idle_hook_reads_background_tasks_of_the_stop_hook_input() {
        let claude = ClaudeCode {
            executable: "claude".into(),
        };
        for (hook, running) in [
            // An older Claude Code writes no `background_tasks`.
            (json!({"hook_event_name": "Stop"}), false),
            (json!({"background_tasks": []}), false),
            (
                json!({"background_tasks": [{"id": "b1", "status": "completed"}]}),
                false,
            ),
            (
                json!({"background_tasks": [
                    {"id": "b1", "status": "completed"},
                    {"id": "b2", "type": "shell", "status": "running"}
                ]}),
                true,
            ),
        ] {
            let idle = claude.idle_hook(hook.to_string().as_bytes());
            assert_eq!(idle.background_running, running, "{hook}");
            assert_eq!(idle.background_tasks.len(), usize::from(running), "{hook}");
        }
        let idle = claude.idle_hook(
            json!({"background_tasks": [
                {"id": "b2", "status": "running", "description": "cargo test", "command": "cargo test --locked"}
            ]})
            .to_string()
            .as_bytes(),
        );
        assert_eq!(
            idle.background_tasks,
            [BackgroundTask {
                id: "b2".into(),
                description: "cargo test".into(),
                command: "cargo test --locked".into(),
            }]
        );
        let idle = claude.idle_hook(
            json!({"hook_event_name": "Stop", "session_id": "s1", "stop_hook_active": false})
                .to_string()
                .as_bytes(),
        );
        assert_eq!(
            idle.evidence,
            vec![
                ("hook_event_name", json!("Stop")),
                ("session_id", json!("s1")),
                ("stop_hook_active", json!(false)),
            ]
        );
        // A marker that is not JSON still tells the agent stopped.
        let idle = claude.idle_hook(b"not json");
        assert!(!idle.background_running);
        assert_eq!(idle.evidence[0], ("hook_event_name", Value::Null));
    }

    #[test]
    fn a_wall_is_read_only_from_a_line_that_starts_with_it() {
        assert!(auth_line(
            "API Error: 401 {\"type\":\"error\"} · Please run /login"
        ));
        assert!(auth_line("Invalid API key · Please run /login"));
        // Text that only mentions the error is the work.
        assert!(!auth_line(
            "src/claude.rs:12: \"API Error: 401\" · Please run /login"
        ));
        assert!(!auth_line("API Error: 401 (retrying)"));
        for line in [
            "5-hour limit reached ∙ resets 3pm",
            "Opus weekly limit reached · resets Mon 9am (Asia/Tokyo)",
            "Claude AI usage limit reached|1759000000",
            "You've hit your limit · resets 11pm",
        ] {
            assert!(usage_limit_line(line), "{line}");
            assert!(!auth_line(line), "{line}");
        }
        assert!(!usage_limit_line(
            "src/claude.rs:12: \"Claude AI usage limit reached\""
        ));
        assert!(!usage_limit_line(
            "The retry budget of the whole job's limit reached · resets nothing"
        ));
        assert!(!usage_limit_line("5-hour limit reached (a note)"));
    }

    #[test]
    fn job_wall_reads_a_headless_jobs_output() {
        let claude = ClaudeCode {
            executable: "claude".into(),
        };
        assert_eq!(
            job_wall("Invalid API key · Please run /login\n"),
            Some(Wall::Authentication)
        );
        assert_eq!(
            claude.job_failure(
                "{\"type\":\"result\",\"is_error\":true,\"result\":\"API Error: 401 {\\\"type\\\":\\\"authentication_error\\\"} · Please run /login\"}\n"
            ),
            JobFailure::Authentication
        );
        assert_eq!(
            job_failure("Claude AI usage limit reached|1759000000\n"),
            JobFailure::UsageLimit
        );
        assert_eq!(job_failure("no verdict here\n"), JobFailure::Other);
        assert_eq!(
            job_wall(
                "{\"type\":\"result\",\"is_error\":true,\"result\":\"Claude AI usage limit reached|1759000000\"}"
            ),
            Some(Wall::UsageLimit)
        );
        assert_eq!(
            job_wall("starting\nError: 5-hour limit reached ∙ resets 3pm\n"),
            Some(Wall::UsageLimit)
        );
        assert_eq!(
            job_wall("Credit balance is too low\n"),
            Some(Wall::UsageLimit)
        );
        assert_eq!(job_wall("no verdict here\n"), None);
        assert_eq!(job_wall("{\"verdict\":\"pass\"}\n"), None);
        assert_eq!(
            job_wall("the diff adds \"Please run /login\" to the tests\n"),
            None
        );
    }
}
