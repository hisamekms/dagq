//! The tokens of `stats` per Execution (ADR-t1486-1): what each
//! `claude -p` / `codex exec` call (a headless turn, a headless job) and
//! each cut of an interactive session's transcript recorded, summed over
//! the window by when it ended (a cut by the time it cuts at), per actor
//! (the kind of session the supervisor started), provider and model.
//!
//! Only the records in the form of [`crate::domain::tokens::ExecutionTokens`]
//! count: an event that ends an Execution but predates that form (a
//! `turn_finished` counted from `result.usage`, a job's end without
//! tokens) and the tokens a span recorded at its close are not mixed in.
//! When the records start is `recorded_from`, and `coverage` says whether
//! the window is wholly, partly or not at all after it. The records are
//! advisory: the files they are counted from are the executing side's to
//! change, so they bill or authorize nothing.

use std::collections::{BTreeMap, HashMap};

use serde::{Serialize, Serializer};
use serde_json::Value;

use super::{context::RecordedContext, rfc3339_millis, timestamp_millis, tokens::Usd};
use crate::domain::{
    GoalId, Provider, RunEvent, RunId, TaskId, event_kind,
    sessions::{
        GOAL_REVIEW, HEADLESS_ROUTE, KINDS, OBSERVER, PLAN_REVIEW, REVIEW, RUN_SESSION,
        RUNTIME_PLANNER, SESSION_OPENED, SESSION_TOKENS, THROUGHPUT_REVIEW, TOKEN_CUT_KINDS,
        TRIAGE, WORKER,
    },
    tokens::{MODEL_UNKNOWN, TokenSource},
};

use super::sessions::INTERACTIVE;

/// The provider or model of an Execution that names none.
pub const UNKNOWN: &str = "unknown";

/// One Execution's counts, or one model's in it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    pub cost_usd: Option<Usd>,
}

impl Counts {
    /// The counts of a `tokens` object or an entry of `tokens_by_model`;
    /// `None` when it is not an object.
    fn of(value: &Value) -> Option<Self> {
        if !value.is_object() {
            return None;
        }
        let count = |key: &str| value[key].as_i64().unwrap_or(0);
        Some(Self {
            input: count("input"),
            output: count("output"),
            cache_read: count("cache_read"),
            cache_creation: count("cache_creation"),
            cost_usd: Usd::of(&value["cost_usd"]),
        })
    }

    fn kinds(&self) -> [i64; 4] {
        [
            self.input,
            self.output,
            self.cache_read,
            self.cache_creation,
        ]
    }

    /// All four kinds together.
    pub fn total(&self) -> i64 {
        self.kinds().iter().sum()
    }

    /// What is left of `self` after `parts`, never below 0, without a cost.
    fn less(&self, parts: &[(String, Self)]) -> Self {
        let mut left = self.kinds();
        for (_, part) in parts {
            for (left, part) in left.iter_mut().zip(part.kinds()) {
                *left = (*left - part).max(0);
            }
        }
        Self {
            input: left[0],
            output: left[1],
            cache_read: left[2],
            cache_creation: left[3],
            cost_usd: None,
        }
    }
}

/// One Execution, or one cut of an interactive session, as its record
/// says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    /// When it ended (a cut: the time it cuts at), unix milliseconds.
    pub at: i64,
    /// The kind of session the supervisor started it as
    /// ([`crate::domain::sessions::KINDS`]).
    pub actor: &'static str,
    /// `claude` / `codex`: the turn's `provider`, else what its
    /// `tokens_source` is read from; [`UNKNOWN`] for an unmeasured job.
    pub provider: String,
    /// `headless` for a turn, `interactive` for a cut; `None` for a job.
    pub route: Option<&'static str>,
    pub run_id: Option<RunId>,
    pub task_id: Option<TaskId>,
    pub goal_id: Option<GoalId>,
    /// `None` when its tokens could not be counted.
    pub tokens: Option<Counts>,
    /// Its tokens per model; together they are `tokens`. What no model
    /// was named for is [`UNKNOWN`]'s.
    pub by_model: Vec<(String, Counts)>,
    /// How large its context grew; `None` when its record has none
    /// ([`super::context`]).
    pub context: Option<RecordedContext>,
}

