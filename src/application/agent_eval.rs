//! The eval of an agent as a use case of the queue (ADR-t1728-1 decisions
//! 5 to 7): `agent eval` records a request, which the supervisor runs
//! (`supervise::agent_eval`); `agent results` and `agent result` read the
//! rounds and their scores back from the eval's events alone. Each is
//! authorized as the caller before the store is read or written: a dev
//! round needs `eval.request`, a hold-out or production round
//! `eval.request_held_out`, a rerun of a held-out set `eval.rerun` too,
//! and a read `eval.read`; a worker asks and reads on its own run only.
//! Nobody here starts an agent: a worker never runs an LLM's CLI.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::commands::{DenialLog, Gate};
use crate::domain::agent_eval::record::{self, Request, Round};
use crate::domain::agent_eval::review::{self, Metric, Metrics, ReviewOutcome, ReviewRun};
use crate::domain::agent_eval::round::held_out;
use crate::domain::agent_eval::{Case, Split};
use crate::domain::authorization::eval_request_needs;
use crate::domain::review_subagents::valid_agent_name;
use crate::domain::{ActorContext, Authorizer, Capability, EventId, EventKind, Resource, RunEvent};

/// Where the eval's events are written and read.
pub trait EvalStore: DenialLog {
    /// Record a queue event of the eval.
    fn record_eval_event(&self, kind: EventKind, payload: Value) -> Result<EventId>;
    /// Every event of the eval's kinds ([`record::KINDS`]), oldest first.
    fn eval_events(&self) -> Result<Vec<RunEvent>>;
}

/// The rounds the eval's events of `store` say, oldest first.
pub fn rounds(store: &dyn EvalStore) -> Result<Vec<Round>> {
    let events = store.eval_events()?;
    Ok(record::rounds(events.iter().map(|event| {
        (event.id.as_i64(), event.kind.as_str(), &event.payload)
    })))
}

/// `agent eval <agent>`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalRequest {
    pub agent: String,
    /// `--split`, `dev` by default.
    pub split: Split,
    /// `--k`: each case's runs, 1 or more.
    pub k: Option<u32>,
    /// `--cases`: the ids of the split's cases to run; none runs them all.
    pub cases: Option<Vec<String>>,
    /// `--rerun`: run a held-out set whose key ran already (a person's, or
    /// the inbox's at a person's word).
    pub rerun: bool,
}

/// The eval's commands of one actor on one store.
pub struct AgentEvals<'a> {
    store: &'a dyn EvalStore,
    gate: Gate<'a>,
}

impl<'a> AgentEvals<'a> {
    pub fn new(
        store: &'a dyn EvalStore,
        actor: &'a ActorContext,
        authorizer: &'a dyn Authorizer,
    ) -> Self {
        Self {
            store,
            gate: Gate { actor, authorizer },
        }
    }

    fn actor(&self) -> &ActorContext {
        self.gate.actor
    }

    /// The eval as the actor names it: on its run, for a worker.
    fn own(&self) -> Resource {
        Resource::AgentEval {
            run: self.actor().run_id().cloned(),
        }
    }

    /// `agent eval`: record the request as the actor, after the policy
    /// allowed its split and its rerun. The round's own checks (its
    /// definition and cases, the once rule, the limits) are the
    /// supervisor's when it starts it.
    pub fn request(&self, request: &EvalRequest) -> Result<Value> {
        if !valid_agent_name(&request.agent) {
            bail!(
                "agent {:?} is not kebab-case (lowercase letters and digits joined by -)",
                request.agent
            );
        }
        if request.k == Some(0) {
            bail!("--k must be 1 or more");
        }
        if request.cases.as_ref().is_some_and(Vec::is_empty) {
            bail!("--cases names no case");
        }
        let resource = self.own();
        for need in eval_request_needs(held_out(request.split), request.rerun) {
            self.gate.authorize(self.store, need, &resource)?;
        }
        let actor = self.actor();
        let recorded = Request {
            agent: request.agent.clone(),
            split: request.split,
            k: request.k,
            cases: request.cases.clone(),
            rerun: request.rerun,
            requested_by: actor.role().as_str().to_owned(),
            requested_by_id: actor.actor_id().to_owned(),
            run_id: actor.run_id().map(ToString::to_string),
            before_landing: false,
        };
        let id = self
            .store
            .record_eval_event(EventKind::AgentEvalRequested, recorded.record())?;
        Ok(json!({
            "eval_id": id,
            "agent": recorded.agent,
            "split": recorded.split.as_str(),
            "status": "waiting",
        }))
    }

