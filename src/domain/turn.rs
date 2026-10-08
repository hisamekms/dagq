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
    event_kind::{TURN_FINISHED, TURN_REQUESTED, TURN_STARTED},
    stats::timestamp_millis,
    tokens::{ModelTokens, RolloutUsage, TokenSource, TokenUsage},
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
            Self::Failed => failure.is_some_and(TurnFailure::at_provider_wall),
            Self::Silent | Self::TimedOut | Self::LaunchMismatch | Self::Stopped => false,
        }
    }
}

impl TurnFailure {
    /// Whether a turn that failed so met the provider's wall (its login,
    /// its usage limit, an agent that did not start): the session waits
    /// for the provider rather than ending.
    pub fn at_provider_wall(self) -> bool {
        matches!(self, Self::Authentication | Self::UsageLimit | Self::Launch)
    }
}

/// What a request that makes a call failed at the provider's wall again,
/// on the same provider once its hold ended, is called (`turn_requested`'s
/// `what`).
pub const PROVIDER_RETRY: &str = "provider retry";

/// Whether a turn that ended with `outcome` and `failure` met the
/// provider's wall.
fn walled(outcome: Option<&str>, failure: Option<&str>) -> bool {
    outcome == Some(TurnOutcome::Failed.as_str())
        && failure
            .and_then(|failure| failure.parse::<TurnFailure>().ok())
            .is_some_and(TurnFailure::at_provider_wall)
}

/// The request a headless planner's wrapper takes next of the `pending`
/// ones (oldest first, each with its `what`). After a turn that met the
/// provider's wall (`after_wall`) it takes none but the `provider retry`
/// the supervisor writes once Claude can be used again, so that the
/// request the failed turn took is made again before any request that
/// waited behind it (a revise, an answer, a `planner request` written
/// while the turn ran), and none of those runs against a Claude that
/// cannot be used (task 1596); the ones that waited are taken after it,
/// in order.
pub fn request_to_take<'a>(
    pending: impl IntoIterator<Item = (u64, &'a str)>,
    after_wall: bool,
) -> Option<u64> {
    pending
        .into_iter()
        .find(|(_, what)| !after_wall || *what == PROVIDER_RETRY)
        .map(|(seq, _)| seq)
}

/// Whether a headless planner whose idle marker is newer than its last
/// input is idle: no request waits in its `turns/` (`waiting`), or the
/// turn the marker is of (`mark`) met the provider's wall, after which the
/// wrapper takes nothing before the `provider retry` and the planner waits
/// at the wall for the supervisor's hold (task 1596).
pub fn idle_after_turn(mark: Option<&TurnMark>, waiting: bool) -> bool {
    !waiting || mark.is_some_and(TurnMark::at_provider_wall)
}

/// Whether the wall a headless planner's last turn met is answered: a
/// `provider retry` was requested among the `events` after that turn's
/// `turn_finished`. Another request written after it waits behind the
/// retry (the wrapper takes the retry first), so it is no answer.
pub fn wall_answered(events: &[RunEvent]) -> bool {
    events
        .iter()
        .any(|e| e.kind == TURN_REQUESTED && e.payload["what"] == PROVIDER_RETRY)
}

/// Whether a headless planner's wrapper waiting at the provider's wall
/// renews its idle marker (written at `marker`, `None` without one): an
/// input stamped after it (`last_input`) with no `provider retry` waiting
/// (`retry_waiting`) is a request the wrapper does not take before the
/// retry, or a retry whose write failed after its stamp, and leaves the
/// planner at work by its view while nothing runs, so that the wall would
/// never be tended (task 1596). Renewed, the planner is idle at the wall
/// again.
pub fn renew_wall_marker(
    retry_waiting: bool,
    marker: Option<std::time::SystemTime>,
    last_input: std::time::SystemTime,
) -> bool {
    !retry_waiting && marker.is_some_and(|marker| marker < last_input)
}

/// The request a `provider retry` after the last turn of a headless
/// planner's `events` makes again: the one that turn took, or, when that
/// turn was itself a `provider retry` (it met the wall again), the one the
/// turn before it took, back to the call that first met the wall, so that
/// a retry carries the call itself rather than a retry of a retry. `None`
/// when that turn took no request (the initial prompt).
pub fn request_to_retry(events: &[RunEvent]) -> Option<u64> {
    let mut started = events.iter().rev().filter(|e| e.kind == TURN_STARTED);
    loop {
        let turn = started.next()?;
        if turn.payload["what"] != PROVIDER_RETRY {
            return turn.payload["request"].as_u64();
        }
    }
}