/// The actor of the event that ends an Execution, and its route; `None`
/// for any other event. `run_kind` is the kind of the run's own session
/// open last.
fn actor_of(
    event: &RunEvent,
    run_kind: Option<&'static str>,
) -> Option<(&'static str, Option<&'static str>)> {
    let job = |actor| Some((actor, None));
    match event.kind.as_str() {
        event_kind::TURN_FINISHED if event.run_id.is_some() => {
            Some((run_kind.unwrap_or(WORKER), Some(HEADLESS_ROUTE)))
        }
        event_kind::TURN_FINISHED if event.payload["planner_id"].is_i64() => {
            Some((RUNTIME_PLANNER, Some(HEADLESS_ROUTE)))
        }
        event_kind::REVIEW_FINISHED | event_kind::REVIEW_FAILED | event_kind::REVIEW_RETRIED => {
            job(REVIEW)
        }
        // A recovery job also records its tokens on its `triage_*`, which
        // are not counted again.
        event_kind::RECOVERY_FINISHED => job(TRIAGE),
        event_kind::PLAN_REVIEW_FINISHED
        | event_kind::PLAN_REVIEW_FAILED
        | event_kind::PLAN_REVIEW_DISCARDED => job(PLAN_REVIEW),
        event_kind::GOAL_REVIEW_FINISHED | event_kind::GOAL_REVIEW_FAILED => job(GOAL_REVIEW),
        event_kind::OBSERVE_FINISHED => job(OBSERVER),
        event_kind::THROUGHPUT_REVIEW_FINISHED => job(THROUGHPUT_REVIEW),
        SESSION_TOKENS => {
            let kind = event.payload["kind"].as_str()?;
            let kind = TOKEN_CUT_KINDS.into_iter().find(|cut| *cut == kind)?;
            Some((kind, Some(INTERACTIVE)))
        }
        _ => None,
    }
}

/// The provider an Execution ran on: the one its event names, else the
/// one whose output its tokens were counted from. A cut is of a Claude
/// Code transcript, measured or not.
fn provider_of(event: &RunEvent) -> String {
    let payload = &event.payload;
    if event.kind == SESSION_TOKENS {
        return Provider::Claude.as_str().to_owned();
    }
    if let Some(provider) = payload["provider"].as_str() {
        return provider.to_owned();
    }
    payload["tokens_source"]
        .as_str()
        .and_then(TokenSource::of)
        .map_or(UNKNOWN, |source| source.provider().as_str())
        .to_owned()
}

/// Every Execution `events` (ascending id) recorded in the form of
/// ADR-t1486-1, oldest first.
pub fn executions(events: &[RunEvent]) -> Vec<Execution> {
    let mut run_kinds: HashMap<&RunId, &'static str> = HashMap::new();
    let mut executions = Vec::new();
    for event in events {
        if event.kind == SESSION_OPENED
            && let Some(run) = &event.run_id
            && let Some(kind) = event.payload["kind"].as_str()
            && let Some(kind) = RUN_SESSION.into_iter().find(|own| *own == kind)
        {
            run_kinds.insert(run, kind);
            continue;
        }
        let payload = &event.payload;
        // Only the form of every Execution: it always writes the source.
        if payload.get("tokens_source").is_none() {
            continue;
        }
        let run_kind = event
            .run_id
            .as_ref()
            .and_then(|run| run_kinds.get(run).copied());
        let Some((actor, route)) = actor_of(event, run_kind) else {
            continue;
        };
        let at = if event.kind == SESSION_TOKENS {
            payload["at"].as_str().and_then(rfc3339_millis)
        } else {
            timestamp_millis(&event.created_at)
        };
        let Some(at) = at else {
            continue;
        };
        let tokens = Counts::of(&payload["tokens"]);
        let mut by_model: Vec<(String, Counts)> = Vec::new();
        if let Some(tokens) = tokens {
            by_model = payload["tokens_by_model"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    let model = entry["model"].as_str().unwrap_or(MODEL_UNKNOWN);
                    Some((model.to_owned(), Counts::of(entry)?))
                })
                .collect();
            let left = tokens.less(&by_model);
            if by_model.is_empty() {
                let model = payload["model"].as_str().unwrap_or(UNKNOWN);
                by_model.push((model.to_owned(), tokens));
            } else if left.total() > 0 {
                by_model.push((UNKNOWN.to_owned(), left));
            }
        }
        executions.push(Execution {
            at,
            actor,
            provider: provider_of(event),
            route,
            run_id: event.run_id.clone(),
            task_id: event.task_id,
            goal_id: event.goal_id,
            tokens,
            by_model,
            context: RecordedContext::of(payload),
        });
    }
    executions
}

