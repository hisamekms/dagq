//! The turns of a headless worker (ADR-t813-1): one non-interactive call of
//! the agent per turn, its output read into the provider-neutral
//! [`TurnSignal`]s and [`TurnResult`] below, and the files in the run's
//! `turns/` directory the supervisor and the session wrapper talk through.
//! The supervisor writes a request ([`TurnRequest`]) where it would type
//! into an interactive session, and the exit request where it would type
//! `/exit`; the wrapper takes each request in order and runs it as the
//! next turn (a resume of the same session), and writes the run's idle
//! marker when the turn's process ended ([`idle_marker`]).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::DomainError;

/// The directory of a run's turns, in its run directory.
pub const TURNS_DIR: &str = "turns";

/// The exit request in [`TURNS_DIR`]: the wrapper stops a turn that runs
/// and exits.
pub const EXIT_FILE: &str = "exit";

/// The limits of the run's turns in [`TURNS_DIR`] ([`TurnLimits`]), which
/// the supervisor writes from its `[stall]` settings.
pub const LIMITS_FILE: &str = "limits.json";

/// How many times the runtime asks a turn that ended with neither a
/// receipt nor an open question to go on, one resume each, before the
/// run's recovery job looks at it (ADR-t813-1 decision 9, amending
/// ADR-0047 decision 30's single nudge for headless runs).
pub const HEADLESS_NUDGES: usize = 2;

/// A turn that ended with neither a receipt nor an open question after
/// this many refused tool calls (`permission_denials`) goes to the run's
/// recovery job at once instead of a nudge: the worker is refused what it
/// needs and does not get on (ADR-t813-1 decision 9).
pub const PERMISSION_DENIAL_LIMIT: usize = 3;

string_enum!(TurnOutcome {
    Succeeded => "succeeded",
    Failed => "failed",
    Silent => "silent",
    TimedOut => "timed_out",
    LaunchMismatch => "launch_mismatch",
    Stopped => "stopped",
});

string_enum!(TurnFailure {
    Authentication => "authentication",
    UsageLimit => "usage_limit",
    Model => "model",
    Other => "other",
});

impl TurnOutcome {
    /// Whether the run goes on after a turn that ended so: the session
    /// waits for its next request. A turn the runtime stopped (silent, past
    /// its limit, started otherwise than asked) or that failed for any
    /// reason but a login or a usage limit ends the session, and the run
    /// goes to its recovery job as a run that failed (ADR-t813-1 decision
    /// 9); one stopped by the exit request ends it too.
    pub fn goes_on(self, failure: Option<TurnFailure>) -> bool {
        match self {
            Self::Succeeded => true,
            Self::Failed => matches!(
                failure,
                Some(TurnFailure::Authentication | TurnFailure::UsageLimit)
            ),
            Self::Silent | Self::TimedOut | Self::LaunchMismatch | Self::Stopped => false,
        }
    }
}

/// The limits the wrapper holds a turn to: stopped after `silence_secs`
/// without a line of output (for an agent whose output has a heartbeat),
/// and after `limit_secs` in all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnLimits {
    pub silence_secs: i64,
    pub limit_secs: i64,
}

impl TurnLimits {
    /// The limits in `text` (the [`LIMITS_FILE`]), else `fallback`; a
    /// value that is not positive keeps the fallback's.
    pub fn parse_or(text: Option<&str>, fallback: Self) -> Self {
        let Some(value) = text.and_then(|text| serde_json::from_str::<Value>(text).ok()) else {
            return fallback;
        };
        let secs = |name: &str, default: i64| {
            value[name]
                .as_i64()
                .filter(|&secs| secs > 0)
                .unwrap_or(default)
        };
        Self {
            silence_secs: secs("silence_secs", fallback.silence_secs),
            limit_secs: secs("limit_secs", fallback.limit_secs),
        }
    }
}

/// What the supervisor asks the session for next: the prompt of the next
/// turn, and what it is (`answer of ask 3`, `revise request`, `nudge`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRequest {
    pub seq: u64,
    pub what: String,
    pub prompt: String,
}

/// The turns directory of the run whose directory is `run_dir`.
pub fn turns_dir(run_dir: &Path) -> PathBuf {
    run_dir.join(TURNS_DIR)
}