/// Whether the headless planner whose turns are `events` read the request
/// `seq` (the answer of its question): the turn that took it finished
/// otherwise than at the provider's wall, or it met the wall and the
/// `provider retry` turn the wrapper took next carried it to such an end.
/// A turn that met the wall did not get to its request, and a planner
/// ended then would lose the answer unread (task 1596).
pub fn request_read(events: &[RunEvent], seq: u64) -> bool {
    let Some(mut at) = events
        .iter()
        .position(|e| e.kind == TURN_STARTED && e.payload["request"] == seq)
    else {
        return false;
    };
    loop {
        let turn = &events[at].payload["turn"];
        let Some(finished) = events[at..]
            .iter()
            .position(|e| e.kind == TURN_FINISHED && e.payload["turn"] == *turn)
            .map(|offset| at + offset)
        else {
            return false;
        };
        let payload = &events[finished].payload;
        if !walled(payload["outcome"].as_str(), payload["failure"].as_str()) {
            return true;
        }
        let Some(next) = events[finished..]
            .iter()
            .position(|e| e.kind == TURN_STARTED)
            .map(|offset| finished + offset)
        else {
            return false;
        };
        if events[next].payload["what"] != PROVIDER_RETRY {
            return false;
        }
        at = next;
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

/// Where a request file stands: waiting to be taken, taken by the wrapper,
/// or dropped untaken when a later session started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    Pending,
    Taken,
    Dropped,
}

/// The sequence number and state of a request file named `name`; `None`
/// for any other file.
pub fn request_state(name: &str) -> Option<(u64, RequestState)> {
    let rest = name.strip_prefix("request-")?;
    let (seq, state) = if let Some(seq) = rest.strip_suffix(".taken.json") {
        (seq, RequestState::Taken)
    } else if let Some(seq) = rest.strip_suffix(".dropped") {
        (seq, RequestState::Dropped)
    } else {
        (rest.strip_suffix(".json")?, RequestState::Pending)
    };
    seq.parse().ok().map(|seq| (seq, state))
}

/// A request found in a session's `turns/`, with its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedRequest {
    pub state: RequestState,
    pub request: TurnRequest,
}

/// Whether an adopted revise or conflict request (`what`, whose text is
/// `text`) reached the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptedDelivery {
    /// It waits or was taken as request `seq`: it is not written again.
    Delivered(u64),
    /// It was recorded and the supervisor stopped before writing it: it is
    /// written once.
    Write,
}

/// Whether the request of an adopted attempt is among `requests`: one of
/// the same `what` and the attempt's `text`, numbered after `after` (the
/// last `turn_requested` recorded before the attempt's record, which an
/// earlier attempt's request cannot be numbered after), waiting or taken.
/// `what` is the same for every attempt, so it alone names none; a dropped
/// request never ran and does not count.
pub fn adopted_delivery(
    requests: &[ListedRequest],
    what: &str,
    text: &str,
    after: Option<u64>,
) -> AdoptedDelivery {
    requests
        .iter()
        .filter(|listed| listed.state != RequestState::Dropped)
        .map(|listed| &listed.request)
        .filter(|request| after.is_none_or(|after| request.seq > after))
        .find(|request| request.what == what && request.prompt == text)
        .map_or(AdoptedDelivery::Write, |request| {
            AdoptedDelivery::Delivered(request.seq)
        })
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
    /// `tokens` per model (Claude's `modelUsage`; the session's running
    /// totals when `tokens_cumulative`), empty when the provider gives
    /// none per model.
    #[serde(skip)]
    pub tokens_by_model: Vec<ModelTokens>,
    /// What `tokens` were counted from, and why none were or why a
    /// fallback was used ([`crate::domain::tokens::ExecutionTokens`]).
    #[serde(skip)]
    pub tokens_source: Option<TokenSource>,
    #[serde(skip)]
    pub tokens_reason: Option<&'static str>,
    /// The subagents or child threads the turn started, when the provider
    /// says (Claude's `subagent_stats.spawned`, the turn's own).
    #[serde(skip)]
    pub children: Option<i64>,
    /// What Codex's rollouts say of the turn, when they could be counted:
    /// the turn's tokens are taken from them, less the root turns the
    /// thread's earlier turns counted ([`counted_rollout_turns`]), and
    /// `tokens` is only the thread's running total, kept for a later turn
    /// whose rollout cannot be counted.
    #[serde(skip)]
    pub rollout: Option<RolloutUsage>,
    /// The turn resumed a session the agent does not have (Codex's `no
    /// rollout found`): it did nothing, and a new session is started.
    pub session_missing: bool,
    /// The model the agent says the turn ran on (Claude's `system/init`,
    /// Codex's rollout), and why none was read when it was not.
    pub model: Option<String>,
    pub model_unknown: Option<String>,
    /// The commands and tools the turn ran, with when the wrapper read
    /// their start and end, for the session's work breakdown: `Some` for a
    /// provider whose output names them (Codex's items) and that has no
    /// transcript the breakdown is read from, `None` for the others.
    #[serde(skip)]
    pub commands: Option<Vec<TurnCommand>>,
}