    /// `agent results`: the rounds, newest first, `agent`'s only when given,
    /// at most `limit`; a worker's of its run only.
    pub fn list(&self, agent: Option<&str>, limit: usize) -> Result<Value> {
        self.gate
            .authorize(self.store, Capability::EvalRead, &self.own())?;
        let own_run = self.actor().run_id().map(ToString::to_string);
        let rounds = rounds(self.store)?;
        let listed: Vec<Value> = rounds
            .iter()
            .rev()
            .filter(|round| agent.is_none_or(|agent| round.request.agent == agent))
            .filter(|round| own_run.is_none() || round.request.run_id == own_run)
            .take(limit)
            .map(summary)
            .collect();
        Ok(json!({"evals": listed}))
    }

    /// `agent result <id>`: one round whole, if the actor may read it.
    pub fn show(&self, id: i64) -> Result<Value> {
        self.gate.refuse_ungranted(
            self.store,
            Capability::EvalRead,
            &Resource::AgentEval { run: None },
        )?;
        let rounds = rounds(self.store)?;
        let Some(round) = rounds.iter().find(|round| round.id == id) else {
            bail!("no eval {id}");
        };
        let run = round
            .request
            .run_id
            .as_deref()
            .map(crate::domain::RunId::new)
            .transpose()?;
        self.gate.authorize(
            self.store,
            Capability::EvalRead,
            &Resource::AgentEval { run },
        )?;
        Ok(detail(round))
    }
}

/// Where a round is: `waiting`, `refused`, `running` or `finished`.
pub fn status(round: &Round) -> &'static str {
    if round.refused.is_some() {
        "refused"
    } else if round.finished.is_some() {
        "finished"
    } else if round.started.is_some() {
        "running"
    } else {
        "waiting"
    }
}

/// A round in a list: its request, where it is and its result.
fn summary(round: &Round) -> Value {
    let request = &round.request;
    let finished = round.finished.as_ref();
    json!({
        "eval_id": round.id,
        "agent": request.agent,
        "split": request.split.as_str(),
        "status": status(round),
        "requested_by": request.requested_by,
        "run_id": request.run_id,
        "waiting": round.waiting,
        "refused": round.refused.as_ref().map(|refused| &refused["reason"]),
        "outcome": finished.map(|finished| &finished["outcome"]),
        "passed": finished.map(|finished| &finished["passed"]),
        "scores": finished.map(|finished| &finished["scores"]),
        "spent_usd": round.spent_usd(),
    })
}

/// A round whole: its request, its refusal or start, each run and its
/// scores.
fn detail(round: &Round) -> Value {
    let mut value = summary(round);
    value["request"] = round.request.record();
    value["refusal"] = json!(round.refused);
    value["started"] = json!(round.started_payload);
    value["runs"] = json!(
        round
            .runs
            .iter()
            .map(|run| json!({
                "case": run.case,
                "round": run.round,
                "ended": run.end.is_some(),
                "abandoned": run.end.as_ref().map(|end| end.abandoned),
                "verdict": run.end.as_ref().and_then(|end| end.result.as_ref()).map(|result| &result["verdict"]),
                "cost": run.end.as_ref().map(|end| end.cost.record()),
            }))
            .collect::<Vec<_>>()
    );
    value["finished"] = json!(round.finished);
    value
}

/// Why a round ended before every run: the next run would have spent
/// past its limit (`cost_limit`, ADR-t1728-1 decision 8). An incomplete
/// round never passes.
/// A round a supervisor took up whose definition or cases no longer read
/// at the commit it started on ends with the runs it has
/// (`cases_unreadable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incomplete {
    CostLimit,
    CasesUnreadable,
}

impl Incomplete {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CostLimit => "cost_limit",
            Self::CasesUnreadable => "cases_unreadable",
        }
    }
}

fn metrics(metrics: &Metrics) -> Value {
    let mut value = json!({"runs": metrics.runs, "errors": metrics.errors});
    for metric in Metric::ALL {
        value[metric.as_str()] = json!(metric.of(metrics));
    }
    value
}

