//! The turns of a headless worker (ADR-t813-1): one non-interactive call of
//! the agent per turn, its output read into the provider-neutral
//! [`TurnSignal`]s and [`TurnResult`] below, and the files in the run's
//! `turns/` directory the supervisor and the session wrapper talk through.
//! The supervisor writes a request ([`TurnRequest`]) where it would type
//! into an interactive session, and the exit request where it would type
//! `/exit`; the wrapper takes each request in order and runs it as the
//! next turn (a resume of the same session), and writes the run's idle
//! marker when the turn's process ended ([`idle_marker`]).

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    DomainError, RunEvent,
    event_kind::{TURN_FINISHED, TURN_STARTED},
    stats::timestamp_millis,
    tokens::TokenUsage,
    transcript::Turn,
};

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

// Why a turn failed; `launch`: the agent did not start (its executable
// could not be run, or it ended with nothing on its output, ADR-t813-2
// decision 2).
string_enum!(TurnFailure {
    Authentication => "authentication",
    UsageLimit => "usage_limit",
    Model => "model",
    Sandbox => "sandbox",
    Launch => "launch",
    Other => "other",
});

impl TurnOutcome {
    /// Whether the run goes on after a turn that ended so: the session
    /// waits for its next request. A turn the runtime stopped (silent, past
    /// its limit, started otherwise than asked) or that failed for any
    /// reason but a login, a usage limit or an agent that did not start
    /// ends the session, and the run goes to its recovery job as a run that
    /// failed (ADR-t813-1 decision 9); one stopped by the exit request ends
    /// it too. After those three the supervisor moves the worker to the
    /// other provider or holds it (ADR-t813-2).
    pub fn goes_on(self, failure: Option<TurnFailure>) -> bool {
        match self {
            Self::Succeeded => true,
            Self::Failed => matches!(
                failure,
                Some(TurnFailure::Authentication | TurnFailure::UsageLimit | TurnFailure::Launch)
            ),
            Self::Silent | Self::TimedOut | Self::LaunchMismatch | Self::Stopped => false,
        }
    }
}

/// The limits the wrapper holds a turn to: stopped after `silence_secs`
/// without a line of output (for an agent whose output has a heartbeat),
/// and after `limit_secs` in all. A test may set either in milliseconds
/// (`silence_ms`, `limit_ms`; [`super::stall::StallConfig::with_millis`]),
/// which then stand in for the seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnLimits {
    pub silence_secs: i64,
    pub limit_secs: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub silence_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_ms: Option<u64>,
}

impl TurnLimits {
    /// The limits in `text` (the [`LIMITS_FILE`]), else `fallback`; a
    /// value that is not positive keeps the fallback's, and a limit read
    /// in seconds is in milliseconds only when `text` says so too.
    pub fn parse_or(text: Option<&str>, fallback: Self) -> Self {
        let Some(value) = text.and_then(|text| serde_json::from_str::<Value>(text).ok()) else {
            return fallback;
        };
        let limit = |name: &str, ms_name: &str, default: i64, default_ms: Option<u64>| match value
            [name]
            .as_i64()
            .filter(|&secs| secs > 0)
        {
            Some(secs) => (secs, value[ms_name].as_u64().filter(|&ms| ms > 0)),
            None => (default, default_ms),
        };
        let (silence_secs, silence_ms) = limit(
            "silence_secs",
            "silence_ms",
            fallback.silence_secs,
            fallback.silence_ms,
        );
        let (limit_secs, limit_ms) = limit(
            "limit_secs",
            "limit_ms",
            fallback.limit_secs,
            fallback.limit_ms,
        );
        Self {
            silence_secs,
            limit_secs,
            silence_ms,
            limit_ms,
        }
    }

    /// How long a turn may go without a line of output.
    pub fn silence(&self) -> Duration {
        duration(self.silence_secs, self.silence_ms)
    }

    /// How long a turn may run in all.
    pub fn limit(&self) -> Duration {
        duration(self.limit_secs, self.limit_ms)
    }
}

fn duration(secs: i64, ms: Option<u64>) -> Duration {
    ms.map_or_else(
        || Duration::from_secs(u64::try_from(secs).unwrap_or(0)),
        Duration::from_millis,
    )
}