/// The extension of the file of a headless turn's commands
/// ([`commands_path`]).
pub const COMMANDS_FILE: &str = "commands.jsonl";

/// One item of a headless turn that ran a command or a tool (Codex's
/// `command_execution` or `mcp_tool_call`), as the wrapper read it: one
/// line of the turn's [`commands_path`]. Its output has no times, so its
/// `started` and `ended` are when the wrapper read the item's
/// `item.started` and `item.completed` (unix milliseconds); `None` when it
/// did not read that line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCommand {
    /// The item's id in the turn.
    pub id: String,
    /// The item's type: `command_execution` for a shell command.
    pub tool: String,
    /// The shell command of a `command_execution`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub started: Option<i64>,
    #[serde(default)]
    pub ended: Option<i64>,
    #[serde(default)]
    pub exit_code: Option<i64>,
    /// The item's `status` when it completed (`completed`, `failed`,
    /// `declined`).
    #[serde(default)]
    pub status: Option<String>,
    /// The tests its output names as failed (whatever the command; the
    /// breakdown keeps them only for one that runs tests).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_tests: Vec<String>,
}

/// The commands of turn `turn` of the run whose directory is `run_dir`,
/// one [`TurnCommand`] per line, which the wrapper writes before it
/// records the turn's end.
pub fn commands_path(run_dir: &Path, turn: u64) -> PathBuf {
    output_path(run_dir, turn, COMMANDS_FILE)
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
    /// Whether the turn met the provider's wall: it failed at a login, a
    /// usage limit or an agent that did not start.
    pub fn at_provider_wall(&self) -> bool {
        self.outcome == TurnOutcome::Failed
            && self.failure.is_some_and(TurnFailure::at_provider_wall)
    }

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
///
/// Turns recorded before `session_cost_usd` existed kept the running total
/// as a resumed turn's `cost_usd`; those events are not rewritten and
/// `stats` / `kpi` do not correct them when read, so a Claude headless
/// cost summed over that period counts a resumed run's earlier turns again.
pub fn turn_own_cost(events: &[RunEvent], turn: u64, session: &str, total: f64) -> f64 {
    if !resumed(events, turn) {
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

/// Whether turn `turn` resumed its session, as its `turn_started` says.
fn resumed(events: &[RunEvent], turn: u64) -> bool {
    events
        .iter()
        .rfind(|e| e.kind == TURN_STARTED && e.payload["turn"].as_u64() == Some(turn))
        .is_some_and(|e| e.payload["resume"] == true)
}

/// The own tokens per model of turn `turn` of the session `session`,
/// whose agent gave `totals`, the session's running totals so far
/// (Claude's `modelUsage`, ADR-t1486-1), taken as [`turn_own_cost`] takes
/// the cost: when the turn resumed the session, `totals` less those the
/// session's last turn with them recorded (`tokens_total_by_model`); the
/// whole `totals` for a turn that started its session, and when no earlier
/// totals were recorded or a count is below its earlier one (the session
/// is not the one the earlier turn ran in).
pub fn turn_own_models(
    events: &[RunEvent],
    turn: u64,
    session: &str,
    totals: &[ModelTokens],
) -> Vec<ModelTokens> {
    if !resumed(events, turn) {
        return totals.to_vec();
    }
    events
        .iter()
        .rev()
        .filter(|e| e.kind == TURN_FINISHED && e.payload["session_id"].as_str() == Some(session))
        .find_map(|e| ModelTokens::from_payloads(&e.payload["tokens_total_by_model"]))
        .and_then(|earlier| ModelTokens::since(totals, &earlier))
        .unwrap_or_else(|| totals.to_vec())
}

/// The root turns of Codex's thread `session` whose tokens the session's
/// turns counted (`tokens_turns`, ADR-t1486-1), which a turn that resumes
/// the thread does not count again. They are read from the run's events
/// only, so a wrapper that took over a run or started again finds what the
/// one before it counted.
pub fn counted_rollout_turns(events: &[RunEvent], session: &str) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.kind == TURN_FINISHED && e.payload["session_id"].as_str() == Some(session))
        .filter_map(|e| e.payload["tokens_turns"].as_array())
        .flatten()
        .filter_map(|turn| turn.as_str().map(str::to_owned))
        .collect()
}

/// The own tokens of a turn of Codex's thread `session` whose rollouts
/// could not be counted, from `total`, the thread's running total
/// (`turn.completed`, ADR-t813-2 decision 7): `total` less the total the
/// session's last turn with one recorded (`tokens_total`) and the `tokens`
/// of the session's turns after it that recorded no total (counted from
/// the rollouts though their `turn.completed` was not read), so that none
/// is counted twice; never below 0. Those turns' counts include their
/// child threads, which `total` leaves out, so with child threads this
/// may take too much. The whole `total` when nothing earlier
/// was recorded.
pub fn thread_total_own(events: &[RunEvent], session: &str, total: &TokenUsage) -> TokenUsage {
    let mut earlier: Option<TokenUsage> = None;
    let mut after = TokenUsage::default();
    for event in events
        .iter()
        .rev()
        .filter(|e| e.kind == TURN_FINISHED && e.payload["session_id"].as_str() == Some(session))
    {
        if let Some(recorded) = TokenUsage::from_payload(&event.payload["tokens_total"]) {
            earlier = Some(recorded);
            break;
        }
        if let Some(own) = TokenUsage::from_payload(&event.payload["tokens"]) {
            after.input += own.input;
            after.output += own.output;
            after.cache_read += own.cache_read;
            after.cache_creation += own.cache_creation;
            after.messages += 1;
        }
    }
    match earlier {
        None if after.messages == 0 => TokenUsage {
            messages: 1,
            ..total.clone()
        },
        earlier => {
            let earlier = earlier.unwrap_or_default();
            total.since(&TokenUsage {
                input: earlier.input + after.input,
                output: earlier.output + after.output,
                cache_read: earlier.cache_read + after.cache_read,
                cache_creation: earlier.cache_creation + after.cache_creation,
                ..TokenUsage::default()
            })
        }
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

/// The model most of the `turn_finished` among `events` said they ran on
/// (the earliest of those tied); `None` when none said one.
pub fn turns_model(events: &[RunEvent]) -> Option<String> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for model in events
        .iter()
        .filter(|event| event.kind == TURN_FINISHED)
        .filter_map(|event| event.payload["model"].as_str())
    {
        match counts.iter_mut().find(|(seen, _)| *seen == model) {
            Some((_, count)) => *count += 1,
            None => counts.push((model, 1)),
        }
    }
    let most = counts.iter().map(|(_, count)| *count).max()?;
    counts
        .into_iter()
        .find(|(_, count)| *count == most)
        .map(|(model, _)| model.to_owned())
}

/// `text` cut to at most `max` characters, with `…` when cut.
pub fn shortened(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// A finished Codex turn of the span has no file of its commands, or no
/// turn number to find it by: it ran before its wrapper wrote them (the
/// past), or the file could not be written (task 1354).
pub const TURN_COMMANDS_MISSING: &str = "turn_commands_missing";

/// A turn of a Codex span whose time and commands its work breakdown
/// counts (task 1354), from [`codex_span_turns`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexSpanTurn {
    /// When it started (unix milliseconds); `i64::MIN` when it began before
    /// the span opened, so that none of its commands before the span is
    /// the span's.
    pub start: i64,
    /// When it ended, or the span's end for one still running.
    pub end: i64,
    /// The turn whose commands file the breakdown reads; `None` for one
    /// still running at the close, whose commands are not written yet.
    pub commands_of: Option<u64>,
}

/// The turns of the Codex span `start`..`end` (unix milliseconds) whose
/// work breakdown counts them, from its run's turn `events` after it
/// opened, oldest first; other kinds are skipped. As its active time
/// counts them ([`HeadlessSpan`]): a turn started again without an end is
/// the new one; a `turn_finished` with no `turn_started` in the span began
/// before it; a turn still running is the model's to the end, its
/// commands not read, unless the close is `inferred`, whose end the turn's
/// is not known to reach. Only a turn that ran on Codex (its events name
/// Codex, or no provider) is kept: one that ran on another provider after
/// a switch writes no commands. A finished one with no turn number is
/// [`TURN_COMMANDS_MISSING`].
pub fn codex_span_turns(
    events: &[RunEvent],
    start: i64,
    end: i64,
    inferred: bool,
) -> Result<Vec<CodexSpanTurn>, &'static str> {
    struct Seen {
        number: Option<u64>,
        start: Option<i64>,
        end: Option<i64>,
        codex: bool,
    }
    let mut seen: Vec<Seen> = Vec::new();
    for event in events {
        let Some(at) = timestamp_millis(&event.created_at) else {
            continue;
        };
        let number = event.payload["turn"].as_u64();
        let codex = event.payload["provider"]
            .as_str()
            .is_none_or(|provider| provider == "codex");
        match event.kind.as_str() {
            TURN_STARTED => {
                if seen.last().is_some_and(|last| last.end.is_none()) {
                    seen.pop();
                }
                seen.push(Seen {
                    number,
                    start: Some(at.max(start)),
                    end: None,
                    codex,
                });
            }
            TURN_FINISHED => match seen.last_mut() {
                Some(last) if last.end.is_none() => {
                    last.end = Some(at.max(last.start.unwrap_or(start)));
                    last.codex &= codex;
                }
                _ => seen.push(Seen {
                    number,
                    start: None,
                    end: Some(at.max(start)),
                    codex,
                }),
            },
            _ => {}
        }
    }
    let mut turns = Vec::new();
    for turn in seen.into_iter().filter(|turn| turn.codex) {
        let from = turn.start.unwrap_or(i64::MIN);
        match turn.end {
            Some(to) => turns.push(CodexSpanTurn {
                start: from,
                end: to,
                commands_of: Some(turn.number.ok_or(TURN_COMMANDS_MISSING)?),
            }),
            None if inferred => {}
            None => turns.push(CodexSpanTurn {
                start: from,
                end: end.max(from),
                commands_of: None,
            }),
        }
    }
    Ok(turns)
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
    fn request_files_name_their_state() {
        assert_eq!(
            request_state("request-000002.json"),
            Some((2, RequestState::Pending))
        );
        assert_eq!(
            request_state("request-000001.taken.json"),
            Some((1, RequestState::Taken))
        );
        assert_eq!(
            request_state("request-000004.dropped"),
            Some((4, RequestState::Dropped))
        );
        assert_eq!(request_state("request-x.json"), None);
        assert_eq!(request_state("turn-000001.jsonl"), None);
        assert_eq!(request_state("exit"), None);
    }

    fn listed(seq: u64, state: RequestState, what: &str, prompt: &str) -> ListedRequest {
        ListedRequest {
            state,
            request: TurnRequest {
                seq,
                what: what.to_owned(),
                prompt: prompt.to_owned(),
            },
        }
    }

    #[test]
    fn an_adopted_request_is_matched_by_its_attempts_text_and_numbers() {
        const WHAT: &str = "revise request";
        let first = "revise 1: fix the test";
        let second = "revise 2: fix the docs";
        // Nothing written: write it.
        assert_eq!(
            adopted_delivery(&[], WHAT, second, Some(1)),
            AdoptedDelivery::Write
        );
        // An earlier attempt's request of the same what, in any state,
        // does not hold back the attempt's own.
        for state in [
            RequestState::Pending,
            RequestState::Taken,
            RequestState::Dropped,
        ] {
            assert_eq!(
                adopted_delivery(&[listed(1, state, WHAT, first)], WHAT, second, Some(1)),
                AdoptedDelivery::Write
            );
        }
        // Nor does one with the attempt's text numbered no later than the
        // last request recorded before the attempt (the same text sent by
        // an earlier attempt).
        assert_eq!(
            adopted_delivery(
                &[listed(1, RequestState::Taken, WHAT, second)],
                WHAT,
                second,
                Some(1)
            ),
            AdoptedDelivery::Write
        );
        // The attempt's request waiting or taken is not written twice.
        for state in [RequestState::Pending, RequestState::Taken] {
            assert_eq!(
                adopted_delivery(
                    &[
                        listed(1, RequestState::Taken, WHAT, first),
                        listed(2, state, WHAT, second)
                    ],
                    WHAT,
                    second,
                    Some(1)
                ),
                AdoptedDelivery::Delivered(2)
            );
        }
        // A dropped one never ran: it is written.
        assert_eq!(
            adopted_delivery(
                &[listed(2, RequestState::Dropped, WHAT, second)],
                WHAT,
                second,
                Some(1)
            ),
            AdoptedDelivery::Write
        );
        // Another what with the same text is not the attempt's request.
        assert_eq!(
            adopted_delivery(
                &[listed(2, RequestState::Pending, "nudge", second)],
                WHAT,
                second,
                Some(1)
            ),
            AdoptedDelivery::Write
        );
        // Without a request recorded before the attempt, no number is
        // ruled out.
        assert_eq!(
            adopted_delivery(
                &[listed(1, RequestState::Taken, WHAT, first)],
                WHAT,
                first,
                None
            ),
            AdoptedDelivery::Delivered(1)
        );
    }

    fn turn_event(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: super::super::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    fn started(turn: u64, request: Option<u64>, what: &str) -> RunEvent {
        turn_event(
            TURN_STARTED,
            json!({"turn": turn, "request": request, "what": what}),
        )
    }

    fn finished(turn: u64, failure: Option<&str>) -> RunEvent {
        let outcome = if failure.is_some() {
            "failed"
        } else {
            "succeeded"
        };
        turn_event(
            TURN_FINISHED,
            json!({"turn": turn, "outcome": outcome, "failure": failure}),
        )
    }

    #[test]
    fn after_a_wall_only_the_provider_retry_is_taken_and_the_rest_wait_behind_it() {
        let pending = [(2, "revise"), (3, "answer of ask 4"), (5, PROVIDER_RETRY)];
        assert_eq!(request_to_take(pending, false), Some(2));
        assert_eq!(request_to_take(pending, true), Some(5));
        assert_eq!(request_to_take(pending[..2].iter().copied(), true), None);
        assert_eq!(request_to_take([], false), None);
        for failure in [
            TurnFailure::Authentication,
            TurnFailure::UsageLimit,
            TurnFailure::Launch,
        ] {
            assert!(failure.at_provider_wall(), "{failure:?}");
        }
        for failure in [TurnFailure::Model, TurnFailure::Sandbox, TurnFailure::Other] {
            assert!(!failure.at_provider_wall(), "{failure:?}");
        }
    }

    #[test]
    fn a_planner_at_the_wall_is_idle_with_requests_waiting_and_otherwise_not() {
        let mark = |outcome, failure| TurnMark {
            turn: 2,
            outcome,
            failure,
            permission_denials: 0,
        };
        let walls = [
            TurnFailure::Authentication,
            TurnFailure::UsageLimit,
            TurnFailure::Launch,
        ];
        for failure in walls {
            let walled = mark(TurnOutcome::Failed, Some(failure));
            assert!(walled.at_provider_wall(), "{failure:?}");
            assert!(idle_after_turn(Some(&walled), true), "{failure:?}");
            assert!(idle_after_turn(Some(&walled), false), "{failure:?}");
        }
        for other in [
            mark(TurnOutcome::Succeeded, None),
            mark(TurnOutcome::Failed, Some(TurnFailure::Model)),
            mark(TurnOutcome::Failed, None),
            mark(TurnOutcome::TimedOut, Some(TurnFailure::UsageLimit)),
        ] {
            assert!(!other.at_provider_wall(), "{other:?}");
            assert!(!idle_after_turn(Some(&other), true), "{other:?}");
            assert!(idle_after_turn(Some(&other), false), "{other:?}");
        }
        assert!(!idle_after_turn(None, true));
        assert!(idle_after_turn(None, false));
    }

    #[test]
    fn only_a_provider_retry_after_the_turn_answers_its_wall() {
        let requested = |what: &str| turn_event(TURN_REQUESTED, json!({"seq": 3, "what": what}));
        assert!(!wall_answered(&[]));
        assert!(!wall_answered(&[requested("revise")]));
        assert!(!wall_answered(&[
            requested("follow-up request followup-1"),
            requested("answer of ask 4"),
        ]));
        assert!(wall_answered(&[
            requested("revise"),
            requested(PROVIDER_RETRY),
        ]));
        assert!(!wall_answered(&[turn_event(
            TURN_STARTED,
            json!({"turn": 3, "what": PROVIDER_RETRY}),
        )]));
    }

    #[test]
    fn a_marker_at_the_wall_is_renewed_after_a_later_stamp_until_the_retry_waits() {
        let at = |secs| std::time::UNIX_EPOCH + Duration::from_secs(secs);
        assert!(renew_wall_marker(false, Some(at(10)), at(11)));
        assert!(!renew_wall_marker(true, Some(at(10)), at(11)), "the retry");
        assert!(!renew_wall_marker(false, Some(at(11)), at(11)), "not stale");
        assert!(!renew_wall_marker(false, Some(at(12)), at(11)));
        assert!(!renew_wall_marker(false, None, at(11)), "no marker");
    }

    #[test]
    fn a_retry_makes_again_the_call_that_first_met_the_wall_not_a_retry_of_it() {
        let first = [
            started(1, None, "initial prompt"),
            finished(1, None),
            started(2, Some(1), "answer of ask 3"),
            finished(2, Some("usage_limit")),
        ];
        assert_eq!(request_to_retry(&first), Some(1));
        let again = [
            first.to_vec(),
            vec![
                started(3, Some(4), PROVIDER_RETRY),
                finished(3, Some("authentication")),
            ],
        ]
        .concat();
        assert_eq!(request_to_retry(&again), Some(1));
        assert_eq!(
            request_to_retry(&[
                started(1, None, "initial prompt"),
                finished(1, Some("launch"))
            ]),
            None
        );
        assert_eq!(request_to_retry(&[]), None);
    }

    #[test]
    fn an_answer_is_read_only_once_a_turn_carrying_it_ends_otherwise_than_at_the_wall() {
        let walled = vec![
            started(1, Some(2), "answer of ask 3"),
            finished(1, Some("usage_limit")),
        ];
        assert!(!request_read(&walled, 2));
        let retried = [walled.clone(), vec![started(2, Some(4), PROVIDER_RETRY)]].concat();
        assert!(!request_read(&retried, 2), "the retry runs");
        let again = [retried.clone(), vec![finished(2, Some("launch"))]].concat();
        assert!(!request_read(&again, 2), "the retry met the wall too");
        let read = [
            again.clone(),
            vec![started(3, Some(5), PROVIDER_RETRY), finished(3, None)],
        ]
        .concat();
        assert!(request_read(&read, 2));
        let other = [
            walled.clone(),
            vec![started(2, Some(4), "revise"), finished(2, None)],
        ]
        .concat();
        assert!(!request_read(&other, 2), "a turn that did not carry it");
        assert!(request_read(
            &[started(1, Some(2), "answer of ask 3"), finished(1, None)],
            2
        ));
        assert!(
            request_read(
                &[
                    started(1, Some(2), "answer of ask 3"),
                    finished(1, Some("model"))
                ],
                2
            ),
            "a failure that ends the session is no wall"
        );
        assert!(!request_read(&[started(1, Some(2), "answer of ask 3")], 2));
        assert!(!request_read(&walled, 9));
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

    /// A Codex turn that falls back to the thread's total takes from it
    /// the last recorded total and what the turns after it counted from
    /// the rollouts without one.
    #[test]
    fn a_thread_total_less_what_earlier_turns_counted_is_the_turns_own() {
        let finished = |session: &str, payload: Value| {
            let mut payload = payload;
            payload["session_id"] = json!(session);
            RunEvent {
                id: super::super::EventId::new(1),
                task_id: None,
                goal_id: None,
                run_id: None,
                kind: TURN_FINISHED.to_owned(),
                payload,
                created_at: String::new(),
                actor: None,
            }
        };
        let usage = |input: i64| TokenUsage {
            input,
            output: input,
            messages: 1,
            ..TokenUsage::default()
        };
        let total = usage(100);
        // Nothing earlier: the whole total.
        assert_eq!(thread_total_own(&[], "th", &total), total);
        let events = [
            finished(
                "th",
                json!({"tokens_total": usage(30).payload(), "tokens": usage(30).payload()}),
            ),
            finished("other", json!({"tokens_total": usage(90).payload()})),
            // Counted from the rollouts, its turn.completed not read.
            finished(
                "th",
                json!({"tokens_total": null, "tokens": usage(20).payload()}),
            ),
        ];
        assert_eq!(thread_total_own(&events, "th", &total).input, 50);
        assert_eq!(thread_total_own(&events[..1], "th", &total).input, 70);
        // Without a total before, the rollouts' counts alone; never below 0.
        assert_eq!(thread_total_own(&events[2..], "th", &total).input, 80);
        assert_eq!(thread_total_own(&events, "th", &usage(10)).input, 0);
    }

    /// The root turns a Codex thread's turns counted are read back from
    /// their `turn_finished`: every turn of the thread's, no other
    /// session's, whatever wrapper recorded them.
    #[test]
    fn the_rollout_turns_a_thread_counted_are_read_from_its_turns() {
        let finished = |session: &str, turns: Value| RunEvent {
            id: super::super::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: TURN_FINISHED.to_owned(),
            payload: json!({"session_id": session, "tokens_turns": turns}),
            created_at: String::new(),
            actor: None,
        };
        let events = [
            finished("th", json!(["a"])),
            finished("other", json!(["x"])),
            // A turn counted from its thread total names none.
            finished("th", Value::Null),
            finished("th", json!(["b", "c"])),
        ];
        assert_eq!(counted_rollout_turns(&events, "th"), ["a", "b", "c"]);
        assert!(counted_rollout_turns(&events, "none").is_empty());
    }

    /// A resumed turn's tokens per model are what it added to the
    /// session's running totals (Claude's `modelUsage`), as its cost is
    /// (task 1199): a turn that started its session, one without an
    /// earlier total and one whose totals fell below them keep the totals
    /// (ADR-t1486-1).
    #[test]
    fn a_resumed_turn_uses_what_it_added_to_the_sessions_totals_per_model() {
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
        let model = |name: &str, input: i64, output: i64| ModelTokens {
            model: name.to_owned(),
            input,
            output,
            ..ModelTokens::default()
        };
        let started =
            |turn: u64, resume: bool| event(TURN_STARTED, json!({"turn": turn, "resume": resume}));
        let finished = |turn: u64, session: &str, totals: Value| {
            event(
                TURN_FINISHED,
                json!({"turn": turn, "session_id": session, "tokens_total_by_model": totals}),
            )
        };
        let totals = [model("opus", 15, 25), model("haiku", 1, 2)];
        // The turn started its session: the whole totals.
        assert_eq!(
            turn_own_models(&[started(1, false)], 1, "s", &totals),
            totals
        );
        let resumed = [
            started(1, false),
            finished(
                1,
                "s",
                json!([{"model": "opus", "input": 10, "output": 20}]),
            ),
            // Another session's turn, and one of the session without
            // totals, are skipped.
            started(2, false),
            finished(
                2,
                "thread",
                json!([{"model": "opus", "input": 14, "output": 0}]),
            ),
            finished(2, "s", Value::Null),
            started(3, true),
        ];
        assert_eq!(
            turn_own_models(&resumed, 3, "s", &totals),
            [model("opus", 5, 5), model("haiku", 1, 2)]
        );
        // A count below the earlier one: a session of its own.
        let fell = [model("opus", 9, 30)];
        assert_eq!(turn_own_models(&resumed, 3, "s", &fell), fell);
        // No earlier turn of the session with totals.
        assert_eq!(
            turn_own_models(&[started(2, true)], 2, "s", &totals),
            totals
        );
        let without = [finished(1, "s", Value::Null), started(2, true)];
        assert_eq!(turn_own_models(&without, 2, "s", &totals), totals);
    }

    /// A Codex span's turns for its work breakdown (task 1354), from turn
    /// events as values: a turn begun before the span starts before it, a
    /// turn started again without an end is replaced, a Claude turn after
    /// a switch is left out, a running turn is the model's to the close
    /// unless it was inferred, and a finished turn with no number is
    /// missing its commands.
    #[test]
    fn a_codex_spans_turns_are_those_its_breakdown_counts() {
        let event = |kind: &str, secs: i64, payload: Value| RunEvent {
            id: super::super::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: super::super::transcript::millis_text(secs * 1000),
            actor: None,
        };
        let turn = |start: i64, end: i64, of: Option<u64>| CodexSpanTurn {
            start,
            end: end * 1000,
            commands_of: of,
        };
        let events = [
            // Begun before the span (opened at 100).
            event(TURN_FINISHED, 120, json!({"turn": 1, "provider": "codex"})),
            // Started again without an end: the next one replaces it.
            event(TURN_STARTED, 125, json!({"turn": 2, "provider": "codex"})),
            event(TURN_STARTED, 130, json!({"turn": 3})),
            event(TURN_FINISHED, 140, json!({"turn": 3})),
            // On Claude after a switch.
            event(TURN_STARTED, 150, json!({"turn": 4, "provider": "claude"})),
            event(TURN_FINISHED, 160, json!({"turn": 4, "provider": "claude"})),
            // A time that cannot be read is skipped.
            RunEvent {
                created_at: "never".into(),
                ..event(TURN_STARTED, 0, json!({"turn": 9}))
            },
            event(TURN_STARTED, 170, json!({"turn": 5, "provider": "codex"})),
        ];
        assert_eq!(
            codex_span_turns(&events, 100_000, 200_000, false),
            Ok(vec![
                turn(i64::MIN, 120, Some(1)),
                turn(130_000, 140, Some(3)),
                turn(170_000, 200, None),
            ])
        );
        // Inferred: the running turn is left out.
        assert_eq!(
            codex_span_turns(&events, 100_000, 200_000, true),
            Ok(vec![
                turn(i64::MIN, 120, Some(1)),
                turn(130_000, 140, Some(3))
            ])
        );
        // A turn that started before the span opened is counted from it.
        assert_eq!(
            codex_span_turns(
                &[event(TURN_STARTED, 90, json!({"turn": 1}))],
                100_000,
                110_000,
                false
            ),
            Ok(vec![turn(100_000, 110, None)])
        );
        // A finished turn with no number cannot be found.
        assert_eq!(
            codex_span_turns(
                &[
                    event(TURN_STARTED, 101, json!({})),
                    event(TURN_FINISHED, 102, json!({}))
                ],
                100_000,
                110_000,
                false
            ),
            Err(TURN_COMMANDS_MISSING)
        );
        // Nothing but other events: no turn.
        assert_eq!(
            codex_span_turns(
                &[event("session_exited", 105, json!({}))],
                100_000,
                110_000,
                false
            ),
            Ok(Vec::new())
        );
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

    /// A planner's span takes the model most of its turns ran on.
    #[test]
    fn the_model_of_turns_is_the_one_most_said() {
        let finished = |id: i64, model: Value| RunEvent {
            id: super::super::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: TURN_FINISHED.to_owned(),
            payload: json!({"model": model}),
            created_at: String::new(),
            actor: None,
        };
        assert_eq!(turns_model(&[]), None);
        assert_eq!(turns_model(&[finished(1, Value::Null)]), None);
        let events = [
            finished(1, json!("a")),
            finished(2, json!("b")),
            finished(3, json!("b")),
            finished(4, Value::Null),
        ];
        assert_eq!(turns_model(&events).as_deref(), Some("b"));
        assert_eq!(turns_model(&events[..2]).as_deref(), Some("a"));
    }
}