/// Where request `seq` waits to be taken.
pub fn request_path(run_dir: &Path, seq: u64) -> PathBuf {
    turns_dir(run_dir).join(format!("request-{seq:06}.json"))
}

/// Where request `seq` goes once the wrapper took it.
pub fn taken_path(run_dir: &Path, seq: u64) -> PathBuf {
    turns_dir(run_dir).join(format!("request-{seq:06}.taken.json"))
}

/// The stdout (`jsonl`) and stderr (`err`) of turn `turn`.
pub fn output_path(run_dir: &Path, turn: u64, what: &str) -> PathBuf {
    turns_dir(run_dir).join(format!("turn-{turn:06}.{what}"))
}

/// The exit request of the run whose directory is `run_dir`.
pub fn exit_path(run_dir: &Path) -> PathBuf {
    turns_dir(run_dir).join(EXIT_FILE)
}

/// The sequence number of a request file named `name`, and whether it was
/// taken; `None` for any other file.
pub fn request_seq(name: &str) -> Option<(u64, bool)> {
    let rest = name.strip_prefix("request-")?;
    // A request dropped with an earlier session is as good as taken.
    let (seq, taken) = match rest
        .strip_suffix(".taken.json")
        .or_else(|| rest.strip_suffix(".dropped"))
    {
        Some(seq) => (seq, true),
        None => (rest.strip_suffix(".json")?, false),
    };
    seq.parse().ok().map(|seq| (seq, taken))
}

/// The sequence number the next request gets, after every request named
/// in `names` (taken or not).
pub fn next_seq<'a>(names: impl IntoIterator<Item = &'a str>) -> u64 {
    names
        .into_iter()
        .filter_map(request_seq)
        .map(|(seq, _)| seq)
        .max()
        .unwrap_or(0)
        + 1
}

/// The requests named in `names` that wait to be taken, oldest first.
pub fn pending<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<u64> {
    let mut pending: Vec<u64> = names
        .into_iter()
        .filter_map(request_seq)
        .filter(|(_, taken)| !taken)
        .map(|(seq, _)| seq)
        .collect();
    pending.sort_unstable();
    pending
}

/// What one line of a turn's output said, as the runtime reads it whatever
/// the provider.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnSignal {
    /// The agent started the turn: its session, model and permission mode
    /// when the provider says them (ADR-t813-1 decision 8).
    Started {
        session_id: Option<String>,
        model: Option<String>,
        permission_mode: Option<String>,
    },
    /// The provider cannot be used: its login ran out or a usage limit was
    /// hit. The turn is stopped at once rather than waited out.
    Unusable(TurnFailure, String),
    /// A text of the agent's, for the terminal.
    Said(String),
    /// A tool the agent called, for the terminal.
    Tool(String),
}

/// How a turn ended, as the output and the process's exit say.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TurnResult {
    /// The output ended with the agent's result.
    pub result_seen: bool,
    pub is_error: bool,
    /// Why it failed, when it did.
    pub failure: Option<TurnFailure>,
    /// The result's or the error's text, shortened.
    pub message: Option<String>,
    pub session_id: Option<String>,
    /// The agent's session has a conversation to resume: it answered at
    /// least once.
    pub session_created: bool,
    pub num_turns: Option<u64>,
    pub duration_ms: Option<u64>,
    pub cost_usd: Option<f64>,
    pub usage: Value,
    /// The tools refused a permission, one entry per refusal.
    pub permission_denials: Vec<String>,
}

/// The idle marker the wrapper writes when turn `turn` ended: a `Stop`
/// shaped object (no background task: a headless turn's background shells
/// end with it) with the turn's outcome in `dagq_turn`.
pub fn idle_marker(
    session_id: &str,
    turn: u64,
    outcome: TurnOutcome,
    result: &TurnResult,
) -> Value {
    json!({
        "hook_event_name": "Stop",
        "session_id": session_id,
        "stop_hook_active": false,
        "dagq_turn": {
            "turn": turn,
            "outcome": outcome,
            "failure": result.failure,
            "permission_denials": result.permission_denials.len(),
        },
    })
}

/// What an idle marker says of the turn that wrote it; `None` for a
/// marker a headless turn did not write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnMark {
    pub turn: u64,
    pub outcome: TurnOutcome,
    pub failure: Option<TurnFailure>,
    pub permission_denials: usize,
}