/// The tokens of some Executions, summed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ExecutionTotals {
    /// The Executions (and cuts) recorded, measured or not.
    pub executions: usize,
    /// Of them, those whose tokens could not be counted.
    pub unmeasured: usize,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    /// All four kinds together.
    pub total: i64,
    /// The sum of the costs the provider gave (Claude Code's); null when
    /// no Execution had one.
    pub cost_usd: Option<Usd>,
    /// The Executions whose cost was recorded.
    pub cost_executions: usize,
}

impl ExecutionTotals {
    /// Add one Execution's counts (`None`: not measured).
    pub fn add(&mut self, counts: Option<&Counts>) {
        self.executions += 1;
        let Some(counts) = counts else {
            self.unmeasured += 1;
            return;
        };
        self.input += counts.input;
        self.output += counts.output;
        self.cache_read += counts.cache_read;
        self.cache_creation += counts.cache_creation;
        self.total = self.input + self.output + self.cache_read + self.cache_creation;
        if let Some(cost) = counts.cost_usd {
            self.cost_executions += 1;
            self.cost_usd = Some(self.cost_usd.unwrap_or_default().plus(cost));
        }
    }

    /// The Executions whose tokens were counted.
    pub fn measured(&self) -> usize {
        self.executions - self.unmeasured
    }
}

/// How much of a window the records cover.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Coverage {
    /// They started at or before the window's start.
    Full,
    /// They started in the window: what ended before is not in it.
    Partial,
    /// They started after the window, or have not yet.
    #[default]
    None,
}

impl Coverage {
    /// The coverage of the window `(from, until]` (no `from`: from the
    /// first event) by records that started at `first`.
    pub fn of(first: Option<i64>, from: Option<i64>, until: i64) -> Self {
        match first {
            Some(first) if first > until => Self::None,
            Some(first) if from.is_some_and(|from| first <= from) => Self::Full,
            Some(_) => Self::Partial,
            None => Self::None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Partial => "partial",
            Self::None => "none",
        }
    }
}

impl Serialize for Coverage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// One provider's Executions of an actor in the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProviderTokens {
    /// When the actor's first record on the provider ended.
    pub recorded_from: Option<String>,
    pub coverage: Coverage,
    #[serde(flatten)]
    pub totals: ExecutionTotals,
    /// Per model; an Execution on two models counts in both.
    pub by_model: BTreeMap<String, ExecutionTotals>,
}

/// One actor's Executions in the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ActorTokens {
    /// When its first record ended; null when it has none.
    pub recorded_from: Option<String>,
    pub coverage: Coverage,
    #[serde(flatten)]
    pub totals: ExecutionTotals,
    /// Every provider it has a record on, and any of the window.
    pub by_provider: BTreeMap<String, ProviderTokens>,
}

/// The Executions that ended in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ExecutionStats {
    /// The window in time: what ended after `from` (null: from the first
    /// event) up to `until`.
    pub from: Option<String>,
    pub until: String,
    /// When the first record of any actor ended; null without one.
    pub recorded_from: Option<String>,
    pub coverage: Coverage,
    #[serde(flatten)]
    pub totals: ExecutionTotals,
    /// Every kind of session, with no Execution too.
    pub by_actor: BTreeMap<&'static str, ActorTokens>,
    /// The same per actor and route (`headless` turns, `interactive`
    /// cuts), for `sessions.by_route`.
    #[serde(skip)]
    pub by_route: BTreeMap<(&'static str, &'static str), ExecutionTotals>,
    /// The window's Executions themselves, for `kpi`'s strata.
    #[serde(skip)]
    pub executions: Vec<Execution>,
}

