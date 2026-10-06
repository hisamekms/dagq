//! Moving a run's worker off a provider it cannot use (ADR-t813-2). A
//! provider cannot be used when its executable is missing, it does not
//! start, its login ran out or it hit a usage limit; nothing else (a failed
//! test, a failed turn of any other kind) moves a worker. At the claim a
//! task whose provider cannot be used starts on the other one
//! ([`routes`]); in a headless run the next call after a turn that failed
//! so goes to the other provider in a new session of the same worktree
//! ([`SwitchPhase`]). A run switches at most [`MAX_PROVIDER_SWITCHES`]
//! times, so it cannot go back and forth. Claude's hold is the queue's
//! `queue_hold` ask; Codex's is a [`ProviderHold`] on the queue's events,
//! which no ask shows and which ends on its own after a while. A person
//! may turn the move off (`fallback` false, ADR-t1857-1): a worker whose
//! provider cannot be used then waits for its hold to end and goes on
//! there; `--no-claude` still sends a Claude task to Codex.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    DomainError, Provider, RunEvent, RunId,
    claim_hold::HoldReason,
    event_kind,
    queue_hold::Wall,
    turn::TurnFailure,
    worker::{Worker, WorkerMode},
};

/// How many times a run's worker may move between the providers: once
/// away, and once back when the other one cannot be used either
/// (ADR-t813-2 decision 4). A run that used them up waits in the hold ask
/// instead.
pub const MAX_PROVIDER_SWITCHES: usize = 2;

string_enum!(SwitchReason {
    Disabled => "provider_disabled",
    ExecutableMissing => "executable_missing",
    Authentication => "authentication",
    UsageLimit => "usage_limit",
    LaunchFailed => "launch_failed",
    // A review that requires subagents its provider cannot run
    // (ADR-t1453-1 decision 8): the provider itself can be used, so it is
    // not held.
    SubagentsUnsupported => "subagents_unsupported",
});

impl SwitchReason {
    /// The reason of a turn that failed so, if it says its provider cannot
    /// be used: a login, a usage limit, or an agent that did not start.
    pub const fn of_failure(failure: TurnFailure) -> Option<Self> {
        match failure {
            TurnFailure::Authentication => Some(Self::Authentication),
            TurnFailure::UsageLimit => Some(Self::UsageLimit),
            TurnFailure::Launch => Some(Self::LaunchFailed),
            TurnFailure::Model | TurnFailure::Sandbox | TurnFailure::Other => None,
        }
    }

    /// The reason of the queue's hold ask (Claude's hold).
    pub const fn of_hold(reason: HoldReason) -> Option<Self> {
        match reason {
            HoldReason::Authentication => Some(Self::Authentication),
            HoldReason::UsageLimit => Some(Self::UsageLimit),
            HoldReason::DiskSpace | HoldReason::LoadAverage => None,
        }
    }

    /// Whether this says the provider cannot be used (missing, not
    /// started, a login, a usage limit): the moves a fallback turned off
    /// stops (ADR-t1857-1). `--no-claude` (a person's ban, ADR-t1204-1) and
    /// a review's need of subagents (a choice by ability, ADR-t1453-1
    /// decision 8) still move.
    pub const fn unusable(self) -> bool {
        match self {
            Self::ExecutableMissing
            | Self::LaunchFailed
            | Self::Authentication
            | Self::UsageLimit => true,
            Self::Disabled | Self::SubagentsUnsupported => false,
        }
    }

    /// How long a provider held for this stays held before its next call
    /// checks it again (ADR-t813-2 decision 6): a usage limit's window is
    /// long, a login may be fixed sooner, and an agent that did not start
    /// is tried again soonest. A usage limit whose reset the provider's
    /// text says ends then instead ([`reset_at`]).
    pub const fn hold_secs(self) -> i64 {
        match self {
            // Policy, never a timed provider hold; nor is a review's need
            // of subagents (ADR-t1453-1 decision 8).
            Self::Disabled | Self::SubagentsUnsupported => 0,
            Self::UsageLimit => 1800,
            Self::Authentication => 900,
            Self::LaunchFailed | Self::ExecutableMissing => 600,
        }
    }
}

string_enum!(SwitchPhase {
    Start => "start",
    Answer => "answer",
    Revise => "revise",
    Resume => "resume",
    Nudge => "nudge",
});

impl SwitchPhase {
    /// The phase of the call a switch makes in place of the turn that
    /// failed: the task's prompt (`request` is none), an answer, a revise,
    /// a nudge, or any other request to go on (a resolution request, a
    /// recovery instruction, the hold's continue).
    pub fn of_request(request: Option<u64>, what: &str) -> Self {
        match (request, what) {
            (None, _) => Self::Start,
            (_, what) if what.starts_with("answer") => Self::Answer,
            (_, "revise request") => Self::Revise,
            (_, "nudge") => Self::Nudge,
            _ => Self::Resume,
        }
    }
}

/// `[provider_fallback]` of `dagq.toml` (ADR-t1857-1): whether a worker
/// moves off a provider it cannot use (`workers`), and whether a headless
/// job whose role names its provider does (`jobs`); by default both do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderFallback {
    pub workers: bool,
    pub jobs: bool,
}

impl ProviderFallback {
    /// The keys of `[provider_fallback]`.
    pub const KEYS: [&str; 2] = ["workers", "jobs"];
}

impl Default for ProviderFallback {
    fn default() -> Self {
        Self {
            workers: true,
            jobs: true,
        }
    }
}

