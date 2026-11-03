//! The eval's record (ADR-t1728-1 decisions 4, 5 and 7): every request,
//! round, run and score is a queue event, and the state of each round is
//! read back from them alone, by any supervisor, after a restart too. No
//! table holds it. The events, each a queue event whose payload names the
//! round by `eval_id` (the id of its request's event):
//!
//! - `agent_eval_requested`: a request ([`Request`]).
//! - `agent_eval_refused`: the round was not started (`reason`, a
//!   [`super::round::RefusalReason`], `detail`, and `estimate` when one was
//!   made; `definition_bytes` and `limit` for `definition_over_limit`).
//! - `agent_eval_waiting`: its new runs wait for the review's provider
//!   (`provider`, `reason`, a [`super::round::ProviderWait`]); recorded
//!   when the reason changes.
//! - `agent_eval_started`: the round started ([`Started`]), with its
//!   estimate as its reservation, its owner, the supervisor that runs it
//!   (`supervisor`, its token), and the launch it starts its runs with
//!   (`provider`, `model`, `effort`, [`Launch`]).
//! - `agent_eval_taken_up`: another supervisor took the round up from an
//!   owner that is gone (`supervisor`, the new owner, and `from`).
//! - `agent_eval_run_started`: one run of a case started (`case`, `round`
//!   from 0, the launch the job started with, `provider`, `model` and
//!   `effort` ([`Launch`]), and the agent job's `prompt_bytes`).
//! - `agent_eval_run_finished`: that run ended (`case`, `round`, the
//!   agent's `result` or `null` without a judgment, `error`, `cost`
//!   ([`super::round::RunCost`]), and `abandoned` for a run a supervisor
//!   stopped without its end or one stopped at a provider's wall, which is
//!   run again but whose cost counts as spent, with `retry` for the one
//!   retry of a non-zero exit), with the job's `tokens` (`null` with a
//!   `tokens_reason` when not measured) and `duration_secs`.
//! - `agent_eval_case_checked`: a case's program reviews ended its check
//!   before its agent (`case`, and `outcome`, `program` and `failure` of
//!   [`super::programs::CaseCheck`], with `programs`, those that ran
//!   against the case's tree, `base_failing`, those not run for it as they
//!   exited non-zero on its base, `against`, the tree of the one that
//!   failed it, and the ends of the output of the one that stopped or
//!   failed it).
//!   Recorded once per case that needs a program.
//! - `agent_eval_finished`: the round's scores (see
//!   `application::agent_eval::finish`).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::Split;
use super::programs::CaseCheck;
use super::round::{RoundKey, RunCost, Waiting, held_out};
use crate::domain::actor_model::AgentModel;
use crate::domain::tokens::TokenUsage;
use crate::domain::{EventKind, Provider, RecordedKind};

/// The launch a round's start or a run's start records: the provider, the
/// model and the effort its agent job started with (the destination's
/// when the job was switched). `model` and `effort` are `None` for the
/// provider's own default. An event recorded before launches were (no
/// `model` or no `effort` key) reads as no launch: unknown, never a
/// default.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Launch {
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl Launch {
    /// The launch `payload` records; `None` when it records none.
    pub fn read(payload: &Value) -> Option<Self> {
        let (model, effort) = (payload.get("model")?, payload.get("effort")?);
        Some(Self {
            provider: payload["provider"].as_str()?.to_owned(),
            model: model.as_str().map(str::to_owned),
            effort: effort.as_str().map(str::to_owned),
        })
    }

    pub fn record(&self) -> Value {
        json!({"provider": self.provider, "model": self.model, "effort": self.effort})
    }
}

/// A request of an eval (`agent_eval_requested`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub agent: String,
    pub split: Split,
    /// `--k`: each case's runs, over the list's and the case's own `k`.
    pub k: Option<u32>,
    /// `--cases`: the ids of the cases to run, in the split's list; none
    /// runs every case of it.
    pub cases: Option<Vec<String>>,
    /// `--set`: the production set of the queue's side the round runs
    /// whole (its id, [`super::production`]), in place of the
    /// repository's `production.json`; `cases` is then `None`.
    pub set: Option<i64>,
    /// `--rerun`: run a held-out set whose key ran already.
    pub rerun: bool,
    /// The requester's role and actor id.
    pub requested_by: String,
    pub requested_by_id: String,
    /// The worker's run, for a worker's request: what it may read back.
    pub run_id: Option<String>,
    /// The dev a run waits for before it lands (ADR-t1728-1 decision 10),
    /// which goes before every request.
    pub before_landing: bool,
    /// The commit the definition is read from, for the dev a run waits for
    /// before it lands: the run's head, whose definition is measured on
    /// the landing branch's cases. `None` reads the definition from the
    /// landing branch's commit, as the cases are.
    pub definition_commit: Option<String>,
    /// `--provider`, `--model` and `--effort`: what a dev round's runs
    /// start with over the agent's table in `dagq.toml`
    /// ([`super::round::requested_launch`]); none written (the default)
    /// starts them as the agent's review job does. Recorded as `launch`
    /// only when written, and refused for a held-out round
    /// ([`super::round::launch_refusal`]).
    pub launch: AgentModel,
}

