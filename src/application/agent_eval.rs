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
use crate::domain::agent_eval::programs;
use crate::domain::agent_eval::record::{self, Request, Round};
use crate::domain::agent_eval::review::{self, Metric, Metrics, ReviewOutcome, ReviewRun};
use crate::domain::agent_eval::round::{held_out, subset_refusal};
use crate::domain::agent_eval::{Case, Split};
use crate::domain::authorization::eval_request_needs;
use crate::domain::review_builtins;
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
        (event.id.as_i64(), &event.kind, &event.payload)
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
    /// A held-out split takes them only with `rerun` (ADR-t2229-1).
    pub cases: Option<Vec<String>>,
    /// `--set`: a production set of the queue's side (the id of its
    /// request, `agent cases`), run whole as production; never with
    /// `cases`, which picks a part of a split of the repository's lists.
    pub set: Option<i64>,
    /// `--rerun`: run a held-out set whose key ran already (a person's, or
    /// the inbox's at a person's word).
    pub rerun: bool,
}

impl EvalRequest {
    /// The split the round runs: production for a set of the queue's side,
    /// else `--split`.
    pub fn split(&self) -> Split {
        if self.set.is_some() {
            Split::Production
        } else {
            self.split
        }
    }
}

/// Why `--set` and `--cases` are refused together: a set is run whole, and
/// a part of it would be a key of its own.
pub const SET_WITH_CASES: &str = "--set runs a production set whole and takes no --cases: name the set alone, or a split's cases without --set";

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
    /// allowed its split and its rerun; then a part of a held-out split's
    /// cases without a rerun is refused unrecorded (ADR-t2229-1). The round's
    /// own checks (its definition and cases, the once rule, the limits)
    /// are the supervisor's when it starts it.
    pub fn request(&self, request: &EvalRequest) -> Result<Value> {
        // A project's agent or a built-in agent; never the whole review.
        if let Err(why) = review_builtins::evaluated(&request.agent) {
            bail!(why);
        }
        if request.k == Some(0) {
            bail!("--k must be 1 or more");
        }
        if request.cases.as_ref().is_some_and(Vec::is_empty) {
            bail!("--cases names no case");
        }
        if request.set.is_some() && request.cases.is_some() {
            bail!(SET_WITH_CASES);
        }
        if request.set.is_some() && request.split == Split::Holdout {
            bail!("--set runs a production set: it takes no --split holdout");
        }
        let split = request.split();
        let resource = self.own();
        for need in eval_request_needs(held_out(split), request.rerun) {
            self.gate.authorize(self.store, need, &resource)?;
        }
        if let Some(refused) = subset_refusal(split, request.cases.is_some(), request.rerun) {
            bail!(refused.detail);
        }
        let actor = self.actor();
        let recorded = Request {
            agent: request.agent.clone(),
            split,
            k: request.k,
            cases: request.cases.clone(),
            set: request.set,
            rerun: request.rerun,
            requested_by: actor.role().as_str().to_owned(),
            requested_by_id: actor.actor_id().to_owned(),
            run_id: actor.run_id().map(ToString::to_string),
            before_landing: false,
            definition_commit: None,
        };
        let id = self
            .store
            .record_eval_event(EventKind::AgentEvalRequested, recorded.record())?;
        Ok(json!({
            "eval_id": id,
            "agent": recorded.agent,
            "split": recorded.split.as_str(),
            "set": recorded.set,
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
        "set": request.set,
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
/// A program review of a case could not start or ran past its time, so
/// whether the case breaks its check is not known (`program_failed`,
/// ADR-t1728-1 (i)); no run starts after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incomplete {
    CostLimit,
    CasesUnreadable,
    ProgramFailed,
}

impl Incomplete {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CostLimit => "cost_limit",
            Self::CasesUnreadable => "cases_unreadable",
            Self::ProgramFailed => "program_failed",
        }
    }

    /// Why a round that ends now is incomplete: a program that could not
    /// tell (`program_failed`) over runs left for its dollars
    /// (`cost_limit`); `None` when every run it planned ended.
    pub fn of(program_failed: bool, runs_left: bool) -> Option<Self> {
        if program_failed {
            Some(Self::ProgramFailed)
        } else if runs_left {
            Some(Self::CostLimit)
        } else {
            None
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
/// cases' digests, and, for a built-in, its name, build and definition's
/// digest as the start recorded them (`builtin`, `null` for a project's
/// agent). `incomplete` says why it ended before every run. The
/// cases a program review stopped are out of the agent's scores and
/// counted in `program_stopped` with each one's id and program
/// ([`programs::stopped_record`]); those with a program not run for them
/// as it failed on their base, in `program_base_failing`
/// ([`programs::base_failing_record`]).
pub fn finished(round: &Round, all_cases: &[Case], incomplete: Option<Incomplete>) -> Value {
    let cases = &programs::scored_cases(all_cases, &round.checks);
    let started = round.started.as_ref();
    let threshold = started.map_or(crate::domain::agent_eval::DEFAULT_THRESHOLD, |s| {
        s.threshold
    });
    let ended: Vec<(&str, u32, Option<&Value>)> = round
        .runs
        .iter()
        .filter(|run| {
            !matches!(
                round.checks.get(&run.case),
                Some(programs::CaseCheck::Stopped { .. })
            )
        })
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
        "program_stopped": programs::stopped_record(all_cases, &round.checks),
        "program_base_failing": programs::base_failing_record(all_cases, &round.base_failing),
        "runs": runs.len(),
        "cost": {"spent_usd": round.spent_usd(), "by_source": by_source},
        "definition_digest": started.map(|s| &s.definition_digest),
        "builtin": round.started_payload.as_ref().and_then(|payload| payload.get("builtin")).filter(|builtin| !builtin.is_null()),
        "case_set_digest": started.map(|s| &s.case_set_digest),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent_eval::read_case_file;
    use crate::domain::event_kind::RecordedKind;
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
                    set: None,
                    rerun: false,
                    requested_by: "user".into(),
                    requested_by_id: "user".into(),
                    run_id: None,
                    before_landing: false,
                    definition_commit: None,
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
        let events: Vec<(i64, RecordedKind, Value)> = events
            .into_iter()
            .map(|(id, kind, payload)| (id, kind.into(), payload))
            .collect();
        record::rounds(
            events
                .iter()
                .map(|(id, kind, payload)| (*id, kind, payload)),
        )
        .remove(0)
    }

    fn verdict(verdict: &str, codes: &[&str], text: &str) -> Option<Value> {
        Some(
            json!({"agent": "demo", "status": "completed", "verdict": verdict,
                    "reasons": [{"text": text, "codes": codes}], "summary": "s"}),
        )
    }

    /// A store of the eval's events in memory.
    #[derive(Default)]
    struct Store {
        events: std::sync::Mutex<Vec<Value>>,
    }

    impl DenialLog for Store {
        fn record_denial(&self, _: Value) -> Result<()> {
            Ok(())
        }
    }

    impl EvalStore for Store {
        fn record_eval_event(&self, _: EventKind, payload: Value) -> Result<EventId> {
            let mut events = self.events.lock().unwrap();
            events.push(payload);
            Ok(EventId::new(i64::try_from(events.len()).unwrap()))
        }
        fn eval_events(&self) -> Result<Vec<RunEvent>> {
            Ok(Vec::new())
        }
    }

    fn eval(split: Split, cases: Option<Vec<String>>, set: Option<i64>) -> EvalRequest {
        EvalRequest {
            agent: "adr-rules".into(),
            split,
            k: None,
            cases,
            set,
            rerun: false,
        }
    }

    /// A production set of the queue's side is chosen whole by `--set`,
    /// apart from `--cases`: the two together are refused with why, the
    /// set's request is a production round with no cases, held out as
    /// production is, and a split's `--cases` goes on as it did.
    #[test]
    fn a_set_is_run_whole_and_never_with_cases() {
        use crate::domain::StaticPolicy;
        use crate::domain::actor::ActorRole;
        let store = Store::default();
        let user = ActorContext::instance(ActorRole::User, "1");
        let evals = AgentEvals::new(&store, &user, &StaticPolicy);
        let error = evals
            .request(&eval(Split::Dev, Some(vec!["a".into()]), Some(3)))
            .unwrap_err();
        assert_eq!(error.to_string(), SET_WITH_CASES);
        assert!(evals.request(&eval(Split::Holdout, None, Some(3))).is_err());
        assert!(store.events.lock().unwrap().is_empty());
        let asked = evals.request(&eval(Split::Dev, None, Some(3))).unwrap();
        assert_eq!(asked["split"], "production");
        assert_eq!(asked["set"], 3);
        let recorded = Request::read(&store.events.lock().unwrap()[0]).unwrap();
        assert_eq!(recorded.cases, None);
        assert_eq!(recorded.set, Some(3));
        assert_eq!(recorded.split, Split::Production);
        // A part of a split is still `--cases`, without a set.
        evals
            .request(&eval(Split::Dev, Some(vec!["a".into()]), None))
            .unwrap();
        let recorded = Request::read(&store.events.lock().unwrap()[1]).unwrap();
        assert_eq!(recorded.set, None);
        assert_eq!(recorded.cases, Some(vec!["a".to_owned()]));
        // A worker's set is a production round, which it may not ask for.
        let worker = ActorContext::worker(
            &crate::domain::RunId::new("r1").unwrap(),
            crate::domain::TaskId::new(1),
        );
        let evals = AgentEvals::new(&store, &worker, &StaticPolicy);
        let error = evals.request(&eval(Split::Dev, None, Some(3))).unwrap_err();
        assert!(
            error.to_string().contains("eval.request_held_out"),
            "{error:#}"
        );
    }

    /// A held-out split's `--cases` without `--rerun` is refused unrecorded
    /// with why (ADR-t2229-1); a rerun's (a person's) and dev's go on, and
    /// so does a held-out split or a production set asked for whole.
    #[test]
    fn a_held_out_part_of_cases_is_refused_but_a_rerun_or_dev_goes_on() {
        use crate::domain::StaticPolicy;
        use crate::domain::actor::ActorRole;
        use crate::domain::agent_eval::round::HELD_OUT_SUBSET;
        let store = Store::default();
        let user = ActorContext::instance(ActorRole::User, "1");
        let evals = AgentEvals::new(&store, &user, &StaticPolicy);
        for split in [Split::Holdout, Split::Production] {
            let error = evals
                .request(&eval(split, Some(vec!["a".into()]), None))
                .unwrap_err();
            assert_eq!(error.to_string(), HELD_OUT_SUBSET);
        }
        assert!(store.events.lock().unwrap().is_empty());
        for split in [Split::Holdout, Split::Production] {
            let rerun = EvalRequest {
                rerun: true,
                ..eval(split, Some(vec!["a".into()]), None)
            };
            evals.request(&rerun).unwrap();
        }
        evals
            .request(&eval(Split::Dev, Some(vec!["a".into()]), None))
            .unwrap();
        evals.request(&eval(Split::Holdout, None, None)).unwrap();
        evals.request(&eval(Split::Dev, None, Some(3))).unwrap();
        let recorded: Vec<Request> = store
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|event| Request::read(event).unwrap())
            .collect();
        assert_eq!(recorded.len(), 5);
        assert!(recorded[..2].iter().all(|r| r.rerun && r.cases.is_some()));
        assert_eq!(recorded[2].split, Split::Dev);
        assert_eq!(
            (recorded[3].split, &recorded[3].cases),
            (Split::Holdout, &None)
        );
        assert_eq!(
            (recorded[4].split, recorded[4].set),
            (Split::Production, Some(3))
        );
    }

    /// A built-in agent's eval is asked for by its name; the whole review
    /// (`builtin:acceptance`), a built-in that is not carried
    /// (`builtin:docs`) and another unknown name are refused before
    /// anything is recorded (acceptance 5).
    #[test]
    fn a_builtin_is_measured_by_its_name_and_the_whole_review_never() {
        use crate::domain::StaticPolicy;
        use crate::domain::actor::ActorRole;
        let store = Store::default();
        let user = ActorContext::instance(ActorRole::User, "1");
        let evals = AgentEvals::new(&store, &user, &StaticPolicy);
        let named = |agent: &str, split: Split| EvalRequest {
            agent: agent.into(),
            ..eval(split, None, None)
        };
        for refused in [
            "builtin:acceptance",
            "builtin:docs",
            "builtin:nope",
            "Not-Kebab",
        ] {
            let error = evals.request(&named(refused, Split::Dev)).unwrap_err();
            assert!(error.to_string().contains(refused), "{error:#}");
        }
        assert!(store.events.lock().unwrap().is_empty());
        for split in [Split::Dev, Split::Holdout] {
            let asked = evals.request(&named("builtin:tests", split)).unwrap();
            assert_eq!(asked["agent"], "builtin:tests");
            assert_eq!(asked["split"], split.as_str());
        }
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

    /// A case a program review stopped is out of the agent's scores (its
    /// judgment and codes, its failed cases) and named in
    /// `program_stopped` with the program (ADR-t1728-1 (i)).
    #[test]
    fn a_case_a_program_stopped_is_out_of_the_scores_and_named() {
        let mut stopped = round(&[(
            "needs-d1",
            verdict("revise", &["D-1"], "adds notes.txt"),
            json!({"usd": 0.25, "source": "actual"}),
        )]);
        stopped.checks.insert(
            "clean-one".to_owned(),
            programs::CaseCheck::Stopped {
                program: "fmt".to_owned(),
            },
        );
        stopped
            .checks
            .insert("needs-d1".to_owned(), programs::CaseCheck::Passed);
        let scores = finished(&stopped, &cases(), None);
        assert_eq!(scores["outcome"], "complete");
        assert_eq!(scores["passed"], true, "{scores}");
        assert_eq!(scores["runs"], 1);
        assert_eq!(scores["failed_cases"], json!([]), "{scores}");
        assert_eq!(
            scores["program_stopped"],
            json!({"count": 1, "cases": [{"id": "clean-one", "program": "fmt"}]})
        );
        // A judgment recorded on a stopped case counts for nothing; the
        // same judgment on a case no program stopped counts against it.
        let judged = |stopped: bool| {
            let mut judged = round(&[
                (
                    "needs-d1",
                    verdict("revise", &["D-1"], "adds notes.txt"),
                    json!({"usd": 0.25, "source": "actual"}),
                ),
                (
                    "clean-one",
                    verdict("concern", &["D-1"], "notes look odd"),
                    json!({"usd": 0.25, "source": "actual"}),
                ),
            ]);
            if stopped {
                judged.checks.insert(
                    "clean-one".to_owned(),
                    programs::CaseCheck::Stopped {
                        program: "fmt".to_owned(),
                    },
                );
            }
            finished(&judged, &cases(), None)
        };
        let scores = judged(true);
        assert_eq!(
            (scores["passed"].clone(), scores["runs"].clone()),
            (json!(true), json!(1)),
            "{scores}"
        );
        let scores = judged(false);
        assert_eq!(scores["program_stopped"]["count"], 0);
        assert_eq!(scores["passed"], false, "{scores}");
        assert_eq!(scores["failed_cases"][0]["id"], "clean-one", "{scores}");
    }

    /// A program that could not run or ran past its time closes the round
    /// as `program_failed`, which never passes even with every value over
    /// the threshold; it goes before the dollars' limit.
    #[test]
    fn a_program_that_failed_closes_the_round_incomplete() {
        assert_eq!(Incomplete::of(true, true), Some(Incomplete::ProgramFailed));
        assert_eq!(Incomplete::of(true, false), Some(Incomplete::ProgramFailed));
        assert_eq!(Incomplete::of(false, true), Some(Incomplete::CostLimit));
        assert_eq!(Incomplete::of(false, false), None);
        let perfect = round(&[
            (
                "needs-d1",
                verdict("revise", &["D-1"], "adds notes.txt"),
                json!({"usd": 0.25, "source": "actual"}),
            ),
            (
                "clean-one",
                verdict("pass", &[], "fine"),
                json!({"usd": 0.25, "source": "actual"}),
            ),
        ]);
        let scores = finished(&perfect, &cases(), Some(Incomplete::ProgramFailed));
        assert_eq!(scores["outcome"], "incomplete");
        assert_eq!(scores["incomplete_reason"], "program_failed");
        assert_eq!(scores["passed"], false);
        assert!(scores["below"].as_array().unwrap().is_empty(), "{scores}");
    }
}