/// The run's switches, as recorded.
pub fn switches(events: &[RunEvent]) -> usize {
    events
        .iter()
        .filter(|e| e.kind == event_kind::PROVIDER_SWITCHED)
        .count()
}

/// Whether the run may switch once more ([`MAX_PROVIDER_SWITCHES`]).
pub fn may_switch(events: &[RunEvent]) -> bool {
    switches(events) < MAX_PROVIDER_SWITCHES
}

/// The switch made in place of turn `turn`, if one was.
pub fn switch_of_turn(events: &[RunEvent], turn: u64) -> Option<&RunEvent> {
    events
        .iter()
        .rev()
        .find(|e| e.kind == event_kind::PROVIDER_SWITCHED && e.payload["turn"] == turn)
}

/// Whether the run already waits for a provider after turn `turn`
/// (`provider_waiting`).
pub fn waiting_on(events: &[RunEvent], turn: u64) -> bool {
    events
        .iter()
        .any(|e| e.kind == event_kind::PROVIDER_WAITING && e.payload["turn"] == turn)
}

/// The run's events since its last switch: those of the session of its
/// current provider (all of them when it never switched).
pub fn since_switch(events: &[RunEvent]) -> &[RunEvent] {
    match events
        .iter()
        .rposition(|e| e.kind == event_kind::PROVIDER_SWITCHED)
    {
        Some(at) => &events[at + 1..],
        None => events,
    }
}

/// The longest a usage limit's reset read from a provider's text holds its
/// provider: a later time is taken as misread.
pub const MAX_RESET_SECS: i64 = 7 * 24 * 60 * 60;

/// When the usage limit a provider's `text` reports resets, if the text
/// says so in a form read without a time zone (ADR-t813-2 decision 6): a
/// unix time after `|` (Claude Code's `Claude AI usage limit reached|<t>`)
/// or after `resets at` (Claude's `rate_limit_event`, as its reader words
/// it), or a span after `in` (`try again in 2 hours 13 minutes`, `in 45s`).
/// A clock time (`Try again at 3pm`, `resets 3pm`) needs the provider's
/// time zone and is not read. `None` when nothing is read, the time is
/// past, or it is more than [`MAX_RESET_SECS`] away.
pub fn reset_at(text: &str, now: i64) -> Option<i64> {
    let lower = text.to_ascii_lowercase();
    let epoch_after = |marker: &str| {
        lower.match_indices(marker).find_map(|(at, _)| {
            let rest = lower[at + marker.len()..].trim_start();
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            (digits.len() == 10)
                .then(|| digits.parse::<i64>().ok())
                .flatten()
        })
    };
    let at = epoch_after("|")
        .or_else(|| epoch_after("resets at"))
        .or_else(|| {
            lower
                .match_indices(" in ")
                .find_map(|(at, _)| span_secs(&lower[at + 4..]))
                .map(|secs| now + secs)
        })?;
    (at >= now && at - now <= MAX_RESET_SECS).then_some(at)
}

/// The seconds of a span at the start of `text` (`2 hours 13 minutes`,
/// `45s`, `1 day, 3 hours`); `None` when it starts with none.
fn span_secs(text: &str) -> Option<i64> {
    let mut secs = 0;
    let mut read = false;
    let mut rest = text;
    loop {
        rest = rest
            .trim_start_matches([' ', ','])
            .trim_start_matches("and ");
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            break;
        }
        let number: i64 = digits.parse().ok()?;
        rest = rest[digits.len()..].trim_start();
        let unit: String = rest.chars().take_while(char::is_ascii_alphabetic).collect();
        let size = match unit.as_str() {
            "d" | "day" | "days" => 86_400,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            _ => break,
        };
        secs += number * size;
        read = true;
        rest = &rest[unit.len()..];
    }
    read.then_some(secs)
}

/// The payload of `provider_switched`.
#[allow(clippy::too_many_arguments)]
pub fn switched_payload(
    from: Provider,
    to: Worker,
    reason: SwitchReason,
    phase: SwitchPhase,
    turn: Option<u64>,
    count: usize,
    message: Option<&str>,
) -> Value {
    json!({
        "from": from,
        "to": to.provider,
        "worker_mode": to.mode,
        "reason": reason,
        "phase": phase,
        "turn": turn,
        "count": count,
        "message": message,
    })
}

/// How a claim runs a task that asks for `requested`: on `actual`, which
/// differs when the requested provider cannot be used, and why
/// (`switch`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerRoute {
    pub requested: Worker,
    pub actual: Worker,
    pub switch: Option<SwitchReason>,
}

impl WorkerRoute {
    /// Each of `workers` run as asked.
    pub fn direct(workers: &[Worker]) -> Vec<Self> {
        workers
            .iter()
            .map(|&worker| Self {
                requested: worker,
                actual: worker,
                switch: None,
            })
            .collect()
    }
}