impl Request {
    pub fn record(&self) -> Value {
        let mut value = json!({
            "agent": self.agent,
            "split": self.split.as_str(),
            "k": self.k,
            "cases": self.cases,
            "set": self.set,
            "rerun": self.rerun,
            "requested_by": self.requested_by,
            "requested_by_id": self.requested_by_id,
            "run_id": self.run_id,
            "before_landing": self.before_landing,
            "definition_commit": self.definition_commit,
        });
        if self.launch.written() {
            value["launch"] = json!({
                "provider": self.launch.provider,
                "model": self.launch.model,
                "effort": self.launch.effort,
            });
        }
        value
    }

    pub fn read(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            agent: text("agent")?,
            split: Split::ALL
                .into_iter()
                .find(|split| Some(split.as_str()) == payload["split"].as_str())?,
            k: payload["k"].as_u64().and_then(|k| u32::try_from(k).ok()),
            cases: payload["cases"].as_array().map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
            set: payload["set"].as_i64(),
            rerun: payload["rerun"].as_bool().unwrap_or(false),
            requested_by: text("requested_by").unwrap_or_default(),
            requested_by_id: text("requested_by_id").unwrap_or_default(),
            run_id: text("run_id"),
            before_landing: payload["before_landing"].as_bool().unwrap_or(false),
            definition_commit: text("definition_commit"),
            launch: read_launch(&payload["launch"]),
        })
    }
}

/// The launch a request's `launch` writes; none for a request without one.
fn read_launch(value: &Value) -> AgentModel {
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    AgentModel {
        provider: text("provider").and_then(|name| name.parse().ok()),
        model: text("model"),
        effort: text("effort"),
    }
}

/// A round as `agent_eval_started` records it: what it runs and on what.
#[derive(Debug, Clone, PartialEq)]
pub struct Started {
    pub provider: Provider,
    /// The commit the definition was read from: the landing branch's, or
    /// the run's head for the dev a run waits for before it lands.
    pub definition_commit: String,
    /// The landing branch's commit the cases (and the program reviews)
    /// were read from, when it is not `definition_commit`.
    pub cases_commit: Option<String>,
    pub definition_digest: String,
    pub case_set_digest: String,
    /// Each case's id and runs, in the list's order.
    pub planned: Vec<(String, u32)>,
    /// One run's estimate, which the check before each run adds.
    pub per_run_usd: f64,
    pub max_cost_usd: f64,
    pub concurrency: usize,
    pub threshold: f64,
    /// For a round of a production set of the queue's side (`--set`), the
    /// id of the queue's event its set was read up to when it started: a
    /// supervisor that takes the round up reads the set's labels and
    /// decisions up to the same event, so the round is scored on the
    /// expectations it started with. `None` for any other round.
    pub set_through: Option<i64>,
}

impl Started {
    pub fn read(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            provider: payload["provider"].as_str()?.parse().ok()?,
            definition_commit: text("definition_commit")?,
            cases_commit: text("cases_commit"),
            definition_digest: text("definition_digest")?,
            case_set_digest: text("case_set_digest")?,
            planned: payload["planned"]
                .as_array()?
                .iter()
                .filter_map(|item| {
                    Some((
                        item.get(0)?.as_str()?.to_owned(),
                        u32::try_from(item.get(1)?.as_u64()?).ok()?,
                    ))
                })
                .collect(),
            per_run_usd: payload["estimate"]["per_run_usd"].as_f64()?,
            max_cost_usd: payload["max_cost_usd"].as_f64()?,
            concurrency: usize::try_from(payload["concurrency"].as_u64()?).ok()?,
            threshold: payload["threshold"].as_f64()?,
            set_through: payload["set_through"].as_i64(),
        })
    }

    /// The landing branch's commit its cases were read from.
    pub fn cases_commit(&self) -> &str {
        self.cases_commit
            .as_deref()
            .unwrap_or(&self.definition_commit)
    }

    /// Every run the round plans, each case's in order.
    pub fn runs(&self) -> Vec<(String, u32)> {
        self.planned
            .iter()
            .flat_map(|(case, k)| (0..*k).map(move |round| (case.clone(), round)))
            .collect()
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq)]
pub struct RunEnd {
    /// The agent's result as it printed it; `None` without a judgment.
    pub result: Option<Value>,
    pub cost: RunCost,
    /// Stopped by a supervisor without its end, or ended by a failure
    /// worth running again: run again.
    pub abandoned: bool,
    /// Run again as the one retry of a non-zero exit, which a run gets
    /// once.
    pub retry: bool,
    /// The job's tokens; `None` when they were not measured.
    pub tokens: Option<TokenUsage>,
    /// How long the job ran; `None` when its end records none.
    pub duration_secs: Option<u64>,
}

/// One run of a round.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub case: String,
    pub round: u32,
    /// The launch its start records; `None` for a start that records none.
    pub launch: Option<Launch>,
    pub end: Option<RunEnd>,
}

/// One round: its request and what became of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Round {
    /// The id of its request's event.
    pub id: i64,
    pub request: Request,
    pub refused: Option<Value>,
    /// The latest `agent_eval_waiting`'s reason, `None` when it does not
    /// wait (or never did).
    pub waiting: Option<String>,
    pub started: Option<Started>,
    /// The started payload as recorded.
    pub started_payload: Option<Value>,
    /// The launch its start records ([`Launch::read`]).
    pub launch: Option<Launch>,
    /// When it started and finished (Unix milliseconds of the events'
    /// `created_at`), when the events were read with their times
    /// ([`rounds_at`]).
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub runs: Vec<RunRecord>,
    /// What each case's program reviews said, by case id, for the cases
    /// whose check ended.
    pub checks: BTreeMap<String, CaseCheck>,
    /// The programs not run for a case as they exited non-zero on its
    /// base, by case id, for the cases whose check ended naming one.
    pub base_failing: BTreeMap<String, Vec<String>>,
    pub finished: Option<Value>,
    /// The supervisor that runs it: the `supervisor` of its latest start
    /// or take-up; `None` before it started, or for one recorded without.
    pub owner: Option<String>,
}

