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
//!   estimate as its reservation and its owner, the supervisor that runs
//!   it (`supervisor`, its token).
//! - `agent_eval_taken_up`: another supervisor took the round up from an
//!   owner that is gone (`supervisor`, the new owner, and `from`).
//! - `agent_eval_run_started`: one run of a case started (`case`, `round`
//!   from 0, `provider`, and the agent job's `prompt_bytes`).
//! - `agent_eval_run_finished`: that run ended (`case`, `round`, the
//!   agent's `result` or `null` without a judgment, `error`, `cost`
//!   ([`super::round::RunCost`]), and `abandoned` for a run a supervisor
//!   stopped without its end or one stopped at a provider's wall, which is
//!   run again but whose cost counts as spent, with `retry` for the one
//!   retry of a non-zero exit).
//! - `agent_eval_case_checked`: a case's program reviews ended its check
//!   before its agent (`case`, and `outcome`, `program` and `failure` of
//!   [`super::programs::CaseCheck`], with `programs`, those that ran, and
//!   the ends of the output of the one that stopped or failed it).
//!   Recorded once per case that needs a program.
//! - `agent_eval_finished`: the round's scores (see
//!   `application::agent_eval::finish`).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::Split;
use super::programs::CaseCheck;
use super::round::{RoundKey, RunCost, Waiting, held_out};
use crate::domain::{EventKind, Provider};

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
}

impl Request {
    pub fn record(&self) -> Value {
        json!({
            "agent": self.agent,
            "split": self.split.as_str(),
            "k": self.k,
            "cases": self.cases,
            "rerun": self.rerun,
            "requested_by": self.requested_by,
            "requested_by_id": self.requested_by_id,
            "run_id": self.run_id,
            "before_landing": self.before_landing,
        })
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
            rerun: payload["rerun"].as_bool().unwrap_or(false),
            requested_by: text("requested_by").unwrap_or_default(),
            requested_by_id: text("requested_by_id").unwrap_or_default(),
            run_id: text("run_id"),
            before_landing: payload["before_landing"].as_bool().unwrap_or(false),
        })
    }
}

/// A round as `agent_eval_started` records it: what it runs and on what.
#[derive(Debug, Clone, PartialEq)]
pub struct Started {
    pub provider: Provider,
    /// The landing branch's commit the definition and the cases were read
    /// from.
    pub definition_commit: String,
    pub definition_digest: String,
    pub case_set_digest: String,
    /// Each case's id and runs, in the list's order.
    pub planned: Vec<(String, u32)>,
    /// One run's estimate, which the check before each run adds.
    pub per_run_usd: f64,
    pub max_cost_usd: f64,
    pub concurrency: usize,
    pub threshold: f64,
}

impl Started {
    pub fn read(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            provider: payload["provider"].as_str()?.parse().ok()?,
            definition_commit: text("definition_commit")?,
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
        })
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
}

/// One run of a round.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub case: String,
    pub round: u32,
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
    pub runs: Vec<RunRecord>,
    /// What each case's program reviews said, by case id, for the cases
    /// whose check ended.
    pub checks: BTreeMap<String, CaseCheck>,
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
pub fn rounds<'a>(events: impl IntoIterator<Item = (i64, &'a str, &'a Value)>) -> Vec<Round> {
    let mut rounds: BTreeMap<i64, Round> = BTreeMap::new();
    for (id, kind, payload) in events {
        if kind == EventKind::AgentEvalRequested {
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
                        owner: None,
                        runs: Vec::new(),
                        checks: BTreeMap::new(),
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
        if kind == EventKind::AgentEvalRefused {
            round.refused = Some(payload.clone());
        } else if kind == EventKind::AgentEvalWaiting {
            round.waiting = payload["reason"].as_str().map(str::to_owned);
        } else if kind == EventKind::AgentEvalStarted {
            round.started = Started::read(payload);
            round.started_payload = Some(payload.clone());
            round.waiting = None;
            round.owner = payload["supervisor"].as_str().map(str::to_owned);
        } else if kind == EventKind::AgentEvalTakenUp {
            round.owner = payload["supervisor"].as_str().map(str::to_owned);
        } else if kind == EventKind::AgentEvalRunStarted {
            round.runs.push(RunRecord {
                case,
                round: run,
                end: None,
            });
        } else if kind == EventKind::AgentEvalRunFinished {
            let end = RunEnd {
                result: Some(payload["result"].clone()).filter(|result| !result.is_null()),
                cost: RunCost::read(&payload["cost"]).unwrap_or(RunCost {
                    usd: 0.0,
                    source: super::round::CostSource::Estimated,
                }),
                abandoned: payload["abandoned"].as_bool().unwrap_or(false),
                retry: payload["retry"].as_bool().unwrap_or(false),
            };
            if let Some(record) =
                round.runs.iter_mut().rev().find(|record| {
                    record.case == case && record.round == run && record.end.is_none()
                })
            {
                record.end = Some(end);
            }
        } else if kind == EventKind::AgentEvalCaseChecked {
            if let Some(check) = CaseCheck::read(payload) {
                round.checks.insert(case, check);
            }
        } else if kind == EventKind::AgentEvalFinished {
            round.finished = Some(payload.clone());
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
            rerun: false,
            requested_by: "user".into(),
            requested_by_id: "user".into(),
            run_id: None,
            before_landing: false,
        }
        .record()
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
        let rounds = rounds(
            events
                .iter()
                .map(|(id, kind, payload)| (*id, *kind, payload)),
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
        let rounds = rounds(
            events
                .iter()
                .map(|(id, kind, payload)| (*id, *kind, payload)),
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
        let rounds = rounds(
            events
                .iter()
                .map(|(id, kind, payload)| (*id, *kind, payload)),
        );
        assert_eq!(
            rounds[0].checks.get("a"),
            Some(&CaseCheck::Stopped {
                program: "fmt".to_owned()
            })
        );
        assert_eq!(rounds[0].left(), vec![("b".to_owned(), 0)]);
    }

    #[test]
    fn a_request_reads_back_as_recorded() {
        let request = Request {
            agent: "migration-rules".into(),
            split: Split::Dev,
            k: Some(2),
            cases: Some(vec!["a".into(), "b".into()]),
            rerun: false,
            requested_by: "worker".into(),
            requested_by_id: "worker:r1".into(),
            run_id: Some("r1".into()),
            before_landing: false,
        };
        assert_eq!(Request::read(&request.record()), Some(request));
    }
}