/// How a supervisor with the adapters of `supported` runs each worker a
/// task may ask for, while `held` says why a provider is held: headless
/// on the requested provider when it is usable; otherwise
/// headless on the other provider when that one is (ADR-t813-2 decisions
/// 2 and 6); otherwise not at all (no route: the task waits). With the
/// fallback off (`fallback` false, ADR-t1857-1) a provider that cannot be
/// used ([`SwitchReason::unusable`]) leaves its tasks no route, so they
/// wait for its hold to end; `--no-claude` (`Disabled`) still moves them.
pub fn routes(
    supported: &[Worker],
    held: impl Fn(Provider) -> Option<SwitchReason>,
    fallback: bool,
) -> Vec<WorkerRoute> {
    let usable = |worker: &Worker| supported.contains(worker) && held(worker.provider).is_none();
    Worker::ALL
        .iter()
        .filter_map(|&requested| {
            let preferred = Worker {
                provider: requested.provider,
                mode: WorkerMode::Headless,
            };
            if usable(&preferred) {
                return Some(WorkerRoute {
                    requested,
                    actual: preferred,
                    switch: None,
                });
            }
            let reason = held(requested.provider).unwrap_or(SwitchReason::ExecutableMissing);
            if !fallback && reason.unusable() {
                return None;
            }
            let actual = Worker {
                provider: requested.provider.other(),
                mode: WorkerMode::Headless,
            };
            usable(&actual).then_some(WorkerRoute {
                requested,
                actual,
                switch: Some(reason),
            })
        })
        .collect()
}

/// Whether `worker` has no route only because the fallback is off
/// (ADR-t1857-1): with it on, [`routes`] would run it on the other
/// provider. Its claim is deferred with that said.
pub fn stopped_by_fallback(
    supported: &[Worker],
    held: impl Fn(Provider) -> Option<SwitchReason>,
    worker: Worker,
) -> bool {
    route_of(&routes(supported, &held, true), worker).is_some()
        && route_of(&routes(supported, &held, false), worker).is_none()
}

/// Why `provider` is held for the workers now, if it is, given
/// `--no-claude` (`no_claude`), why the queue's open hold ask holds Claude
/// (`queue_hold`) and the provider's own [`ProviderHold`] (`own`, see
/// [`own_hold`]): Claude is disabled under `--no-claude`, else held by the
/// hold ask before its own hold (an agent that did not start); Codex only
/// by its own, so Claude's hold ask never holds it.
pub fn held(
    provider: Provider,
    no_claude: bool,
    queue_hold: Option<SwitchReason>,
    own: Option<SwitchReason>,
) -> Option<SwitchReason> {
    match provider {
        Provider::Claude if no_claude => Some(SwitchReason::Disabled),
        Provider::Claude => queue_hold.or(own),
        Provider::Codex => own,
    }
}

/// Why `provider`'s own [`ProviderHold`] among `holds` holds it, if one
/// does.
pub fn own_hold(holds: &[ProviderHold], provider: Provider) -> Option<SwitchReason> {
    holds
        .iter()
        .find(|hold| hold.provider == provider)
        .map(|hold| hold.reason)
}

/// Whether a headless turn of `from` that failed for `reason` stopped at a
/// wall of Claude's that its hold ask holds (a login or a usage limit,
/// not an agent that did not start): no [`ProviderHold`] is recorded for
/// it, and the run that moves on opens Claude's hold ask for the
/// Claude-only jobs.
pub fn claude_wall(from: Provider, reason: SwitchReason) -> bool {
    from == Provider::Claude && reason != SwitchReason::LaunchFailed
}

/// The wall of the hold ask a provider that cannot be used for `reason`
/// raises: a usage limit or a login; none for the rest.
pub const fn wall_of(reason: SwitchReason) -> Option<Wall> {
    match reason {
        SwitchReason::UsageLimit => Some(Wall::UsageLimit),
        SwitchReason::Authentication => Some(Wall::Authentication),
        SwitchReason::Disabled
        | SwitchReason::LaunchFailed
        | SwitchReason::ExecutableMissing
        | SwitchReason::SubagentsUnsupported => None,
    }
}

/// What a run whose headless turn failed at its provider's wall does on
/// this look (ADR-t813-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallMove {
    /// The call is made again on the other provider in a new session.
    Switch,
    /// Its own provider can be used again: the call is made again there.
    Retry,
    /// It waits for a provider; `record` on the first look
    /// (`provider_waiting`).
    Wait { record: bool },
}

/// What a run at its provider's wall does: it moves when the fallback is
/// on (`fallback`, ADR-t1857-1), it has switches left (`may_switch`) and
/// the other provider can be used (`other_usable`); else, on a later look
/// (`first_look` false), the call is made again on its own provider once
/// that one is not held (`own_held`) and no hold ask holds the run for a
/// person's `done` (`hold_unclosed`); else it waits, recorded once on the
/// first look. A wall is always a provider that cannot be used, so the
/// fallback off never lets it move.
pub fn wall_move(
    fallback: bool,
    may_switch: bool,
    other_usable: bool,
    first_look: bool,
    own_held: bool,
    hold_unclosed: bool,
) -> WallMove {
    if fallback && may_switch && other_usable {
        WallMove::Switch
    } else if !first_look && !own_held && !hold_unclosed {
        WallMove::Retry
    } else {
        WallMove::Wait { record: first_look }
    }
}

