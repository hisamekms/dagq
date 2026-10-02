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
//! which no ask shows and which ends on its own after a while.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    DomainError, Provider, RunEvent, RunId,
    claim_hold::HoldReason,
    event_kind,
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
/// task may ask for, while `held` says why a provider is held: as asked
/// when it has its adapters and its provider is not held; otherwise
/// headless on the other provider when that one is (ADR-t813-2 decisions
/// 2 and 6); otherwise not at all (no route: the task waits).
pub fn routes(
    supported: &[Worker],
    held: impl Fn(Provider) -> Option<SwitchReason>,
) -> Vec<WorkerRoute> {
    let usable = |worker: &Worker| supported.contains(worker) && held(worker.provider).is_none();
    Worker::ALL
        .iter()
        .filter_map(|&requested| {
            if usable(&requested) {
                return Some(WorkerRoute {
                    requested,
                    actual: requested,
                    switch: None,
                });
            }
            let actual = Worker {
                provider: requested.provider.other(),
                mode: WorkerMode::Headless,
            };
            usable(&actual).then(|| WorkerRoute {
                requested,
                actual,
                switch: Some(held(requested.provider).unwrap_or(SwitchReason::ExecutableMissing)),
            })
        })
        .collect()
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
        assert_eq!(routes(&all, none), WorkerRoute::direct(&all));
        // No Codex: its tasks run on headless Claude.
        let claude_only = [CLAUDE, CLAUDE_HEADLESS];
        let routes_now = routes(&claude_only, none);
        let codex = route_of(&routes_now, CODEX).unwrap();
        assert_eq!(codex.actual, CLAUDE_HEADLESS);
        assert_eq!(codex.switch, Some(SwitchReason::ExecutableMissing));
        assert_eq!(route_of(&routes_now, CLAUDE).unwrap().actual, CLAUDE);
        // Claude held: every task runs on Codex.
        let claude_held = |p: Provider| (p == Provider::Claude).then_some(SwitchReason::UsageLimit);
        let routes_now = routes(&all, claude_held);
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
        assert!(routes(&claude_only, claude_held).is_empty());
        // Codex held: its tasks run on Claude.
        let codex_held =
            |p: Provider| (p == Provider::Codex).then_some(SwitchReason::Authentication);
        let routes_now = routes(&all, codex_held);
        let codex = route_of(&routes_now, CODEX).unwrap();
        assert_eq!(codex.actual, CLAUDE_HEADLESS);
        assert_eq!(codex.switch, Some(SwitchReason::Authentication));
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