impl Round {
    /// Neither refused nor started.
    pub fn is_waiting(&self) -> bool {
        self.refused.is_none() && self.started.is_none()
    }

    /// Started and not finished.
    pub fn is_running(&self) -> bool {
        self.started.is_some() && self.finished.is_none()
    }

    /// The dollars its ended runs cost.
    pub fn spent_usd(&self) -> f64 {
        self.runs
            .iter()
            .filter_map(|run| run.end.as_ref())
            .map(|end| end.cost.usd)
            .sum()
    }

    /// The runs that ended and count: not abandoned.
    pub fn done(&self) -> BTreeSet<(String, u32)> {
        self.runs
            .iter()
            .filter(|run| run.end.as_ref().is_some_and(|end| !end.abandoned))
            .map(|run| (run.case.clone(), run.round))
            .collect()
    }

    /// The runs that started and have no end recorded: a supervisor gone
    /// left them.
    pub fn unended(&self) -> Vec<(String, u32)> {
        self.runs
            .iter()
            .filter(|run| run.end.is_none())
            .map(|run| (run.case.clone(), run.round))
            .collect()
    }

    /// The runs it plans that did not end yet, in order: none of a case a
    /// program stopped, whose agent never runs.
    pub fn left(&self) -> Vec<(String, u32)> {
        let done = self.done();
        self.started
            .as_ref()
            .map(Started::runs)
            .unwrap_or_default()
            .into_iter()
            .filter(|run| !done.contains(run))
            .filter(|(case, _)| !matches!(self.checks.get(case), Some(CaseCheck::Stopped { .. })))
            .collect()
    }

    /// The runs that used their one retry already.
    pub fn retried(&self) -> BTreeSet<(String, u32)> {
        self.runs
            .iter()
            .filter(|run| run.end.as_ref().is_some_and(|end| end.retry))
            .map(|run| (run.case.clone(), run.round))
            .collect()
    }

    /// The launches it ran with: the start's and each run's that records
    /// one, each once, in order. More than one is a round whose launch
    /// was mixed (a supervisor that took it up started its runs with
    /// another).
    pub fn launches(&self) -> Vec<Launch> {
        let mut launches: Vec<Launch> = Vec::new();
        let runs = self.runs.iter().filter_map(|run| run.launch.as_ref());
        for launch in self.launch.iter().chain(runs) {
            if !launches.contains(launch) {
                launches.push(launch.clone());
            }
        }
        launches
    }

    /// Whether its start or a run of it records no launch (an event of a
    /// build before launches were recorded): what that part ran with is
    /// not known.
    pub fn unknown_launch(&self) -> bool {
        (self.started_payload.is_some() && self.launch.is_none())
            || self.runs.iter().any(|run| run.launch.is_none())
    }

    /// The launch the whole round is known to have run with: its one
    /// launch, when its start and every run record that one; `None` when
    /// it records none, more than one, or one beside a part not known.
    pub fn one_launch(&self) -> Option<Launch> {
        match self.launches().as_slice() {
            [one] if !self.unknown_launch() => Some(one.clone()),
            _ => None,
        }
    }

    /// Mixed: more than one launch, or a known one beside a part whose
    /// launch is not known (an older start taken up by a newer build), so
    /// that its scores are never read as one launch's.
    pub fn mixed_launch(&self) -> bool {
        let known = self.launches().len();
        known > 1 || (known == 1 && self.unknown_launch())
    }

    /// The tokens and the time of a round (see [`Usage`]).
    pub fn usage(&self) -> Usage {
        let ends: Vec<&RunEnd> = self
            .runs
            .iter()
            .filter_map(|run| run.end.as_ref())
            .collect();
        let mut tokens: Option<UsageTokens> = None;
        let mut unmeasured_runs = 0;
        for end in &ends {
            match &end.tokens {
                Some(used) => {
                    let sum = tokens.get_or_insert_with(UsageTokens::default);
                    sum.input += used.input;
                    sum.cache_read += used.cache_read;
                    sum.cache_creation += used.cache_creation;
                    sum.output += used.output;
                }
                None => unmeasured_runs += 1,
            }
        }
        let ended = self.started_payload.is_some() && self.finished.is_some();
        let wall_secs = match (self.started_at, self.finished_at) {
            (Some(from), Some(to)) if ended => Some((to - from).max(0) / 1000),
            _ => None,
        };
        let durations: Vec<u64> = ends.iter().filter_map(|end| end.duration_secs).collect();
        let job_secs = (ended && !durations.is_empty()).then(|| durations.iter().sum());
        Usage {
            tokens,
            unmeasured_runs,
            wall_secs,
            job_secs,
            untimed_runs: ends.len() - durations.len(),
        }
    }