/// The Executions of `all` (from [`executions`]) that ended in
/// `(from, until]` (unix milliseconds) and `counts` accepts (`--goal`).
pub fn window(
    all: &[Execution],
    from: Option<i64>,
    until: i64,
    counts: impl Fn(&Execution) -> bool,
) -> ExecutionStats {
    let text = |millis: i64| crate::domain::marks::utc_text(millis);
    let mut first: HashMap<(&str, &str), i64> = HashMap::new();
    for execution in all {
        first
            .entry((execution.actor, execution.provider.as_str()))
            .and_modify(|at| *at = (*at).min(execution.at))
            .or_insert(execution.at);
    }
    let first_of = |actor: &str| {
        first
            .iter()
            .filter(|((of, _), _)| *of == actor)
            .map(|(_, at)| *at)
            .min()
    };
    let recorded_from = first.values().copied().min();
    let mut stats = ExecutionStats {
        from: from.map(text),
        until: text(until),
        recorded_from: recorded_from.map(text),
        coverage: Coverage::of(recorded_from, from, until),
        ..ExecutionStats::default()
    };
    for actor in KINDS {
        let first_at = first_of(actor);
        let by_provider = first
            .iter()
            .filter(|((of, _), _)| *of == actor)
            .map(|((_, provider), at)| {
                let tokens = ProviderTokens {
                    recorded_from: Some(text(*at)),
                    coverage: Coverage::of(Some(*at), from, until),
                    ..ProviderTokens::default()
                };
                ((*provider).to_owned(), tokens)
            })
            .collect();
        stats.by_actor.insert(
            actor,
            ActorTokens {
                recorded_from: first_at.map(text),
                coverage: Coverage::of(first_at, from, until),
                by_provider,
                ..ActorTokens::default()
            },
        );
    }
    for execution in all.iter().filter(|execution| {
        from.is_none_or(|from| execution.at > from) && execution.at <= until && counts(execution)
    }) {
        let tokens = execution.tokens.as_ref();
        stats.totals.add(tokens);
        if let Some(route) = execution.route {
            stats
                .by_route
                .entry((execution.actor, route))
                .or_default()
                .add(tokens);
        }
        let actor = stats.by_actor.entry(execution.actor).or_default();
        actor.totals.add(tokens);
        let provider = actor
            .by_provider
            .entry(execution.provider.clone())
            .or_default();
        provider.totals.add(tokens);
        for (model, counts) in &execution.by_model {
            provider
                .by_model
                .entry(model.clone())
                .or_default()
                .add(Some(counts));
        }
        stats.executions.push(execution.clone());
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;
    use serde_json::json;

    const DAY: i64 = 86_400_000;

    fn event(id: i64, run: Option<&str>, kind: &str, payload: Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: run.map(|_| TaskId::new(1)),
            goal_id: None,
            run_id: run.map(|run| RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: at.to_owned(),
            actor: None,
        }
    }

    fn tokens(input: i64, output: i64) -> Value {
        json!({"input": input, "output": output, "cache_read": 0, "cache_creation": 0, "messages": 1})
    }

    fn millis(text: &str) -> i64 {
        rfc3339_millis(text).unwrap()
    }

    /// The actor is the kind the supervisor started: a run's turn is its
    /// session's kind open last, a planner's turn the runtime planner's, a
    /// job its kind's and a cut its span's; the provider and model come
    /// from the record. Records before the form of every Execution, and a
    /// recovery job's `triage_finished` beside its `recovery_finished`,
    /// are not counted.
    #[test]
    fn executions_are_read_with_their_actor_provider_and_models() {
        let run = Some("r1");
        let events = vec![
            event(
                1,
                run,
                "session_opened",
                json!({"kind": "worker"}),
                "2026-10-01T09:00:00Z",
            ),
            // Recorded before the form: no `tokens_source`.
            event(
                2,
                run,
                "turn_finished",
                json!({"tokens": tokens(1, 1), "provider": "claude"}),
                "2026-10-01T09:10:00Z",
            ),
            event(
                3,
                run,
                "turn_finished",
                json!({"tokens": tokens(100, 10), "provider": "claude", "model": "opus",
                         "tokens_source": "model_usage",
                         "tokens_by_model": [{"model": "opus", "input": 60, "output": 10},
                                             {"model": "haiku", "input": 30, "output": 0}]}),
                "2026-10-01T09:20:00Z",
            ),
            event(
                4,
                run,
                "session_opened",
                json!({"kind": "revise"}),
                "2026-10-01T10:00:00Z",
            ),
            event(
                5,
                run,
                "turn_finished",
                json!({"tokens": null, "provider": "codex", "tokens_source": null,
                         "tokens_reason": "no_result", "tokens_by_model": []}),
                "2026-10-01T10:10:00Z",
            ),
            event(
                6,
                run,
                "triage_finished",
                json!({"tokens": tokens(5, 5), "tokens_source": "model_usage"}),
                "2026-10-01T11:00:00Z",
            ),
            event(
                7,
                run,
                "recovery_finished",
                json!({"tokens": tokens(5, 5), "tokens_source": "model_usage", "tokens_by_model": []}),
                "2026-10-01T11:00:00Z",
            ),
            event(
                8,
                None,
                "plan_review_finished",
                json!({"tokens": tokens(7, 3), "tokens_source": "token_usage_record",
                         "model": "gpt-5", "tokens_by_model": []}),
                "2026-10-01T12:00:00Z",
            ),
            event(
                9,
                None,
                "turn_finished",
                json!({"planner_id": 4, "provider": "claude", "tokens": tokens(2, 2),
                         "tokens_source": "model_usage", "tokens_by_model": []}),
                "2026-10-01T12:30:00Z",
            ),
            // A cut made after midnight for the hour before it.
            event(
                10,
                None,
                "session_tokens",
                json!({"kind": "inbox", "at": "2026-10-01T23:30:00.000Z", "final": false,
                         "tokens": tokens(40, 4), "tokens_source": "transcript",
                         "tokens_by_model": [{"model": "opus", "input": 40, "output": 4}]}),
                "2026-10-02T00:05:00Z",
            ),
            event(
                11,
                None,
                "observe_finished",
                json!({"outcome": "succeeded"}),
                "2026-10-02T01:00:00Z",
            ),
        ];
        let read = executions(&events);
        let actors: Vec<(&str, &str, Option<&str>)> = read
            .iter()
            .map(|e| (e.actor, e.provider.as_str(), e.route))
            .collect();
        assert_eq!(
            actors,
            [
                ("worker", "claude", Some("headless")),
                ("revise", "codex", Some("headless")),
                ("triage", "claude", None),
                ("plan_review", "codex", None),
                ("runtime_planner", "claude", Some("headless")),
                ("inbox", "claude", Some("interactive")),
            ]
        );
        // The models' counts and what no model was named for.
        let models: Vec<(&str, i64)> = read[0]
            .by_model
            .iter()
            .map(|(model, counts)| (model.as_str(), counts.total()))
            .collect();
        assert_eq!(models, [("opus", 70), ("haiku", 30), ("unknown", 10)]);
        assert_eq!(read[1].tokens, None);
        assert!(read[1].by_model.is_empty());
        assert_eq!(read[3].by_model[0].0, "gpt-5");
        assert_eq!(read[4].by_model[0].0, "unknown");
        assert_eq!(read[5].at, millis("2026-10-01T23:30:00Z"));
    }

    /// The window counts by when each Execution ended, a cut by its time
    /// even when it was recorded the next day; `coverage` says whether the
    /// records started before, in or after the window.
    #[test]
    fn a_window_counts_what_ended_in_it_and_says_how_much_was_recorded() {
        let events = vec![
            event(
                1,
                None,
                "session_tokens",
                json!({"kind": "inbox", "at": "2026-10-01T23:30:00.000Z",
                         "tokens": tokens(10, 1), "tokens_source": "transcript", "tokens_by_model": []}),
                "2026-10-01T23:30:05Z",
            ),
            event(
                2,
                None,
                "session_tokens",
                json!({"kind": "inbox", "at": "2026-10-02T00:45:00.000Z",
                         "tokens": tokens(20, 2), "tokens_source": "transcript", "tokens_by_model": []}),
                "2026-10-02T00:45:05Z",
            ),
            // The close's cut, written a day later for the time it closed.
            event(
                3,
                None,
                "session_tokens",
                json!({"kind": "inbox", "at": "2026-10-02T01:00:00.000Z", "final": true,
                         "tokens": null, "tokens_source": null, "tokens_reason": "usage_unsupported",
                         "tokens_by_model": []}),
                "2026-10-03T08:00:00Z",
            ),
        ];
        let all = executions(&events);
        let day1 = millis("2026-10-01T00:00:00Z");
        let first = window(&all, Some(day1), day1 + DAY, |_| true);
        assert_eq!(first.totals.total, 11);
        assert_eq!(first.coverage, Coverage::Partial);
        assert_eq!(
            first.recorded_from.as_deref(),
            Some("2026-10-01T23:30:00.000Z")
        );
        let second = window(&all, Some(day1 + DAY), day1 + 2 * DAY, |_| true);
        assert_eq!(second.coverage, Coverage::Full);
        assert_eq!((second.totals.executions, second.totals.unmeasured), (2, 1));
        assert_eq!(second.totals.total, 22);
        let inbox = &second.by_actor["inbox"];
        assert_eq!(inbox.totals.total, 22);
        assert_eq!(inbox.coverage, Coverage::Full);
        assert_eq!(inbox.by_provider["claude"].totals.executions, 2);
        assert_eq!(second.by_route[&("inbox", "interactive")].total, 22);
        // An actor with no record says so, and a window before the records
        // has none.
        assert_eq!(second.by_actor["observer"].coverage, Coverage::None);
        assert_eq!(second.by_actor["observer"].recorded_from, None);
        assert_eq!(second.by_actor.len(), KINDS.len());
        let before = window(&all, Some(day1 - DAY), day1, |_| true);
        assert_eq!(before.coverage, Coverage::None);
        assert_eq!(before.totals, ExecutionTotals::default());
        let json = serde_json::to_value(&second).unwrap();
        assert_eq!(json["coverage"], "full");
        assert_eq!(json["by_actor"]["inbox"]["total"], 22);
        assert_eq!(
            json["by_actor"]["inbox"]["by_provider"]["claude"]["by_model"]["unknown"]["executions"],
            1
        );
        // `--goal` keeps what `counts` keeps.
        assert_eq!(
            window(&all, None, day1 + 3 * DAY, |_| false)
                .totals
                .executions,
            0
        );
        assert_eq!(Coverage::of(Some(5), None, 10), Coverage::Partial);
    }

    /// The end of a job that started an agent counts as an Execution even
    /// when nothing else of it is recorded (a recovery job stopped, a plan
    /// review discarded): measured, in the executions and the sums; not
    /// measured, in the executions and `unmeasured` only. An end that
    /// started no agent (a review that never ran) records no Execution and
    /// is not counted.
    #[test]
    fn a_stopped_or_discarded_job_counts_and_one_that_started_no_agent_does_not() {
        use crate::domain::{
            headless_job::JobSession,
            tokens::{ExecutionTokens, TokenSource, TokenUsage},
        };
        let ended = |session: Option<JobSession>, mut payload: Value| {
            JobSession::record_execution(session.as_ref(), &mut payload);
            payload
        };
        let measured = || {
            Some(JobSession {
                tokens: Some(ExecutionTokens {
                    tokens: Some(TokenUsage {
                        input: 30,
                        output: 7,
                        ..TokenUsage::default()
                    }),
                    source: Some(TokenSource::UsageRecord),
                    ..ExecutionTokens::default()
                }),
                ..JobSession::default()
            })
        };
        let run = Some("r1");
        let at = "2026-10-01T09:00:00Z";
        let events = vec![
            event(
                1,
                run,
                "recovery_finished",
                ended(measured(), json!({"outcome": "session_ended"})),
                at,
            ),
            event(
                2,
                run,
                "recovery_finished",
                ended(None, json!({"outcome": "session_ended"})),
                at,
            ),
            event(
                3,
                None,
                "plan_review_discarded",
                ended(measured(), json!({"edited": [2]})),
                at,
            ),
            event(
                4,
                None,
                "plan_review_discarded",
                ended(Some(JobSession::default()), json!({"edited": [2]})),
                at,
            ),
            // A review that never started: no session, no Execution.
            event(
                5,
                run,
                "review_failed",
                json!({"code": "job_failed", "duration_secs": 0}),
                at,
            ),
        ];
        let all = executions(&events);
        assert_eq!(all.len(), 4);
        let day = millis("2026-10-01T00:00:00Z");
        let stats = window(&all, Some(day), day + DAY, |_| true);
        assert_eq!(
            (
                stats.totals.executions,
                stats.totals.unmeasured,
                stats.totals.total
            ),
            (4, 2, 74)
        );
        for actor in ["triage", "plan_review"] {
            let totals = &stats.by_actor[actor].totals;
            assert_eq!(
                (totals.executions, totals.unmeasured, totals.total),
                (2, 1, 37),
                "{actor}"
            );
        }
        assert_eq!(stats.by_actor["review"].totals.executions, 0);
        // `kpi`'s `details.tokens` is this window as `stats` prints it.
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(
            (&json["executions"], &json["unmeasured"], &json["total"]),
            (&json!(4), &json!(2), &json!(74))
        );
    }
}