/// What the supervisor asks the session for next: the prompt of the next
/// turn, and what it is (`answer of ask 3`, `revise request`, `nudge`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRequest {
    pub seq: u64,
    pub what: String,
    pub prompt: String,
}

/// The session a turn runs in: a new one (named `name` for a provider that
/// takes the name, Claude; Codex names its own), or the one of that id
/// resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnSession<'a> {
    New(&'a str),
    Resume(&'a str),
}

impl<'a> TurnSession<'a> {
    /// The session resumed, if one is.
    pub fn resumed(self) -> Option<&'a str> {
        match self {
            Self::New(_) => None,
            Self::Resume(id) => Some(id),
        }
    }
}

/// The name of the session a provider that takes one (Claude) gives the
/// run `run_id` after `switches` switches of its provider (ADR-t813-2
/// decision 4): the run's id until the first switch, then a UUID of its own
/// for each, since Claude Code refuses a session id in use and the session
/// after a switch is a new one.
pub fn session_name(run_id: &str, switches: usize) -> String {
    if switches == 0 {
        return run_id.to_owned();
    }
    uuid_of(&format!("{run_id}/{switches}"))
}

/// The name of the session of the headless planner opened at `created_at`
/// (Unix seconds) whose directory is `dir` (ADR-t1394-2 decision 2): a
/// UUID of its own, which a planner of the same ID in a queue made again
/// does not share.
pub fn planner_session_name(dir: &str, created_at: i64) -> String {
    uuid_of(&format!("planner/{dir}/{created_at}"))
}