/// The hold ask a run that waits for a provider joins, if any, after its
/// turn on `from` failed for `reason`: none under `--no-claude`
/// (`no_claude`: Codex keeps its own hold, which no ask shows); Claude's
/// login or usage limit always opens it; otherwise only when the other
/// provider cannot be used either (this supervisor has no headless worker
/// of it, `other_headless` false, or it is held), with the wall of the
/// hold ask open already (`open_hold`, its reason), or of whichever
/// provider's hold is a login or a usage limit. Two agents that do not
/// start open none: the run waits on their holds (`holds`).
pub fn wall_to_raise(
    no_claude: bool,
    from: Provider,
    reason: SwitchReason,
    other_headless: bool,
    open_hold: Option<HoldReason>,
    holds: &[ProviderHold],
) -> Option<Wall> {
    if no_claude {
        return None;
    }
    if claude_wall(from, reason) {
        return wall_of(reason);
    }
    let to = from.other();
    let to_held = held(
        to,
        false,
        open_hold.and_then(SwitchReason::of_hold),
        own_hold(holds, to),
    );
    if other_headless && to_held.is_none() {
        return None;
    }
    if let Some(hold) = open_hold {
        return SwitchReason::of_hold(hold).and_then(wall_of);
    }
    wall_of(reason).or_else(|| to_held.and_then(wall_of))
}

/// The route of `worker` among `routes`.
pub fn route_of(routes: &[WorkerRoute], worker: Worker) -> Option<&WorkerRoute> {
    routes.iter().find(|route| route.requested == worker)
}

/// A provider the workers do not use for now (ADR-t813-2 decision 6):
/// Codex's login, usage limit or start failed, and no ask is opened for
/// it while Claude goes on. It ends at `retry_at`, when the next call of
/// it checks it again, or when a person answers a hold ask `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProviderHold {
    pub provider: Provider,
    pub reason: SwitchReason,
    pub since: i64,
    pub retry_at: i64,
}

impl ProviderHold {
    pub fn new(provider: Provider, reason: SwitchReason, now: i64) -> Self {
        Self {
            provider,
            reason,
            since: now,
            retry_at: now + reason.hold_secs(),
        }
    }

    /// This hold, ending at `reset` (a usage limit's reset read from the
    /// provider's text) when there is one.
    pub fn until(mut self, reset: Option<i64>) -> Self {
        if let Some(reset) = reset {
            self.retry_at = reset;
        }
        self
    }

    /// Whether its time is up at `now`.
    pub fn due(&self, now: i64) -> bool {
        now >= self.retry_at
    }

    /// The payload of `provider_held`.
    pub fn held_payload(&self, run: Option<&RunId>) -> Value {
        json!({
            "provider": self.provider,
            "reason": self.reason,
            "since": self.since,
            "retry_at": self.retry_at,
            "run_id": run,
        })
    }

    /// The payload of `provider_released`.
    pub fn released_payload(&self, why: &str) -> Value {
        json!({
            "provider": self.provider,
            "reason": self.reason,
            "since": self.since,
            "why": why,
        })
    }