/// The payload of `agent_eval_finished` of `round` on `cases` (the cases
/// it ran): the scores of the review's harness over the runs that ended
/// (the judgment's and the rule codes' recall and precision, the runs and
/// those without a judgment), each against `threshold` (the values under
/// it in `below`, with the disputed cases' in `with_disputed` and each
/// code's in `per_code`); `passed` only for a complete round all four of
/// whose values reach it; the failed cases with the agent's reasons; the
/// dollars spent by where they came from; and the definition's and the
/// cases' digests. `incomplete` says why it ended before every run.
pub fn finished(round: &Round, cases: &[Case], incomplete: Option<Incomplete>) -> Value {
    let started = round.started.as_ref();
    let threshold = started.map_or(crate::domain::agent_eval::DEFAULT_THRESHOLD, |s| {
        s.threshold
    });
    let ended: Vec<(&str, u32, Option<&Value>)> = round
        .runs
        .iter()
        .filter_map(|run| {
            let end = run.end.as_ref().filter(|end| !end.abandoned)?;
            Some((run.case.as_str(), run.round, end.result.as_ref()))
        })
        .collect();
    let runs: Vec<ReviewRun> = ended
        .iter()
        .map(|(case, run, result)| ReviewRun {
            case: (*case).to_owned(),
            round: *run,
            outcome: result.and_then(ReviewOutcome::from_agent_result),
        })
        .collect();
    let score = review::score(cases, &runs);
    let check = score.check(threshold);
    let failed_cases: Vec<Value> = score
        .failed_cases()
        .map(|case| {
            let reasons: Vec<Value> = ended
                .iter()
                .filter(|(id, _, _)| *id == case.id)
                .filter_map(|(_, _, result)| result.map(|result| result["reasons"].clone()))
                .collect();
            json!({
                "id": case.id,
                "runs": case.runs,
                "correct": case.correct,
                "errors": case.errors,
                "reasons": reasons,
            })
        })
        .collect();
    let mut by_source = serde_json::Map::new();
    for run in &round.runs {
        if let Some(end) = &run.end {
            let entry = by_source
                .entry(end.cost.source.as_str())
                .or_insert(json!(0.0));
            *entry = json!(entry.as_f64().unwrap_or(0.0) + end.cost.usd);
        }
    }
    let complete = incomplete.is_none();
    json!({
        "eval_id": round.id,
        "agent": round.request.agent,
        "split": round.request.split.as_str(),
        "outcome": if complete { "complete" } else { "incomplete" },
        "incomplete_reason": incomplete.map(Incomplete::as_str),
        "scores": metrics(&score.primary),
        "with_disputed": score.with_disputed.as_ref().map(metrics),
        "threshold": threshold,
        "below": check.below.iter().map(|(metric, value)| json!({"metric": metric.as_str(), "value": value})).collect::<Vec<_>>(),
        "passed": complete && check.passed,
        "per_code": score.per_code_checks(threshold).iter().map(|code| json!({
            "code": code.code, "recall": code.recall, "precision": code.precision, "meets": code.meets,
        })).collect::<Vec<_>>(),
        "failed_cases": failed_cases,
        "runs": runs.len(),
        "cost": {"spent_usd": round.spent_usd(), "by_source": by_source},
        "definition_digest": started.map(|s| &s.definition_digest),
        "case_set_digest": started.map(|s| &s.case_set_digest),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent_eval::read_case_file;
    use std::collections::BTreeSet;

    const PATCH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn cases() -> Vec<Case> {
        let case = |id: &str, verdict: &str, codes: &[&str]| {
            json!({"id": id, "source": "handmade", "made_by": "test",
                   "base_commit": "f46a7963cf708d621bf529eb855ecfb552f78b33", "patch": PATCH,
                   "review": {"input": {}, "expected": {"verdict": verdict, "codes": codes}}})
        };
        let list = json!({"agent": "demo", "role": "review", "codes": ["D-1"], "k": 1, "cases": [
            case("needs-d1", "violation", &["D-1"]),
            case("clean-one", "clean", &[]),
        ]});
        read_case_file(
            "demo",
            "dev.json",
            &list.to_string(),
            &BTreeSet::from([PATCH.to_owned()]),
        )
        .unwrap()
        .cases
    }

    /// A round's events: started on two cases, each run ended with
    /// `results` (by case, `None` for no judgment) at `cost`.
    fn round(results: &[(&str, Option<Value>, Value)]) -> Round {
        let mut events = vec![
            (
                1,
                EventKind::AgentEvalRequested,
                Request {
                    agent: "demo".into(),
                    split: Split::Dev,
                    k: None,
                    cases: None,
                    rerun: false,
                    requested_by: "user".into(),
                    requested_by_id: "user".into(),
                    run_id: None,
                    before_landing: false,
                }
                .record(),
            ),
            (
                2,
                EventKind::AgentEvalStarted,
                json!({"eval_id": 1, "provider": "claude", "definition_commit": "c",
                       "definition_digest": "dd", "case_set_digest": "cd",
                       "planned": [["needs-d1", 1], ["clean-one", 1]],
                       "estimate": {"per_run_usd": 0.4}, "max_cost_usd": 30.0,
                       "concurrency": 4, "threshold": 0.9}),
            ),
        ];
        for (n, (case, result, cost)) in results.iter().enumerate() {
            let id = 3 + 2 * i64::try_from(n).unwrap();
            events.push((
                id,
                EventKind::AgentEvalRunStarted,
                json!({"eval_id": 1, "case": case, "round": 0}),
            ));
            events.push((
                id + 1,
                EventKind::AgentEvalRunFinished,
                json!({"eval_id": 1, "case": case, "round": 0, "result": result,
                       "cost": cost, "abandoned": false}),
            ));
        }
        record::rounds(
            events
                .iter()
                .map(|(id, kind, payload)| (*id, kind.as_str(), payload)),
        )
        .remove(0)
    }

    fn verdict(verdict: &str, codes: &[&str], text: &str) -> Option<Value> {
        Some(
            json!({"agent": "demo", "status": "completed", "verdict": verdict,
                    "reasons": [{"text": text, "codes": codes}], "summary": "s"}),
        )
    }

    /// The scores of a complete round: the four values against the
    /// threshold, the runs, the dollars by where they came from and the
    /// digests (ADR-t1728-1 decisions 4 and 8).
    #[test]
    fn a_complete_round_scores_its_runs_and_names_its_costs_and_digests() {
        let round = round(&[
            (
                "needs-d1",
                verdict("revise", &["D-1"], "adds notes.txt"),
                json!({"usd": 0.25, "source": "actual"}),
            ),
            (
                "clean-one",
                verdict("pass", &[], "fine"),
                json!({"usd": 0.1, "source": "converted"}),
            ),
        ]);
        let scores = finished(&round, &cases(), None);
        assert_eq!(scores["outcome"], "complete");
        assert_eq!(scores["incomplete_reason"], Value::Null);
        assert_eq!(scores["passed"], true, "{scores}");
        for metric in Metric::ALL {
            assert_eq!(scores["scores"][metric.as_str()], 1.0, "{scores}");
        }
        assert_eq!(scores["runs"], 2);
        assert_eq!(scores["threshold"], 0.9);
        assert_eq!(scores["failed_cases"], json!([]));
        assert_eq!(
            scores["cost"]["by_source"],
            json!({"actual": 0.25, "converted": 0.1})
        );
        assert_eq!(scores["definition_digest"], "dd");
        assert_eq!(scores["case_set_digest"], "cd");
    }

    /// A round closed by its dollars is incomplete and never passes, even
    /// with every value over the threshold; a case judged wrongly is named
    /// with the agent's reasons (ADR-t1728-1 decision 8).
    #[test]
    fn an_incomplete_round_does_not_pass_and_a_failed_case_keeps_its_reasons() {
        let perfect = round(&[(
            "needs-d1",
            verdict("revise", &["D-1"], "adds notes.txt"),
            json!({"usd": 0.4, "source": "estimated"}),
        )]);
        let scores = finished(&perfect, &cases(), Some(Incomplete::CostLimit));
        assert_eq!(scores["outcome"], "incomplete");
        assert_eq!(scores["incomplete_reason"], "cost_limit");
        assert_eq!(scores["passed"], false);
        let wrong = round(&[
            (
                "needs-d1",
                verdict("pass", &[], "nothing found"),
                json!({"usd": 0.2, "source": "actual"}),
            ),
            (
                "clean-one",
                verdict("concern", &["D-1"], "notes look odd"),
                json!({"usd": 0.2, "source": "actual"}),
            ),
        ]);
        let scores = finished(&wrong, &cases(), None);
        assert_eq!(scores["passed"], false);
        let failed = scores["failed_cases"].as_array().unwrap();
        assert_eq!(failed.len(), 2, "{scores}");
        assert_eq!(failed[1]["id"], "clean-one");
        assert_eq!(failed[1]["reasons"][0][0]["text"], "notes look odd");
        assert!(!scores["below"].as_array().unwrap().is_empty());
    }
}