    /// The key of a started round (ADR-t1728-1 decision 7).
    pub fn key(&self) -> Option<RoundKey> {
        let started = self.started.as_ref()?;
        Some(RoundKey {
            agent: self.request.agent.clone(),
            definition_digest: started.definition_digest.clone(),
            case_set_digest: started.case_set_digest.clone(),
        })
    }
}

/// The tokens of a round's runs by kind, summed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTokens {
    pub input: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    pub output: i64,
}

/// What a round used, from its events alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// The tokens of every run that ended (an abandoned one and a retry
    /// too, and a run of a case a program stopped), summed by kind; `None`
    /// when no run's tokens were measured, never 0.
    pub tokens: Option<UsageTokens>,
    /// The runs that ended with no tokens measured, left out of `tokens`.
    pub unmeasured_runs: usize,
    /// From its start to its finish, in seconds; `None` for a round that
    /// did not start or has not finished, or read without its events'
    /// times.
    pub wall_secs: Option<i64>,
    /// The sum of its runs' `duration_secs`; `None` for a round that did
    /// not start or has not finished, or no run of which records one.
    pub job_secs: Option<u64>,
    /// The runs that ended with no `duration_secs`, left out of `job_secs`.
    pub untimed_runs: usize,
}

impl Usage {
    /// As `agent result` and `agent results` print it.
    pub fn record(&self) -> Value {
        json!({
            "tokens": self.tokens.map(|tokens| json!({
                "input": tokens.input,
                "cache_read": tokens.cache_read,
                "cache_creation": tokens.cache_creation,
                "output": tokens.output,
            })),
            "unmeasured_runs": self.unmeasured_runs,
            "wall_secs": self.wall_secs,
            "job_secs": self.job_secs,
            "untimed_runs": self.untimed_runs,
        })
    }
}

/// The kinds of the eval's events.
pub const KINDS: [EventKind; 9] = [
    EventKind::AgentEvalRequested,
    EventKind::AgentEvalRefused,
    EventKind::AgentEvalWaiting,
    EventKind::AgentEvalStarted,
    EventKind::AgentEvalTakenUp,
    EventKind::AgentEvalRunStarted,
    EventKind::AgentEvalRunFinished,
    EventKind::AgentEvalCaseChecked,
    EventKind::AgentEvalFinished,
];

/// The rounds the eval's events (`(id, kind, payload)`, oldest first)
/// say, oldest first. An event of a round with no request, or one that
/// does not read, is left out.
pub fn rounds<'a>(
    events: impl IntoIterator<Item = (i64, &'a RecordedKind, &'a Value)>,
) -> Vec<Round> {
    rounds_at(
        events
            .into_iter()
            .map(|(id, kind, payload)| (id, kind, payload, None)),
    )
}