    /// The hold in place, from the latest `provider_held` or
    /// `provider_released` of its provider (`kind` and `payload`): a hold
    /// not released since. Its time may be up; the caller releases it.
    pub fn in_place(kind: &str, payload: &Value) -> Option<Self> {
        if kind != event_kind::PROVIDER_HELD {
            return None;
        }
        Some(Self {
            provider: payload["provider"].as_str()?.parse().ok()?,
            reason: payload["reason"].as_str()?.parse().ok()?,
            since: payload["since"].as_i64()?,
            retry_at: payload["retry_at"].as_i64()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, TaskId};

    fn event(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    const CLAUDE: Worker = Worker::CLAUDE_INTERACTIVE;
    const CLAUDE_HEADLESS: Worker = Worker::ALL[1];
    const CODEX: Worker = Worker::ALL[2];

    #[test]
    fn only_a_provider_that_cannot_be_used_moves_a_worker() {
        assert_eq!(
            SwitchReason::of_failure(TurnFailure::Authentication),
            Some(SwitchReason::Authentication)
        );
        assert_eq!(
            SwitchReason::of_failure(TurnFailure::UsageLimit),
            Some(SwitchReason::UsageLimit)
        );
        assert_eq!(
            SwitchReason::of_failure(TurnFailure::Launch),
            Some(SwitchReason::LaunchFailed)
        );
        for failure in [TurnFailure::Model, TurnFailure::Sandbox, TurnFailure::Other] {
            assert_eq!(SwitchReason::of_failure(failure), None, "{failure:?}");
        }
        assert_eq!(
            SwitchReason::of_hold(HoldReason::UsageLimit),
            Some(SwitchReason::UsageLimit)
        );
        assert_eq!(SwitchReason::of_hold(HoldReason::DiskSpace), None);
        assert_eq!(SwitchReason::of_hold(HoldReason::LoadAverage), None);
        assert_eq!(
            SwitchReason::of_hold(HoldReason::Authentication),
            Some(SwitchReason::Authentication)
        );
    }

    #[test]
    fn the_phase_is_the_call_the_failed_turn_made() {
        assert_eq!(
            SwitchPhase::of_request(None, "the task's prompt"),
            SwitchPhase::Start
        );
        assert_eq!(
            SwitchPhase::of_request(Some(2), "answer of ask 4"),
            SwitchPhase::Answer
        );
        assert_eq!(
            SwitchPhase::of_request(Some(2), "revise request"),
            SwitchPhase::Revise
        );
        assert_eq!(
            SwitchPhase::of_request(Some(2), "nudge"),
            SwitchPhase::Nudge
        );
        assert_eq!(
            SwitchPhase::of_request(Some(2), "resolution request"),
            SwitchPhase::Resume
        );
        assert_eq!(
            SwitchPhase::of_request(Some(2), "continue"),
            SwitchPhase::Resume
        );
    }

    #[test]
    fn a_run_switches_twice_at_most_and_its_session_starts_at_the_last() {
        let first = event(event_kind::PROVIDER_SWITCHED, json!({"turn": 1}));
        let turn = event(event_kind::TURN_STARTED, json!({"turn": 2}));
        let second = event(event_kind::PROVIDER_SWITCHED, json!({"turn": 2}));
        let events = vec![
            event(event_kind::TURN_STARTED, json!({"turn": 1})),
            first.clone(),
        ];
        assert!(may_switch(&events));
        assert!(!waiting_on(&events, 1));
        assert!(waiting_on(
            &[event(event_kind::PROVIDER_WAITING, json!({"turn": 1}))],
            1
        ));
        assert_eq!(since_switch(&events).len(), 0);
        assert!(switch_of_turn(&events, 1).is_some());
        assert!(switch_of_turn(&events, 2).is_none());
        let events = vec![first, turn.clone(), second, turn];
        assert_eq!(switches(&events), 2);
        assert!(!may_switch(&events));
        assert_eq!(since_switch(&events).len(), 1);
        assert_eq!(since_switch(&[]).len(), 0);
    }

    #[test]
    fn a_worker_runs_as_asked_or_on_the_other_provider() {
        let all = Worker::ALL.to_vec();
        let none = |_: Provider| None;
        let normalized = routes(&all, none, true);
        for requested in Worker::ALL {
            let route = route_of(&normalized, requested).unwrap();
            assert_eq!(route.actual.provider, requested.provider);
            assert_eq!(route.actual.mode, WorkerMode::Headless);
            assert_eq!(route.switch, None);
        }
        // No Codex: its tasks run on headless Claude.
        let claude_only = [CLAUDE, CLAUDE_HEADLESS];
        let routes_now = routes(&claude_only, none, true);
        let codex = route_of(&routes_now, CODEX).unwrap();
        assert_eq!(codex.actual, CLAUDE_HEADLESS);
        assert_eq!(codex.switch, Some(SwitchReason::ExecutableMissing));
        assert_eq!(
            route_of(&routes_now, CLAUDE).unwrap().actual,
            CLAUDE_HEADLESS
        );
        // Claude held: every task runs on Codex.
        let claude_held = |p: Provider| (p == Provider::Claude).then_some(SwitchReason::UsageLimit);
        let routes_now = routes(&all, claude_held, true);
        for worker in Worker::ALL {
            let route = route_of(&routes_now, worker).unwrap();
            assert_eq!(route.actual, CODEX, "{worker:?}");
        }
        assert_eq!(
            route_of(&routes_now, CLAUDE).unwrap().switch,
            Some(SwitchReason::UsageLimit)
        );
        assert_eq!(route_of(&routes_now, CODEX).unwrap().switch, None);
        // Claude held and no Codex: nothing runs.
        assert!(routes(&claude_only, claude_held, true).is_empty());
        // Codex held: its tasks run on Claude.
        let codex_held =
            |p: Provider| (p == Provider::Codex).then_some(SwitchReason::Authentication);
        let routes_now = routes(&all, codex_held, true);
        let codex = route_of(&routes_now, CODEX).unwrap();
        assert_eq!(codex.actual, CLAUDE_HEADLESS);
        assert_eq!(codex.switch, Some(SwitchReason::Authentication));
    }

    /// The adapters of every worker but `provider`'s (its executable is
    /// missing) when `reason` is `ExecutableMissing`, and every one with
    /// `provider` held for `reason` otherwise.
    fn cannot_use(
        provider: Provider,
        reason: SwitchReason,
    ) -> (Vec<Worker>, impl Fn(Provider) -> Option<SwitchReason>) {
        let missing = reason == SwitchReason::ExecutableMissing;
        let supported = Worker::ALL
            .into_iter()
            .filter(|worker| !missing || worker.provider != provider)
            .collect();
        (supported, move |p: Provider| {
            (p == provider && !missing).then_some(reason)
        })
    }

    #[test]
    fn with_the_fallback_off_a_task_whose_provider_cannot_be_used_has_no_route() {
        use SwitchReason::{Authentication, ExecutableMissing, LaunchFailed, UsageLimit};
        for requested in [CLAUDE_HEADLESS, CODEX] {
            let other = Worker {
                provider: requested.provider.other(),
                mode: WorkerMode::Headless,
            };
            for reason in [ExecutableMissing, LaunchFailed, Authentication, UsageLimit] {
                assert!(reason.unusable(), "{reason:?}");
                let (supported, held) = cannot_use(requested.provider, reason);
                // On: it moves to the other provider, why said.
                let on = routes(&supported, &held, true);
                let route = route_of(&on, requested).unwrap();
                assert_eq!(route.actual, other, "{requested:?} {reason:?}");
                assert_eq!(route.switch, Some(reason));
                // Off: no route, so the task waits for its provider.
                let off = routes(&supported, &held, false);
                assert!(
                    route_of(&off, requested).is_none(),
                    "{requested:?} {reason:?}"
                );
                assert!(stopped_by_fallback(&supported, &held, requested));
                // The other provider's tasks run there as before.
                let direct = route_of(&off, other).unwrap();
                assert_eq!(direct.actual, other);
                assert_eq!(direct.switch, None);
                assert!(!stopped_by_fallback(&supported, &held, other));
            }
        }
        // Neither provider usable: no route either way, and not for the
        // fallback.
        let both = |_: Provider| Some(UsageLimit);
        assert!(routes(&Worker::ALL, both, true).is_empty());
        assert!(!stopped_by_fallback(&Worker::ALL, both, CODEX));
        // Nothing held: the fallback changes nothing.
        let none = |_: Provider| None;
        assert_eq!(
            routes(&Worker::ALL, none, false),
            routes(&Worker::ALL, none, true)
        );
        // A ban and a need of subagents are not a provider that cannot be
        // used.
        assert!(!SwitchReason::Disabled.unusable());
        assert!(!SwitchReason::SubagentsUnsupported.unusable());
    }

    /// `--no-claude` (a person's ban) sends a Claude task to Codex whether
    /// the fallback is on or off.
    fn no_claude_sends_claude_to_codex(fallback: bool) {
        let no_claude = |p: Provider| held_by(p, true, None, &[]);
        let now = routes(&Worker::ALL, no_claude, fallback);
        for worker in [CLAUDE, CLAUDE_HEADLESS] {
            let route = route_of(&now, worker).unwrap();
            assert_eq!(route.actual, CODEX, "{worker:?}");
            assert_eq!(route.switch, Some(SwitchReason::Disabled));
        }
        assert!(!stopped_by_fallback(
            &Worker::ALL,
            no_claude,
            CLAUDE_HEADLESS
        ));
        // Codex unusable too: no route, as before.
        let codex_held = [hold(Provider::Codex, SwitchReason::UsageLimit)];
        let neither = |p: Provider| held_by(p, true, None, &codex_held);
        assert!(routes(&Worker::ALL, neither, fallback).is_empty());
    }

    #[test]
    fn no_claude_sends_a_claude_task_to_codex_with_the_fallback_on() {
        no_claude_sends_claude_to_codex(true);
    }

    #[test]
    fn no_claude_sends_a_claude_task_to_codex_with_the_fallback_off() {
        no_claude_sends_claude_to_codex(false);
    }

    #[test]
    fn with_the_fallback_off_a_run_at_a_wall_waits_and_retries_its_own_provider() {
        // Never a switch, whatever is left and usable.
        for (may_switch, other_usable, first_look, own_held, hold_unclosed) in [
            (true, true, true, true, false),
            (true, true, false, false, true),
            (true, true, false, false, false),
        ] {
            assert_ne!(
                wall_move(
                    false,
                    may_switch,
                    other_usable,
                    first_look,
                    own_held,
                    hold_unclosed
                ),
                WallMove::Switch
            );
        }
        // The first look records the wait, even with the other provider
        // usable.
        assert_eq!(
            wall_move(false, true, true, true, true, false),
            WallMove::Wait { record: true }
        );
        // Its own provider still held, or a hold ask open: it waits.
        assert_eq!(
            wall_move(false, true, true, false, true, false),
            WallMove::Wait { record: false }
        );
        assert_eq!(
            wall_move(false, true, true, false, false, true),
            WallMove::Wait { record: false }
        );
        // Its hold ended (`retry_at` or `done`): the call goes again to the
        // same provider.
        assert_eq!(
            wall_move(false, true, true, false, false, false),
            WallMove::Retry
        );
    }

    #[test]
    fn a_hold_ends_after_its_time_and_is_read_back_from_its_event() {
        let hold = ProviderHold::new(Provider::Codex, SwitchReason::UsageLimit, 100);
        assert_eq!(hold.retry_at, 1900);
        assert!(!hold.due(1899));
        assert!(hold.due(1900));
        let run = RunId::new("r1".to_owned()).unwrap();
        let payload = hold.held_payload(Some(&run));
        assert_eq!(payload["run_id"], "r1");
        assert_eq!(
            ProviderHold::in_place(event_kind::PROVIDER_HELD, &payload),
            Some(hold)
        );
        assert_eq!(
            ProviderHold::in_place(
                event_kind::PROVIDER_RELEASED,
                &hold.released_payload("done")
            ),
            None
        );
        assert_eq!(
            ProviderHold::in_place(event_kind::PROVIDER_HELD, &json!({"provider": "codex"})),
            None
        );
    }

    #[test]
    fn a_usage_limit_ends_at_the_reset_its_text_says() {
        let now = 1_790_000_000;
        assert_eq!(
            reset_at("Claude AI usage limit reached|1790003600", now),
            Some(1_790_003_600)
        );
        assert_eq!(
            reset_at(
                "the five_hour usage limit was hit (resets at 1790001800)",
                now
            ),
            Some(1_790_001_800)
        );
        assert_eq!(
            reset_at(
                "You've hit your usage limit. Upgrade to Pro or try again in 2 hours 13 minutes.",
                now
            ),
            Some(now + 2 * 3600 + 13 * 60)
        );
        assert_eq!(
            reset_at("rate limited; retry in 1 day, 3 hours and 5 minutes", now),
            Some(now + 86_400 + 3 * 3600 + 300)
        );
        assert_eq!(reset_at("try again in 45s", now), Some(now + 45));
        assert_eq!(reset_at("try again in 0 seconds", now), Some(now));
        // A clock time, a past time, one too far and no time are not read.
        assert_eq!(reset_at("Try again at 3pm.", now), None);
        assert_eq!(reset_at("limit reached|1700000000", now), None);
        assert_eq!(reset_at("try again in 30 days", now), None);
        assert_eq!(reset_at("usage limit reached in the plan", now), None);
        let hold = ProviderHold::new(Provider::Codex, SwitchReason::UsageLimit, now);
        assert_eq!(hold.until(Some(now + 5)).retry_at, now + 5);
        assert_eq!(hold.until(None).retry_at, now + 1800);
    }

    fn hold(provider: Provider, reason: SwitchReason) -> ProviderHold {
        ProviderHold::new(provider, reason, 100)
    }

    /// Why `provider` is held with the hold ask of `ask` open and `holds`
    /// recorded.
    fn held_by(
        provider: Provider,
        no_claude: bool,
        ask: Option<HoldReason>,
        holds: &[ProviderHold],
    ) -> Option<SwitchReason> {
        held(
            provider,
            no_claude,
            ask.and_then(SwitchReason::of_hold),
            own_hold(holds, provider),
        )
    }

    // Moved here by task 1713 from the tests/it cases it removed:
    // runtime_codex::a_codex_review_starts_while_claudes_hold_ask_is_open and
    // runtime_provider_switch::no_claude_without_codex_claims_nothing_and_reports_manual_policy.
    #[test]
    fn claudes_hold_ask_holds_only_claude_and_no_claude_disables_it() {
        let codex_limit = [hold(Provider::Codex, SwitchReason::UsageLimit)];
        // Claude's hold ask holds Claude, not Codex.
        let ask = Some(HoldReason::Authentication);
        assert_eq!(
            held_by(Provider::Claude, false, ask, &[]),
            Some(SwitchReason::Authentication)
        );
        assert_eq!(held_by(Provider::Codex, false, ask, &[]), None);
        // A hold ask of the disk or the load is no provider's.
        assert_eq!(
            held_by(Provider::Claude, false, Some(HoldReason::DiskSpace), &[]),
            None
        );
        // Each provider's own hold.
        assert_eq!(
            held_by(Provider::Codex, false, None, &codex_limit),
            Some(SwitchReason::UsageLimit)
        );
        assert_eq!(held_by(Provider::Claude, false, None, &codex_limit), None);
        let claude_launch = [hold(Provider::Claude, SwitchReason::LaunchFailed)];
        assert_eq!(
            held_by(Provider::Claude, false, None, &claude_launch),
            Some(SwitchReason::LaunchFailed)
        );
        // The hold ask's reason before Claude's own.
        assert_eq!(
            held_by(
                Provider::Claude,
                false,
                Some(HoldReason::UsageLimit),
                &claude_launch
            ),
            Some(SwitchReason::UsageLimit)
        );
        // `--no-claude` disables Claude whatever holds it, and not Codex.
        assert_eq!(
            held_by(Provider::Claude, true, None, &[]),
            Some(SwitchReason::Disabled)
        );
        assert_eq!(
            held_by(Provider::Claude, true, ask, &claude_launch),
            Some(SwitchReason::Disabled)
        );
        assert_eq!(held_by(Provider::Codex, true, None, &[]), None);
        // Claude disabled and no Codex: no worker can be claimed.
        let no_claude = |p: Provider| held_by(p, true, None, &[]);
        assert!(routes(&[CLAUDE, CLAUDE_HEADLESS], no_claude, true).is_empty());
        // Claude disabled with Codex: its tasks run on Codex, why said.
        let routes_now = routes(&Worker::ALL, no_claude, true);
        let claude = route_of(&routes_now, CLAUDE_HEADLESS).unwrap();
        assert_eq!(claude.actual, CODEX);
        assert_eq!(claude.switch, Some(SwitchReason::Disabled));
    }

    // Moved here by task 1713 from the tests/it cases it removed:
    // runtime_provider_switch::a_codex_that_does_not_start_moves_its_run_to_claude
    // and a_run_switches_twice_at_most_and_then_waits_in_the_hold_ask.
    #[test]
    fn only_claudes_login_or_limit_is_a_wall_of_its_hold_ask() {
        assert!(claude_wall(Provider::Claude, SwitchReason::UsageLimit));
        assert!(claude_wall(Provider::Claude, SwitchReason::Authentication));
        // An agent of Claude's that did not start is held like Codex's.
        assert!(!claude_wall(Provider::Claude, SwitchReason::LaunchFailed));
        for reason in [
            SwitchReason::UsageLimit,
            SwitchReason::Authentication,
            SwitchReason::LaunchFailed,
        ] {
            assert!(!claude_wall(Provider::Codex, reason), "{reason:?}");
        }
        assert_eq!(wall_of(SwitchReason::UsageLimit), Some(Wall::UsageLimit));
        assert_eq!(
            wall_of(SwitchReason::Authentication),
            Some(Wall::Authentication)
        );
        for reason in [
            SwitchReason::Disabled,
            SwitchReason::LaunchFailed,
            SwitchReason::ExecutableMissing,
            SwitchReason::SubagentsUnsupported,
        ] {
            assert_eq!(wall_of(reason), None, "{reason:?}");
        }
    }

    // Moved here by task 1713 from the tests/it cases it removed:
    // runtime_provider_switch::a_codex_that_does_not_start_moves_its_run_to_claude,
    // a_run_switches_twice_at_most_and_then_waits_in_the_hold_ask and
    // a_run_out_of_switches_waits_on_codexs_hold_and_retries_at_its_reset. The
    // kept runtime_provider_switch::a_codex_that_does_not_start_while_claude_is_held_waits_in_the_hold_ask
    // checks the wiring of the wait.
    #[test]
    fn a_run_at_a_wall_moves_retries_or_waits_recorded_once() {
        // Switches left and the other provider usable: it moves, whether
        // or not this is the first look.
        assert_eq!(
            wall_move(true, true, true, true, true, false),
            WallMove::Switch
        );
        assert_eq!(
            wall_move(true, true, true, false, false, true),
            WallMove::Switch
        );
        // The other provider held (Claude's hold ask open), or switches used
        // up: the first look records the wait.
        assert_eq!(
            wall_move(true, true, false, true, true, false),
            WallMove::Wait { record: true }
        );
        assert_eq!(
            wall_move(true, false, true, true, true, false),
            WallMove::Wait { record: true }
        );
        // A later look: its own provider still held, or a hold ask holds
        // the run for a person's `done`: it waits, not recorded again.
        assert_eq!(
            wall_move(true, false, true, false, true, false),
            WallMove::Wait { record: false }
        );
        assert_eq!(
            wall_move(true, true, false, false, false, true),
            WallMove::Wait { record: false }
        );
        // Its own hold ended (at the reset its text said, or by `done`)
        // and no ask holds it: the call is made again there.
        assert_eq!(
            wall_move(true, false, true, false, false, false),
            WallMove::Retry
        );
        assert_eq!(
            wall_move(true, true, false, false, false, false),
            WallMove::Retry
        );
        // Never on the first look, even with its own provider free (a
        // Claude wall records no hold of its own).
        assert_eq!(
            wall_move(true, false, false, true, false, false),
            WallMove::Wait { record: true }
        );
    }

    // Moved here by task 1713 from the tests/it cases it removed:
    // runtime_provider_switch::a_run_switches_twice_at_most_and_then_waits_in_the_hold_ask
    // and a_run_out_of_switches_waits_on_codexs_hold_and_retries_at_its_reset.
    // The kept runtime_provider_switch cases check the wiring:
    // a_codex_turn_at_its_usage_limit_moves_to_claude_without_an_ask,
    // a_codex_that_does_not_start_while_claude_is_held_waits_in_the_hold_ask and
    // no_claude_waits_on_codex_limit_then_retries_codex_without_a_claude_hold.
    #[test]
    fn a_waiting_run_joins_the_hold_ask_only_when_a_person_must_act() {
        use Provider::{Claude, Codex};
        use SwitchReason::{Authentication, LaunchFailed, UsageLimit};
        let none: &[ProviderHold] = &[];
        // Claude's login or usage limit always raises its wall, even with
        // Codex usable (switches used up).
        assert_eq!(
            wall_to_raise(false, Claude, UsageLimit, true, None, none),
            Some(Wall::UsageLimit)
        );
        assert_eq!(
            wall_to_raise(false, Claude, Authentication, true, None, none),
            Some(Wall::Authentication)
        );
        // Codex at its limit while Claude can take the run: no ask.
        assert_eq!(
            wall_to_raise(false, Codex, UsageLimit, true, None, none),
            None
        );
        // ... with Claude's hold ask open, the run joins it, of its wall.
        assert_eq!(
            wall_to_raise(
                false,
                Codex,
                UsageLimit,
                true,
                Some(HoldReason::UsageLimit),
                none
            ),
            Some(Wall::UsageLimit)
        );
        assert_eq!(
            wall_to_raise(
                false,
                Codex,
                LaunchFailed,
                true,
                Some(HoldReason::UsageLimit),
                none
            ),
            Some(Wall::UsageLimit)
        );
        assert_eq!(
            wall_to_raise(
                false,
                Codex,
                UsageLimit,
                true,
                Some(HoldReason::Authentication),
                none
            ),
            Some(Wall::Authentication)
        );
        // No headless Claude here: Codex's own limit is the wall.
        assert_eq!(
            wall_to_raise(false, Codex, UsageLimit, false, None, none),
            Some(Wall::UsageLimit)
        );
        // Two agents that do not start open none: the run waits on their
        // holds.
        let claude_launch = [hold(Claude, LaunchFailed)];
        assert_eq!(
            wall_to_raise(false, Codex, LaunchFailed, true, None, &claude_launch),
            None
        );
        assert_eq!(
            wall_to_raise(
                false,
                Claude,
                LaunchFailed,
                true,
                None,
                &[hold(Codex, LaunchFailed)]
            ),
            None
        );
        // An agent that did not start while the other provider's hold is a
        // wall: that wall.
        assert_eq!(
            wall_to_raise(
                false,
                Claude,
                LaunchFailed,
                true,
                None,
                &[hold(Codex, Authentication)]
            ),
            Some(Wall::Authentication)
        );
        // A Claude agent that did not start while Codex can take the run.
        assert_eq!(
            wall_to_raise(false, Claude, LaunchFailed, true, None, none),
            None
        );
        // Under `--no-claude` no hold ask opens: Codex keeps its own hold.
        assert_eq!(
            wall_to_raise(true, Codex, UsageLimit, true, None, none),
            None
        );
        assert_eq!(
            wall_to_raise(
                true,
                Codex,
                UsageLimit,
                false,
                Some(HoldReason::UsageLimit),
                none
            ),
            None
        );
    }

    #[test]
    fn the_switch_says_from_to_why_and_when() {
        let payload = switched_payload(
            Provider::Claude,
            CODEX,
            SwitchReason::UsageLimit,
            SwitchPhase::Answer,
            Some(3),
            1,
            Some("limit"),
        );
        assert_eq!(payload["from"], "claude");
        assert_eq!(payload["to"], "codex");
        assert_eq!(payload["worker_mode"], "headless");
        assert_eq!(payload["reason"], "usage_limit");
        assert_eq!(payload["phase"], "answer");
        assert_eq!(payload["turn"], 3);
        assert_eq!(payload["count"], 1);
    }
}