impl TurnMark {
    pub fn parse(content: &[u8]) -> Option<Self> {
        let value: Value = serde_json::from_slice(content).ok()?;
        let turn = value.get("dagq_turn")?;
        Some(Self {
            turn: turn["turn"].as_u64().unwrap_or(0),
            outcome: turn["outcome"].as_str()?.parse().ok()?,
            failure: turn["failure"].as_str().and_then(|f| f.parse().ok()),
            permission_denials: turn["permission_denials"].as_u64().unwrap_or(0) as usize,
        })
    }
}

/// `text` cut to at most `max` characters, with `…` when cut.
pub fn shortened(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

impl std::fmt::Display for TurnOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_numbered_and_taken_in_order() {
        let names = [
            "request-000002.json",
            "request-000001.taken.json",
            "request-000003.json",
            "request-000004.dropped",
            "turn-000001.jsonl",
            "exit",
        ];
        assert_eq!(next_seq(names), 5);
        assert_eq!(pending(names), [2, 3]);
        assert_eq!(next_seq([]), 1);
        assert_eq!(request_seq("request-x.json"), None);
        let dir = Path::new("/r");
        assert_eq!(
            request_path(dir, 7),
            Path::new("/r/turns/request-000007.json")
        );
        assert_eq!(
            taken_path(dir, 7),
            Path::new("/r/turns/request-000007.taken.json")
        );
        assert_eq!(
            output_path(dir, 2, "jsonl"),
            Path::new("/r/turns/turn-000002.jsonl")
        );
        assert_eq!(exit_path(dir), Path::new("/r/turns/exit"));
    }

    #[test]
    fn a_turn_goes_on_unless_it_was_stopped_or_failed_otherwise_than_at_the_provider() {
        assert!(TurnOutcome::Succeeded.goes_on(None));
        assert!(TurnOutcome::Failed.goes_on(Some(TurnFailure::Authentication)));
        assert!(TurnOutcome::Failed.goes_on(Some(TurnFailure::UsageLimit)));
        assert!(!TurnOutcome::Failed.goes_on(Some(TurnFailure::Model)));
        assert!(!TurnOutcome::Failed.goes_on(None));
        for stopped in [
            TurnOutcome::Silent,
            TurnOutcome::TimedOut,
            TurnOutcome::LaunchMismatch,
            TurnOutcome::Stopped,
        ] {
            assert!(!stopped.goes_on(None), "{stopped}");
        }
    }

    #[test]
    fn the_idle_marker_carries_the_turn() {
        let result = TurnResult {
            failure: Some(TurnFailure::Authentication),
            permission_denials: vec!["Bash".into(), "Edit".into()],
            ..TurnResult::default()
        };
        let marker = idle_marker("s1", 3, TurnOutcome::Failed, &result);
        assert_eq!(marker["hook_event_name"], "Stop");
        let mark = TurnMark::parse(marker.to_string().as_bytes()).unwrap();
        assert_eq!(
            mark,
            TurnMark {
                turn: 3,
                outcome: TurnOutcome::Failed,
                failure: Some(TurnFailure::Authentication),
                permission_denials: 2,
            }
        );
        assert_eq!(TurnMark::parse(br#"{"hook_event_name":"Stop"}"#), None);
        assert_eq!(TurnMark::parse(b"not json"), None);
    }

    #[test]
    fn limits_are_read_with_their_fallback() {
        let fallback = TurnLimits {
            silence_secs: 900,
            limit_secs: 14400,
        };
        assert_eq!(TurnLimits::parse_or(None, fallback), fallback);
        assert_eq!(TurnLimits::parse_or(Some("oops"), fallback), fallback);
        assert_eq!(
            TurnLimits::parse_or(Some(r#"{"silence_secs": 2, "limit_secs": 0}"#), fallback),
            TurnLimits {
                silence_secs: 2,
                limit_secs: 14400
            }
        );
    }

    #[test]
    fn text_is_shortened_by_characters() {
        assert_eq!(shortened("  héllo  ", 10), "héllo");
        assert_eq!(shortened("héllo world", 5), "héllo…");
        assert_eq!("model".parse::<TurnFailure>().unwrap(), TurnFailure::Model);
        assert!("x".parse::<TurnFailure>().is_err());
    }
}