/// [`rounds`] of events read with their `created_at`, which give each
/// round its start's and finish's times.
pub fn rounds_at<'a>(
    events: impl IntoIterator<Item = (i64, &'a RecordedKind, &'a Value, Option<&'a str>)>,
) -> Vec<Round> {
    let mut rounds: BTreeMap<i64, Round> = BTreeMap::new();
    for (id, kind, payload, created_at) in events {
        let at = created_at.and_then(super::production::event_instant);
        if *kind == EventKind::AgentEvalRequested {
            if let Some(request) = Request::read(payload) {
                rounds.insert(
                    id,
                    Round {
                        id,
                        request,
                        refused: None,
                        waiting: None,
                        started: None,
                        started_payload: None,
                        launch: None,
                        started_at: None,
                        finished_at: None,
                        owner: None,
                        runs: Vec::new(),
                        checks: BTreeMap::new(),
                        base_failing: BTreeMap::new(),
                        finished: None,
                    },
                );
            }
            continue;
        }
        let Some(round) = payload["eval_id"]
            .as_i64()
            .and_then(|eval| rounds.get_mut(&eval))
        else {
            continue;
        };
        let case = payload["case"].as_str().unwrap_or_default().to_owned();
        let run = payload["round"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        if *kind == EventKind::AgentEvalRefused {
            round.refused = Some(payload.clone());
        } else if *kind == EventKind::AgentEvalWaiting {
            round.waiting = payload["reason"].as_str().map(str::to_owned);
        } else if *kind == EventKind::AgentEvalStarted {
            round.started = Started::read(payload);
            round.started_payload = Some(payload.clone());
            round.launch = Launch::read(payload);
            round.started_at = at;
            round.waiting = None;
            round.owner = payload["supervisor"].as_str().map(str::to_owned);
        } else if *kind == EventKind::AgentEvalTakenUp {
            round.owner = payload["supervisor"].as_str().map(str::to_owned);
        } else if *kind == EventKind::AgentEvalRunStarted {
            round.runs.push(RunRecord {
                case,
                round: run,
                launch: Launch::read(payload),
                end: None,
            });
        } else if *kind == EventKind::AgentEvalRunFinished {
            let end = RunEnd {
                result: Some(payload["result"].clone()).filter(|result| !result.is_null()),
                cost: RunCost::read(&payload["cost"]).unwrap_or(RunCost {
                    usd: 0.0,
                    source: super::round::CostSource::Estimated,
                }),
                abandoned: payload["abandoned"].as_bool().unwrap_or(false),
                retry: payload["retry"].as_bool().unwrap_or(false),
                tokens: TokenUsage::from_payload(&payload["tokens"]),
                duration_secs: payload["duration_secs"].as_u64(),
            };
            if let Some(record) =
                round.runs.iter_mut().rev().find(|record| {
                    record.case == case && record.round == run && record.end.is_none()
                })
            {
                record.end = Some(end);
            }
        } else if *kind == EventKind::AgentEvalCaseChecked {
            if let Some(check) = CaseCheck::read(payload) {
                let failing = super::programs::read_base_failing(payload);
                if !failing.is_empty() {
                    round.base_failing.insert(case.clone(), failing);
                }
                round.checks.insert(case, check);
            }
        } else if *kind == EventKind::AgentEvalFinished {
            round.finished = Some(payload.clone());
            round.finished_at = at;
        }
    }
    rounds.into_values().collect()
}

/// The rounds waiting to start, for [`super::round::next_round`].
pub fn waiting(rounds: &[Round]) -> Vec<Waiting> {
    rounds
        .iter()
        .filter(|round| round.is_waiting())
        .map(|round| Waiting {
            id: round.id,
            before_landing: round.request.before_landing,
        })
        .collect()
}

/// The round that runs, if any: started and not finished. One at most runs
/// at once; the oldest is taken should there be more.
pub fn running(rounds: &[Round]) -> Option<&Round> {
    rounds.iter().find(|round| round.is_running())
}

/// The keys of the held-out rounds that started, for
/// [`super::round::holdout_refusal`].
pub fn held_out_keys(rounds: &[Round]) -> Vec<RoundKey> {
    rounds
        .iter()
        .filter(|round| held_out(round.request.split))
        .filter_map(Round::key)
        .collect()
}

/// The costs of the latest `limit` runs of `agent` on `provider` whose
/// dollars were measured (actual or converted, not estimated), newest
/// first: what one run is estimated from.
pub fn recent_costs(rounds: &[Round], agent: &str, provider: Provider, limit: usize) -> Vec<f64> {
    rounds
        .iter()
        .rev()
        .filter(|round| round.request.agent == agent)
        .filter(|round| {
            round
                .started
                .as_ref()
                .is_some_and(|s| s.provider == provider)
        })
        .flat_map(|round| round.runs.iter().rev())
        .filter_map(|run| run.end.as_ref())
        .filter(|end| end.cost.source != super::round::CostSource::Estimated)
        .map(|end| end.cost.usd)
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(agent: &str, split: Split) -> Value {
        Request {
            agent: agent.into(),
            split,
            k: None,
            cases: None,
            set: None,
            rerun: false,
            requested_by: "user".into(),
            requested_by_id: "user".into(),
            run_id: None,
            before_landing: false,
            definition_commit: None,
            launch: crate::domain::actor_model::AgentModel::default(),
        }
        .record()
    }

    /// A request's launch over the agent's table reads back as it was
    /// written, each value alone; one that writes none records no `launch`
    /// and reads as none, as an older request does.
    #[test]
    fn a_requests_launch_is_recorded_only_when_written_and_read_back() {
        let plain = request("demo", Split::Dev);
        assert!(plain.get("launch").is_none());
        assert_eq!(Request::read(&plain).unwrap().launch, AgentModel::default());
        for launch in [
            AgentModel {
                provider: Some(Provider::Codex),
                model: Some("gpt-5.5".into()),
                effort: Some("high".into()),
            },
            AgentModel {
                effort: Some("low".into()),
                ..AgentModel::default()
            },
        ] {
            let mut asked = Request::read(&plain).unwrap();
            asked.launch = launch.clone();
            let recorded = asked.record();
            assert_eq!(recorded["launch"]["effort"], json!(launch.effort));
            assert_eq!(Request::read(&recorded).unwrap(), asked);
        }
    }

    fn started(eval: i64, provider: &str, digest: &str) -> Value {
        json!({
            "eval_id": eval, "provider": provider, "definition_commit": "c",
            "definition_digest": digest, "case_set_digest": "s",
            "planned": [["a", 2], ["b", 1]], "estimate": {"per_run_usd": 0.4},
            "max_cost_usd": 30.0, "concurrency": 4, "threshold": 0.9,
            "supervisor": "s1",
        })
    }

    fn finished_run(
        eval: i64,
        case: &str,
        round: u32,
        usd: f64,
        source: &str,
        abandoned: bool,
    ) -> Value {
        json!({"eval_id": eval, "case": case, "round": round, "result": null,
               "cost": {"usd": usd, "source": source}, "abandoned": abandoned})
    }

    #[test]
    fn a_rounds_state_is_read_back_from_its_events_alone() {
        let events = vec![
            (
                1,
                EventKind::AgentEvalRequested.as_str(),
                request("adr-rules", Split::Holdout),
            ),
            (
                2,
                EventKind::AgentEvalRequested.as_str(),
                request("adr-rules", Split::Dev),
            ),
            (
                3,
                EventKind::AgentEvalStarted.as_str(),
                started(1, "claude", "d1"),
            ),
            (
                4,
                EventKind::AgentEvalRunStarted.as_str(),
                json!({"eval_id": 1, "case": "a", "round": 0}),
            ),
            (
                5,
                EventKind::AgentEvalRunStarted.as_str(),
                json!({"eval_id": 1, "case": "a", "round": 1}),
            ),
            (
                6,
                EventKind::AgentEvalRunFinished.as_str(),
                finished_run(1, "a", 0, 0.3, "actual", false),
            ),
            // A run a gone supervisor left, stopped: its estimate is spent.
            (
                7,
                EventKind::AgentEvalRunFinished.as_str(),
                finished_run(1, "a", 1, 0.4, "estimated", true),
            ),
            (
                8,
                EventKind::AgentEvalRunStarted.as_str(),
                json!({"eval_id": 1, "case": "a", "round": 1}),
            ),
            (
                9,
                EventKind::AgentEvalRequested.as_str(),
                request("other", Split::Dev),
            ),
            (
                10,
                EventKind::AgentEvalRefused.as_str(),
                json!({"eval_id": 9, "reason": "cost_unknown"}),
            ),
            (
                11,
                EventKind::AgentEvalTakenUp.as_str(),
                json!({"eval_id": 1, "supervisor": "s2", "from": "s1"}),
            ),
        ];
        let kinds: Vec<RecordedKind> = events.iter().map(|(_, kind, _)| (*kind).into()).collect();
        let rounds = rounds(
            events
                .iter()
                .zip(&kinds)
                .map(|((id, _, payload), kind)| (*id, kind, payload)),
        );
        assert_eq!(rounds.len(), 3);
        let first = &rounds[0];
        assert_eq!(
            first.owner.as_deref(),
            Some("s2"),
            "the latest take-up owns it"
        );
        assert_eq!(rounds[1].owner, None, "a waiting round has no owner");
        assert!(first.is_running());
        assert!((first.spent_usd() - 0.7).abs() < 1e-9);
        assert_eq!(first.done(), BTreeSet::from([("a".to_owned(), 0)]));
        assert_eq!(first.unended(), vec![("a".to_owned(), 1)]);
        // A takeover's abandoned end is no retry.
        assert!(first.retried().is_empty());
        assert_eq!(first.left(), vec![("a".to_owned(), 1), ("b".to_owned(), 0)]);
        assert_eq!(running(&rounds).map(|round| round.id), Some(1));
        assert_eq!(
            waiting(&rounds),
            vec![Waiting {
                id: 2,
                before_landing: false
            }]
        );
        assert!(rounds[2].refused.is_some());
        assert_eq!(held_out_keys(&rounds).len(), 1);
        assert_eq!(
            recent_costs(&rounds, "adr-rules", Provider::Claude, 20),
            vec![0.3]
        );
        assert!(recent_costs(&rounds, "adr-rules", Provider::Codex, 20).is_empty());
    }

    /// A run that used its one retry keeps it across a takeover.
    #[test]
    fn a_retry_is_read_back_from_its_end() {
        let mut retried = finished_run(1, "a", 0, 0.3, "actual", true);
        retried["retry"] = json!(true);
        let events = [
            (
                1,
                EventKind::AgentEvalRequested.as_str(),
                request("x", Split::Dev),
            ),
            (
                2,
                EventKind::AgentEvalStarted.as_str(),
                started(1, "claude", "d"),
            ),
            (
                3,
                EventKind::AgentEvalRunStarted.as_str(),
                json!({"eval_id": 1, "case": "a", "round": 0}),
            ),
            (4, EventKind::AgentEvalRunFinished.as_str(), retried),
        ];
        let kinds: Vec<RecordedKind> = events.iter().map(|(_, kind, _)| (*kind).into()).collect();
        let rounds = rounds(
            events
                .iter()
                .zip(&kinds)
                .map(|((id, _, payload), kind)| (*id, kind, payload)),
        );
        assert_eq!(rounds[0].retried(), BTreeSet::from([("a".to_owned(), 0)]));
        assert!(rounds[0].left().contains(&("a".to_owned(), 0)));
    }

    /// A case a program stopped is read back from its check, and none of
    /// its runs is left: a supervisor that takes the round up never starts
    /// its agent.
    #[test]
    fn a_case_a_program_stopped_leaves_no_run() {
        let mut stopped = CaseCheck::Stopped {
            program: "fmt".to_owned(),
        }
        .record();
        stopped["eval_id"] = json!(1);
        stopped["case"] = json!("a");
        let events = [
            (
                1,
                EventKind::AgentEvalRequested.as_str(),
                request("x", Split::Dev),
            ),
            (
                2,
                EventKind::AgentEvalStarted.as_str(),
                started(1, "claude", "d"),
            ),
            (3, EventKind::AgentEvalCaseChecked.as_str(), stopped),
        ];
        let kinds: Vec<RecordedKind> = events.iter().map(|(_, kind, _)| (*kind).into()).collect();
        let rounds = rounds(
            events
                .iter()
                .zip(&kinds)
                .map(|((id, _, payload), kind)| (*id, kind, payload)),
        );
        assert_eq!(
            rounds[0].checks.get("a"),
            Some(&CaseCheck::Stopped {
                program: "fmt".to_owned()
            })
        );
        assert_eq!(rounds[0].left(), vec![("b".to_owned(), 0)]);
    }

    /// A round of a production set keeps the event its set was read up to,
    /// which a supervisor that takes it up reads the set at again; any
    /// other round has none.
    #[test]
    fn a_set_rounds_start_keeps_where_its_set_was_read() {
        let mut payload = started(1, "claude", "d");
        assert_eq!(Started::read(&payload).unwrap().set_through, None);
        payload["set_through"] = json!(42);
        assert_eq!(Started::read(&payload).unwrap().set_through, Some(42));
    }

    /// The dev a run waits for before it lands reads its definition from
    /// the run's head and its cases from the landing branch's commit, which
    /// a supervisor that takes it up reads them from again: the cases the
    /// run changed on its branch are never the ones it is scored on. Any
    /// other round reads both from the one commit.
    #[test]
    fn a_round_before_landing_reads_its_cases_from_the_landing_branch_not_the_run() {
        let mut payload = started(1, "claude", "d");
        payload["definition_commit"] = json!("run-head");
        payload["cases_commit"] = json!("main-head");
        let before_landing = Started::read(&payload).unwrap();
        assert_eq!(before_landing.definition_commit, "run-head");
        assert_eq!(before_landing.cases_commit(), "main-head");
        let other = Started::read(&started(1, "claude", "d")).unwrap();
        assert_eq!(other.cases_commit(), "c");
    }

    #[test]
    fn a_request_reads_back_as_recorded() {
        let request = Request {
            agent: "migration-rules".into(),
            split: Split::Dev,
            k: Some(2),
            cases: Some(vec!["a".into(), "b".into()]),
            set: None,
            rerun: false,
            requested_by: "worker".into(),
            requested_by_id: "worker:r1".into(),
            run_id: Some("r1".into()),
            before_landing: false,
            definition_commit: Some("abc".into()),
            launch: crate::domain::actor_model::AgentModel::default(),
        };
        assert_eq!(Request::read(&request.record()), Some(request));
    }

    /// The events `(id, kind, payload, created_at)` read as rounds.
    fn timed(events: &[(i64, &str, Value, &str)]) -> Vec<Round> {
        let kinds: Vec<RecordedKind> = events
            .iter()
            .map(|(_, kind, _, _)| (*kind).into())
            .collect();
        rounds_at(
            events
                .iter()
                .zip(&kinds)
                .map(|((id, _, payload, at), kind)| (*id, kind, payload, Some(*at))),
        )
    }

    fn launched(mut payload: Value, model: Option<&str>, effort: Option<&str>) -> Value {
        payload["model"] = json!(model);
        payload["effort"] = json!(effort);
        payload
    }

    /// A round's launch is read from its start and each run's from its
    /// own start: a run started with another launch (a supervisor that
    /// took the round up on the role's launch) makes the round mixed, with
    /// each launch once and each run's kept. A start recorded with no
    /// launch's keys reads as unknown, never as the provider's default.
    #[test]
    fn a_rounds_launch_is_read_from_its_start_and_runs_and_a_mix_is_kept() {
        let haiku = Launch {
            provider: "claude".into(),
            model: Some("claude-haiku-5-5".into()),
            effort: Some("low".into()),
        };
        let role = Launch {
            provider: "claude".into(),
            model: None,
            effort: Some("high".into()),
        };
        let run = |case: &str, model: Option<&str>, effort: Option<&str>| {
            launched(
                json!({"eval_id": 1, "case": case, "round": 0, "provider": "claude"}),
                model,
                effort,
            )
        };
        let t = "2026-11-02T00:00:00Z";
        let events = [
            (
                1,
                EventKind::AgentEvalRequested.as_str(),
                request("x", Split::Dev),
                t,
            ),
            (
                2,
                EventKind::AgentEvalStarted.as_str(),
                launched(
                    started(1, "claude", "d"),
                    Some("claude-haiku-5-5"),
                    Some("low"),
                ),
                t,
            ),
            (
                3,
                EventKind::AgentEvalRunStarted.as_str(),
                run("a", Some("claude-haiku-5-5"), Some("low")),
                t,
            ),
            (
                4,
                EventKind::AgentEvalRunStarted.as_str(),
                run("b", None, Some("high")),
                t,
            ),
        ];
        let rounds = timed(&events);
        assert!(rounds[0].mixed_launch() && !rounds[0].unknown_launch());
        assert_eq!(rounds[0].one_launch(), None);
        assert_eq!(rounds[0].launch.as_ref(), Some(&haiku));
        assert_eq!(rounds[0].runs[0].launch.as_ref(), Some(&haiku));
        assert_eq!(rounds[0].runs[1].launch.as_ref(), Some(&role));
        assert_eq!(rounds[0].launches(), vec![haiku.clone(), role]);
        // One launch throughout is no mix.
        let rounds = timed(&events[..3]);
        assert_eq!(rounds[0].launches(), vec![haiku.clone()]);
        assert_eq!(rounds[0].one_launch(), Some(haiku.clone()));
        assert!(!rounds[0].mixed_launch() && !rounds[0].unknown_launch());
        // An older start and run with no launch's keys: unknown.
        let older = [
            (
                1,
                EventKind::AgentEvalRequested.as_str(),
                request("x", Split::Dev),
                t,
            ),
            (
                2,
                EventKind::AgentEvalStarted.as_str(),
                started(1, "claude", "d"),
                t,
            ),
            (
                3,
                EventKind::AgentEvalRunStarted.as_str(),
                json!({"eval_id": 1, "case": "a", "round": 0, "provider": "claude"}),
                t,
            ),
        ];
        let rounds = timed(&older);
        assert_eq!(rounds[0].launch, None);
        assert_eq!(rounds[0].runs[0].launch, None);
        assert!(rounds[0].launches().is_empty());
        assert!(rounds[0].unknown_launch() && !rounds[0].mixed_launch());
        assert_eq!(rounds[0].one_launch(), None);
        // The older round taken up by a newer build whose run records a
        // launch: the known launch is not the whole round's, which reads
        // as mixed with a part not known, never as that launch's.
        let mut taken_up = older.to_vec();
        taken_up.push((
            4,
            EventKind::AgentEvalRunStarted.as_str(),
            run("b", Some("claude-haiku-5-5"), Some("low")),
            t,
        ));
        let rounds = timed(&taken_up);
        assert_eq!(rounds[0].launches(), vec![haiku]);
        assert_eq!(rounds[0].one_launch(), None);
        assert!(rounds[0].unknown_launch() && rounds[0].mixed_launch());
    }

    fn ended(
        case: &str,
        abandoned: bool,
        tokens: Option<(i64, i64, i64, i64)>,
        secs: Option<u64>,
    ) -> Value {
        let mut payload = finished_run(1, case, 0, 0.1, "actual", abandoned);
        match tokens {
            Some((input, cache_read, cache_creation, output)) => {
                payload["tokens"] = json!({"input": input, "cache_read": cache_read,
                    "cache_creation": cache_creation, "output": output, "messages": 1});
                payload["tokens_reason"] = Value::Null;
            }
            None => {
                payload["tokens"] = Value::Null;
                payload["tokens_reason"] = json!("tokens_not_read");
            }
        }
        if let Some(secs) = secs {
            payload["duration_secs"] = json!(secs);
        }
        payload
    }

    /// A round's tokens are summed by kind over every run that ended, an
    /// abandoned one run again included, and a run with none measured is
    /// counted apart; its wall time runs from its start's event to its
    /// finish's and its jobs' time sums the runs' `duration_secs`, a run
    /// without one counted apart.
    #[test]
    fn a_rounds_usage_sums_every_ended_runs_tokens_and_times() {
        let start = |id| {
            (
                id,
                EventKind::AgentEvalRunStarted.as_str(),
                json!({"eval_id": 1, "case": "a", "round": 0}),
                "2026-11-02T00:00:10Z",
            )
        };
        let events = [
            (
                1,
                EventKind::AgentEvalRequested.as_str(),
                request("x", Split::Dev),
                "2026-11-02T00:00:00Z",
            ),
            (
                2,
                EventKind::AgentEvalStarted.as_str(),
                started(1, "claude", "d"),
                "2026-11-02T00:00:05Z",
            ),
            start(3),
            // Abandoned and run again: its tokens were used.
            (
                4,
                EventKind::AgentEvalRunFinished.as_str(),
                ended("a", true, Some((10, 100, 20, 5)), Some(30)),
                "2026-11-02T00:00:40Z",
            ),
            start(5),
            (
                6,
                EventKind::AgentEvalRunFinished.as_str(),
                ended("a", false, Some((1, 2, 3, 4)), Some(12)),
                "2026-11-02T00:01:00Z",
            ),
            start(7),
            (
                8,
                EventKind::AgentEvalRunFinished.as_str(),
                ended("a", false, None, None),
                "2026-11-02T00:01:10Z",
            ),
            (
                9,
                EventKind::AgentEvalFinished.as_str(),
                json!({"eval_id": 1}),
                "2026-11-02T00:02:05Z",
            ),
        ];
        let usage = timed(&events)[0].usage();
        assert_eq!(
            usage,
            Usage {
                tokens: Some(UsageTokens {
                    input: 11,
                    cache_read: 102,
                    cache_creation: 23,
                    output: 9
                }),
                unmeasured_runs: 1,
                wall_secs: Some(120),
                job_secs: Some(42),
                untimed_runs: 1,
            }
        );
        assert_eq!(
            usage.record()["tokens"],
            json!({"input": 11, "cache_read": 102, "cache_creation": 23, "output": 9})
        );
        // Not finished: no time yet, the tokens so far.
        let running = timed(&events[..8])[0].usage();
        assert_eq!((running.wall_secs, running.job_secs), (None, None));
        assert_eq!(running.tokens.map(|t| t.input), Some(11));
        // No run measured: the total is null, never 0.
        let unmeasured = [
            events[0].clone(),
            events[1].clone(),
            start(3),
            (
                4,
                EventKind::AgentEvalRunFinished.as_str(),
                ended("a", false, None, Some(7)),
                "2026-11-02T00:00:40Z",
            ),
            events[8].clone(),
        ];
        let usage = timed(&unmeasured)[0].usage();
        assert_eq!((usage.tokens, usage.unmeasured_runs), (None, 1));
        assert_eq!(usage.record()["tokens"], Value::Null);
        assert_eq!(usage.job_secs, Some(7));
        // Refused, never started: no time.
        let refused = [
            events[0].clone(),
            (
                2,
                EventKind::AgentEvalRefused.as_str(),
                json!({"eval_id": 1, "reason": "cost_unknown"}),
                "2026-11-02T00:00:05Z",
            ),
        ];
        let usage = timed(&refused)[0].usage();
        assert_eq!(
            (usage.tokens, usage.wall_secs, usage.job_secs),
            (None, None, None)
        );
        // Read without times: the wall time is not known.
        let untimed: Vec<(i64, RecordedKind, Value)> = events
            .iter()
            .map(|(id, kind, payload, _)| (*id, (*kind).into(), payload.clone()))
            .collect();
        let usage = rounds(
            untimed
                .iter()
                .map(|(id, kind, payload)| (*id, kind, payload)),
        )[0]
        .usage();
        assert_eq!((usage.wall_secs, usage.job_secs), (None, Some(42)));
    }
}