/// A version-4 shaped UUID made from `seed`.
fn uuid_of(seed: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(seed.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // A version-4 shaped UUID (the variant and version bits set).
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
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
    /// `cost_usd` (and the cost of `tokens`) is the running total of the
    /// session so far rather than the turn's own (Claude's result's
    /// `total_cost_usd`, task 1199): the wrapper records the turn's own as
    /// what it adds to the total the session's last turn recorded.
    #[serde(skip)]
    pub cost_cumulative: bool,
    pub usage: Value,
    /// The tools refused a permission, one entry per refusal.
    pub permission_denials: Vec<String>,
    /// The tokens of the turn in the runtime's own kinds (ADR-t813-2
    /// decision 7), when the provider's usage could be read: what
    /// `turn_finished` records as `tokens` and the session span sums.
    #[serde(skip)]
    pub tokens: Option<TokenUsage>,
    /// `tokens` is the running total of the session so far rather than the
    /// turn's own (Codex's `turn.completed` carries the thread's total):
    /// the wrapper records the turn's own as what it adds to the total the
    /// session's last turn recorded.
    #[serde(skip)]
    pub tokens_cumulative: bool,
    /// The turn resumed a session the agent does not have (Codex's `no
    /// rollout found`): it did nothing, and a new session is started.
    pub session_missing: bool,
    /// The model the agent says the turn ran on (Claude's `system/init`,
    /// Codex's rollout), and why none was read when it was not.
    pub model: Option<String>,
    pub model_unknown: Option<String>,
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

/// The own cost of turn `turn` of the session `session`, whose agent gave
/// `total`, the session's running total so far (Claude's `total_cost_usd`,
/// task 1199), from the run's `events` up to its `turn_started`: when the
/// turn resumed the session, `total` less the total the session's last
/// turn with one recorded (`session_cost_usd`, or `cost_usd` for a turn
/// recorded before `session_cost_usd` was, when that was the total),
/// rounded to a millionth of a dollar. The whole `total` for a turn that
/// started its session, and when no earlier total was recorded or `total`
/// is below it (the session is not the one the earlier turn ran in).
pub fn turn_own_cost(events: &[RunEvent], turn: u64, session: &str, total: f64) -> f64 {
    let resumed = events
        .iter()
        .rfind(|e| e.kind == TURN_STARTED && e.payload["turn"].as_u64() == Some(turn))
        .is_some_and(|e| e.payload["resume"] == true);
    if !resumed {
        return total;
    }
    let earlier = events
        .iter()
        .rev()
        .filter(|e| e.kind == TURN_FINISHED && e.payload["session_id"].as_str() == Some(session))
        .find_map(|e| match e.payload.get("session_cost_usd") {
            Some(recorded) => recorded.as_f64(),
            None => e.payload["cost_usd"].as_f64(),
        });
    match earlier {
        Some(earlier) if total >= earlier => ((total - earlier) * 1e6).round() / 1e6,
        _ => total,
    }
}

/// The turns of a headless session span and the tokens they used, from
/// its run's `turn_started` and `turn_finished` events after it opened
/// (ADR-t813-2 decision 7): the turns that finished, the start of one still
/// running, and the `tokens` of the finished turns summed (`messages` is
/// the number of turns that recorded tokens; `cost_usd` only when each of
/// them had one). `None` for the tokens when no turn recorded them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeadlessSpan {
    pub finished: Vec<Turn>,
    pub running: Option<i64>,
    pub tokens: Option<TokenUsage>,
}

impl HeadlessSpan {
    /// `events` in ascending id; other kinds are skipped. A `turn_finished`
    /// with no `turn_started` before it (the span opened in the middle of
    /// the turn) is counted from the span's `start`.
    pub fn of(events: &[RunEvent], start: i64) -> Self {
        let mut span = Self::default();
        let mut tokens = TokenUsage::default();
        let mut costs = Some(0.0);
        for event in events {
            let Some(at) = timestamp_millis(&event.created_at) else {
                continue;
            };
            match event.kind.as_str() {
                TURN_STARTED => span.running = Some(at.max(start)),
                TURN_FINISHED => {
                    let begun = span.running.take().unwrap_or(start);
                    span.finished.push(Turn {
                        start: begun,
                        end: at.max(begun),
                    });
                    let used = &event.payload["tokens"];
                    if !used.is_object() {
                        continue;
                    }
                    let count = |key: &str| used[key].as_i64().unwrap_or(0);
                    tokens.input += count("input");
                    tokens.output += count("output");
                    tokens.cache_read += count("cache_read");
                    tokens.cache_creation += count("cache_creation");
                    tokens.messages += 1;
                    costs = costs.zip(used["cost_usd"].as_f64()).map(|(a, b)| a + b);
                }
                _ => {}
            }
        }
        if tokens.messages > 0 {
            tokens.cost_usd = costs;
            span.tokens = Some(tokens);
        }
        span
    }

    /// The turns not in `recorded` yet: the finished ones, and the one
    /// running cut at `now` when given (a span closed on a known end; one
    /// closed as inferred leaves it out, its end unknown). A turn is new
    /// when it starts at or after the end of the last one recorded (turns
    /// do not overlap; one may start in the millisecond the last ended).
    pub fn new_turns(&self, recorded: &[Turn], now: Option<i64>) -> Vec<Turn> {
        let through = recorded.iter().map(|turn| turn.end).max();
        let running = self.running.zip(now).map(|(start, now)| Turn {
            start,
            end: now.max(start),
        });
        self.finished
            .iter()
            .copied()
            .chain(running)
            .filter(|turn| {
                !recorded.contains(turn) && through.is_none_or(|through| turn.start >= through)
            })
            .collect()
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
    fn a_session_is_named_by_the_run_until_its_provider_switched() {
        assert_eq!(session_name("run-1", 0), "run-1");
        let first = session_name("run-1", 1);
        let second = session_name("run-1", 2);
        assert_ne!(first, second);
        assert_eq!(first, session_name("run-1", 1));
        assert_eq!(first.len(), 36);
        assert_eq!(&first[14..15], "4");
        assert!(first.split('-').map(str::len).eq([8, 4, 4, 4, 12]));
        assert_eq!(TurnSession::New("a").resumed(), None);
        assert_eq!(TurnSession::Resume("a").resumed(), Some("a"));
    }

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
        assert!(TurnOutcome::Failed.goes_on(Some(TurnFailure::Launch)));
        assert!(!TurnOutcome::Failed.goes_on(Some(TurnFailure::Model)));
        assert!(!TurnOutcome::Failed.goes_on(Some(TurnFailure::Sandbox)));
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
            silence_ms: None,
            limit_ms: Some(300),
        };
        assert_eq!(TurnLimits::parse_or(None, fallback), fallback);
        assert_eq!(TurnLimits::parse_or(Some("oops"), fallback), fallback);
        assert_eq!(
            TurnLimits::parse_or(Some(r#"{"silence_secs": 2, "limit_secs": 0}"#), fallback),
            TurnLimits {
                silence_secs: 2,
                limit_secs: 14400,
                silence_ms: None,
                limit_ms: Some(300),
            }
        );
    }

    /// A limit set in milliseconds (task 1045) reaches the wrapper through
    /// the limits file and stands in for its seconds.
    #[test]
    fn limits_in_milliseconds_round_trip_and_stand_in_for_seconds() {
        let limits = crate::domain::stall::StallConfig::default()
            .with_millis("turn_silence_secs", 200)
            .turn_limits();
        assert_eq!(limits.silence_secs, 1);
        assert_eq!(limits.silence(), Duration::from_millis(200));
        assert_eq!(limits.limit(), Duration::from_secs(4 * 60 * 60));
        let text = serde_json::to_string(&limits).unwrap();
        assert!(!text.contains("limit_ms"), "{text}");
        let fallback = crate::domain::stall::StallConfig::default().turn_limits();
        assert_eq!(TurnLimits::parse_or(Some(&text), fallback), limits);
        assert_eq!(
            TurnLimits::parse_or(Some(r#"{"silence_secs": 1, "silence_ms": 0}"#), fallback)
                .silence(),
            Duration::from_secs(1)
        );
    }

    /// A resumed turn's cost is the session's total less the one its last
    /// turn recorded (or that turn's `cost_usd`, recorded before
    /// `session_cost_usd` was); a turn that started its session, one
    /// without an earlier total and one whose total fell below it keep the
    /// total (task 1199).
    #[test]
    fn a_resumed_turn_costs_what_it_added_to_the_sessions_total() {
        let event = |kind: &str, payload: Value| RunEvent {
            id: super::super::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
            actor: None,
        };
        let started =
            |turn: u64, resume: bool| event(TURN_STARTED, json!({"turn": turn, "resume": resume}));
        let first = [started(1, false)];
        assert_eq!(turn_own_cost(&first, 1, "s", 4.7212), 4.7212);
        let resumed = [
            started(1, false),
            event(
                TURN_FINISHED,
                json!({"turn": 1, "session_id": "s", "cost_usd": 4.7212, "session_cost_usd": 4.7212}),
            ),
            // Another session's turn is not the session's.
            started(2, false),
            event(
                TURN_FINISHED,
                json!({"turn": 2, "session_id": "thread", "cost_usd": null}),
            ),
            started(3, true),
        ];
        assert_eq!(turn_own_cost(&resumed, 3, "s", 6.0195), 1.2983);
        // A total below the earlier one is a session of its own.
        assert_eq!(turn_own_cost(&resumed, 3, "s", 0.5), 0.5);
        // The turn started a session: its whole total.
        assert_eq!(turn_own_cost(&resumed, 2, "s", 0.5), 0.5);
        // Back on Claude after a switch, a turn starts a session of a new
        // name: its whole total, whatever the earlier session recorded.
        let switched_back = [
            started(1, false),
            event(
                TURN_FINISHED,
                json!({"turn": 1, "session_id": "s", "cost_usd": 4.0, "session_cost_usd": 4.0}),
            ),
            started(2, false),
            event(
                TURN_FINISHED,
                json!({"turn": 2, "session_id": "thread", "cost_usd": null}),
            ),
            started(3, false),
        ];
        assert_eq!(turn_own_cost(&switched_back, 3, "s2", 1.5), 1.5);
        // No earlier turn of the session with a total.
        let without = [
            event(
                TURN_FINISHED,
                json!({"turn": 1, "session_id": "s", "cost_usd": null, "session_cost_usd": null}),
            ),
            started(2, true),
        ];
        assert_eq!(turn_own_cost(&without, 2, "s", 2.0), 2.0);
        assert_eq!(turn_own_cost(&[started(2, true)], 2, "s", 2.0), 2.0);
        // A turn recorded before `session_cost_usd` was: its `cost_usd` was
        // the session's total; a later one without a total is skipped.
        let older = [
            event(
                TURN_FINISHED,
                json!({"turn": 1, "session_id": "s", "cost_usd": 4.0}),
            ),
            event(
                TURN_FINISHED,
                json!({"turn": 2, "session_id": "s", "cost_usd": null, "session_cost_usd": null}),
            ),
            started(3, true),
        ];
        assert_eq!(turn_own_cost(&older, 3, "s", 5.5), 1.5);
    }

    /// A headless span's turns run from each `turn_started` to its
    /// `turn_finished`, one still running to the close unless it was
    /// inferred, only those not recorded yet are new, and their
    /// tokens are summed with the cost only when every turn had one.
    #[test]
    fn a_headless_span_sums_its_turns_and_tokens() {
        let event = |id: i64, kind: &str, secs: i64, payload: Value| RunEvent {
            id: super::super::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: super::super::transcript::millis_text(secs * 1000),
            actor: None,
        };
        let tokens = |input: i64, cost: Option<f64>| {
            let mut tokens = json!({"input": input, "output": 2, "cache_read": 3, "cache_creation": 4, "messages": 1});
            if let Some(cost) = cost {
                tokens["cost_usd"] = json!(cost);
            }
            json!({"turn": 1, "tokens": tokens})
        };
        let events = [
            // Finished before a start was seen: counted from the span's.
            event(1, TURN_FINISHED, 110, tokens(10, Some(0.5))),
            event(2, "turn_requested", 115, json!({})),
            event(3, TURN_STARTED, 120, json!({"turn": 2})),
            event(4, TURN_FINISHED, 150, tokens(20, Some(0.25))),
            event(5, TURN_STARTED, 160, json!({"turn": 3})),
            event(6, TURN_FINISHED, 170, json!({"turn": 3, "tokens": null})),
            event(7, TURN_STARTED, 200, json!({"turn": 4})),
        ];
        let span = HeadlessSpan::of(&events, 100 * 1000);
        assert_eq!(
            span.new_turns(&[], Some(230 * 1000)),
            [
                Turn {
                    start: 100_000,
                    end: 110_000
                },
                Turn {
                    start: 120_000,
                    end: 150_000
                },
                Turn {
                    start: 160_000,
                    end: 170_000
                },
                Turn {
                    start: 200_000,
                    end: 230_000
                },
            ]
        );
        assert_eq!(span.finished.len(), 3);
        // Closed as inferred: the running turn's end is unknown.
        assert_eq!(span.new_turns(&[], None).len(), 3);
        // What was recorded is not again; a turn that starts in the
        // millisecond the last recorded one ended is new.
        let recorded = [span.finished[0], span.finished[1]];
        assert_eq!(span.new_turns(&recorded, None), [span.finished[2]]);
        let touching = HeadlessSpan {
            finished: vec![Turn { start: 0, end: 10 }, Turn { start: 10, end: 20 }],
            ..HeadlessSpan::default()
        };
        assert_eq!(
            touching.new_turns(&[Turn { start: 0, end: 10 }], None),
            [Turn { start: 10, end: 20 }]
        );
        assert_eq!(
            span.tokens.as_ref().map(TokenUsage::payload),
            Some(
                json!({"input": 30, "output": 4, "cache_read": 6, "cache_creation": 8,
                        "messages": 2, "cost_usd": 0.75})
            )
        );
        // A turn without a cost leaves the sum without one.
        let span = HeadlessSpan::of(
            &[
                event(1, TURN_FINISHED, 110, tokens(10, Some(0.5))),
                event(2, TURN_FINISHED, 120, tokens(10, None)),
            ],
            0,
        );
        assert_eq!(span.tokens.unwrap().cost_usd, None);
        assert_eq!(HeadlessSpan::of(&[], 0), HeadlessSpan::default());
    }

    #[test]
    fn text_is_shortened_by_characters() {
        assert_eq!(shortened("  héllo  ", 10), "héllo");
        assert_eq!(shortened("héllo world", 5), "héllo…");
        assert_eq!("model".parse::<TurnFailure>().unwrap(), TurnFailure::Model);
        assert!("x".parse::<TurnFailure>().is_err());
    }
}
