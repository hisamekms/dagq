//! The dev a run that changes an agent's definition runs after its review
//! passed and before it lands (ADR-t1728-1 decision 10): which agents and
//! where the run goes are [`crate::domain::agent_eval::before_landing`]'s.
//!
//! On a pass, before the conflict precheck, the run's diff (the review's
//! `<base>...<head>`) is read: for each agent whose
//! `.dagq/agents/<name>/AGENT.md` it changes and whose `evals/dev.json`
//! the landing branch's commit has, a dev round is asked for
//! (`agent_eval_requested` with `before_landing` and the run's head as the
//! definition's commit), which the eval's own pass
//! ([`super::agent_eval`]) runs outside the run slots, before every other
//! request and never stopping one that runs. The round measures the run's
//! definition on the landing branch's cases only: the run's own case
//! changes are not used. `run_agent_eval_requested` records the rounds at
//! the head and the agents left out with why; a run that changes no
//! definition records nothing and goes on as before.
//!
//! Meanwhile the run waits in its slot with its lease and its session open
//! ([`Phase::AgentEval`]), `run_agent_eval_waiting` saying why when that
//! changes. Once its rounds ended, `run_agent_eval_finished` records where
//! it goes: on to the precheck and the landing when every round passed;
//! back to the session as a review's revise, counted with the run's
//! revises, when one fell under the threshold (with the failed cases' ids
//! and the agent's reasons), or to a person when the revises are used up;
//! to a person (`approve_landing`, told why) when a round could not tell.
//! A supervisor that stops (not one that hands off, whose next process
//! adopts the run and asks for nothing again at the same head) asks a
//! person too: it starts no round.

use std::collections::BTreeSet;

use super::landing::landing_ask;
use super::revise::SentBack;
use super::*;
use crate::domain::ActorContext;
use crate::domain::EventKind;
use crate::domain::ReviseLimit;
use crate::domain::agent_eval::before_landing::{self, Verdict, WaitReason};
use crate::domain::agent_eval::record::{Request, Round};
use crate::domain::agent_eval::{EVALS_DIR, Split};
use crate::domain::review_subagents::{DEFINITION_DIR, definition_path};

/// A passed run waiting for the dev of the definitions it changes: what
/// it goes on with once its rounds ended.
#[derive(Clone)]
pub(super) struct EvalGateWatch {
    session: Option<SessionRef>,
    verdict: ReviewVerdict,
    job: Option<ActorContext>,
    /// The run's head the rounds measure.
    commit: String,
    /// Each round's agent and id.
    rounds: Vec<(String, i64)>,
    /// Why it waits, as last recorded.
    wait: Option<WaitReason>,
}

/// The rounds `run_agent_eval_requested` asked for at `commit`, the latest
/// such, `None` when none was recorded at that commit.
fn asked_at(events: &[RunEvent], commit: &str) -> Option<Vec<(String, i64)>> {
    events
        .iter()
        .rev()
        .find(|event| {
            event.kind == EventKind::RunAgentEvalRequested && event.payload["commit"] == commit
        })
        .map(|event| {
            event.payload["rounds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|round| {
                    Some((
                        round["agent"].as_str()?.to_owned(),
                        round["eval_id"].as_i64()?,
                    ))
                })
                .collect()
        })
}

/// Whether the rounds at `commit` ended with every one passed: the latest
/// `run_agent_eval_finished` at that commit says `passed`.
fn passed_at(events: &[RunEvent], commit: &str) -> bool {
    events
        .iter()
        .rev()
        .find(|event| {
            event.kind == EventKind::RunAgentEvalFinished && event.payload["commit"] == commit
        })
        .is_some_and(|event| event.payload["outcome"] == "passed")
}

/// Why a run whose rounds a stopping supervisor cannot run asks a person.
const STOPPED: &str = "the supervisor stopped before the agent eval of the definitions the run changed ended, so whether they reach the threshold is not known";

/// The question's reason of a send-back the run has no revise left for,
/// its `limit` used up.
fn used_up_why(limit: ReviseLimit, reasons: &[String]) -> String {
    format!(
        "the agent eval of the definitions the run changed fell under the threshold and the run's {limit} revises are used up: {}",
        reasons.join("; ")
    )
}

impl Supervisor<'_> {
    /// The dev before `run` lands, on a pass (with its session, verdict
    /// and job) about to be prechecked: `None` goes on when the run changes
    /// no definition with dev cases or its rounds at its head passed, else
    /// the phase that waits for them (asked for here once per head) or
    /// goes where they sent it (see the module).
    pub(super) fn eval_before_landing(
        &mut self,
        run: &TaskRun,
        (session, verdict, job): (&Option<SessionRef>, &ReviewVerdict, &Option<ActorContext>),
    ) -> Result<Option<Phase>> {
        let Some(head) = run.result_commit().map(|commit| commit.as_str().to_owned()) else {
            return Ok(None);
        };
        let events = self.queue.run_events(run.id())?;
        if passed_at(&events, &head) {
            return Ok(None);
        }
        let rounds = match asked_at(&events, &head) {
            Some(rounds) => rounds,
            None => match self.ask_for_eval_rounds(run, &head) {
                Ok(Some(rounds)) => rounds,
                result => {
                    let failed = match result {
                        Err(error) => unreadable_eval(&error),
                        _ => Verdict::Ask(STOPPED.to_owned()),
                    };
                    if let Verdict::Ask(why) = &failed {
                        warn!(run_id = %run.id(), "run {}: {why}", run.id());
                    }
                    let mut watch = EvalGateWatch {
                        session: session.clone(),
                        verdict: verdict.clone(),
                        job: job.clone(),
                        commit: head,
                        rounds: Vec::new(),
                        wait: None,
                    };
                    return self.eval_ends(run, &mut watch, failed).map(Some);
                }
            },
        };
        if rounds.is_empty() {
            return Ok(None);
        }
        let mut watch = EvalGateWatch {
            session: session.clone(),
            verdict: verdict.clone(),
            job: job.clone(),
            commit: head,
            rounds,
            wait: None,
        };
        let phase = match self.look_at_eval(run, &mut watch)? {
            Some(phase) => phase,
            None => Phase::AgentEval(watch),
        };
        Ok(Some(phase))
    }

    /// Whether this supervisor stops and does not hand off: it starts no
    /// round, and the next process does not take its runs over.
    fn stopping_for_good(&self) -> bool {
        self.registration.draining() && !self.registration.handing_off()
    }

    /// Ask for a dev round of each agent whose definition `run` changes at
    /// `head` and which has dev cases on the landing branch, and record
    /// `run_agent_eval_requested` when any definition changed: the rounds,
    /// each agent with its eval's id. `None` when there are rounds to ask
    /// for but this supervisor stops: it asks for none.
    fn ask_for_eval_rounds(
        &mut self,
        run: &TaskRun,
        head: &str,
    ) -> Result<Option<Vec<(String, i64)>>> {
        let main = self.review_reads.main_head()?;
        let range = self.receipt_range(run, &main)?;
        if self.team_review {
            anyhow::ensure!(
                range.head.eq_ignore_ascii_case(head),
                "eval head differs from review head"
            );
        }
        let definition_head = if self.team_review {
            range.head.as_str()
        } else {
            head
        };
        let targets = eval_targets(&*self.review_reads, &main, &range, definition_head)?;
        if targets.agents.is_empty() && targets.skipped.is_empty() {
            return Ok(Some(Vec::new()));
        }
        if !targets.agents.is_empty() && self.stopping_for_good() {
            return Ok(None);
        }
        let mut rounds = Vec::new();
        for agent in &targets.agents {
            let request = eval_request(
                agent,
                definition_head,
                run.id().as_str(),
                self.registration.token().as_str(),
            );
            let id = self
                .queue
                .record_queue_event(EventKind::AgentEvalRequested, request.record())?;
            rounds.push((agent.clone(), id.as_i64()));
        }
        self.queue.record_runtime_event(
            run.id(),
            EventKind::RunAgentEvalRequested,
            json!({
                "commit": head,
                "landing_commit": main,
                "rounds": rounds.iter().map(|(agent, id)| json!({"agent": agent, "eval_id": id})).collect::<Vec<_>>(),
                "skipped": targets.skipped.iter().map(|(agent, why)| json!({"agent": agent, "reason": why})).collect::<Vec<_>>(),
            }),
        )?;
        for (agent, why) in &targets.skipped {
            info!(run_id = %run.id(), "run {}: it changes {agent}'s definition, which runs no agent eval before it lands ({})", run.id(), why.as_str());
        }
        if !rounds.is_empty() {
            info!(run_id = %run.id(), "run {}: it changes the definitions of {}; their dev runs on the landing branch's cases before it lands", run.id(), targets.agents.join(", "));
        }
        Ok(Some(rounds))
    }

    /// Look at the rounds `watch` waits for: `None` while it waits (why
    /// recorded when it changed), else the phase their end sends the run
    /// to.
    pub(super) fn look_at_eval(
        &mut self,
        run: &TaskRun,
        watch: &mut EvalGateWatch,
    ) -> Result<Option<Phase>> {
        let rounds = self.eval_rounds()?;
        let ours: Vec<(&str, &Round)> = watch
            .rounds
            .iter()
            .filter_map(|(agent, id)| {
                rounds
                    .iter()
                    .find(|round| round.id == *id)
                    .map(|round| (agent.as_str(), round))
            })
            .collect();
        let verdict = if ours.len() == watch.rounds.len() {
            before_landing::judge(ours)
        } else {
            Verdict::Ask(
                "a round of the agent eval the run asked for is not in the queue's events"
                    .to_owned(),
            )
        };
        if let Verdict::Wait(reason) = verdict {
            // A supervisor that stops starts no round: a person decides.
            if self.stopping_for_good() {
                return self
                    .eval_ends(run, watch, Verdict::Ask(STOPPED.to_owned()))
                    .map(Some);
            }
            if watch.wait != Some(reason) {
                self.queue.record_runtime_event(
                    run.id(),
                    EventKind::RunAgentEvalWaiting,
                    json!({
                        "commit": watch.commit,
                        "reason": reason,
                        "eval_ids": watch.rounds.iter().map(|(_, id)| id).collect::<Vec<_>>(),
                    }),
                )?;
                info!(run_id = %run.id(), "run {} waits in its slot for the agent eval before it lands ({})", run.id(), reason.as_str());
                watch.wait = Some(reason);
            }
            return Ok(None);
        }
        self.eval_ends(run, watch, verdict).map(Some)
    }

    /// Record where the rounds sent the run and go there: the precheck, a
    /// revise, or a person's `approve_landing`.
    fn eval_ends(
        &mut self,
        run: &TaskRun,
        watch: &mut EvalGateWatch,
        verdict: Verdict,
    ) -> Result<Phase> {
        let job = watch
            .job
            .clone()
            .unwrap_or_else(|| self.layout.supervisor_actor());
        // A send-back with no revise left asks a person instead: the
        // eval's reasons carry no code (ADR-t2599-1), so they never land.
        let verdict = match verdict {
            Verdict::SendBack(reasons) => match self.revises_used_up(run.id())? {
                Some(limit) => Verdict::Ask(used_up_why(limit, &reasons)),
                None => Verdict::SendBack(reasons),
            },
            verdict => verdict,
        };
        let mut payload = json!({
            "commit": watch.commit,
            "rounds": watch.rounds.iter().map(|(agent, id)| json!({"agent": agent, "eval_id": id})).collect::<Vec<_>>(),
        });
        match &verdict {
            Verdict::Pass => payload["outcome"] = json!("passed"),
            Verdict::SendBack(reasons) => {
                payload["outcome"] = json!("send_back");
                payload["reasons"] = json!(reasons);
            }
            Verdict::Ask(why) => {
                payload["outcome"] = json!("ask");
                payload["why"] = json!(why);
            }
            Verdict::Wait(_) => unreachable!("a wait does not end"),
        }
        self.queue
            .record_runtime_event(run.id(), EventKind::RunAgentEvalFinished, payload)?;
        let (session, verdict_of_review) = (watch.session.clone(), watch.verdict.clone());
        match verdict {
            Verdict::Pass => {
                info!(run_id = %run.id(), "run {}: the agent eval before it lands passed", run.id());
                self.precheck(run, session, verdict_of_review, watch.job.clone())
            }
            Verdict::SendBack(reasons) => {
                warn!(run_id = %run.id(), "run {}: the agent eval before it lands fell under the threshold; it goes back to its session", run.id());
                let revise = ReviewVerdict {
                    verdict: ReviewDecision::Revise,
                    reasons,
                    reason_codes: Vec::new(),
                    reason_paths: Vec::new(),
                    summary: "the agent eval before the landing fell under the threshold"
                        .to_owned(),
                    recommendation: None,
                    confidence: None,
                    reason_category: None,
                    agents: Vec::new(),
                    follow_ups: Vec::new(),
                };
                // The run has a revise left (above), so its findings are
                // not judged past the limit.
                let sent_back = SentBack {
                    attempt: 0,
                    findings: Vec::new(),
                };
                self.send_revise(run, session, revise, &job, None, sent_back)
            }
            Verdict::Ask(why) => {
                warn!(run_id = %run.id(), "run {}: the agent eval before it lands asks a person: {why}", run.id());
                Ok(landing_ask(
                    Some(why),
                    verdict_of_review,
                    session,
                    &job,
                    false,
                ))
            }
            Verdict::Wait(_) => unreachable!("a wait does not end"),
        }
    }
}

/// Decide targets using only committed reads: definitions at the reviewed head,
/// cases at main. An unreadable or partial diff must reach the caller's Ask,
/// never the empty-round shortcut or landing.
fn eval_targets(
    reads: &(impl crate::application::review_input::ReviewReads + ?Sized),
    main: &crate::domain::CommitSha,
    range: &crate::application::review::ReviewRange,
    definition_head: &str,
) -> Result<before_landing::Targets> {
    let changed = reads.changed_paths(&range.base, &range.head)?;
    let agents = before_landing::changed_agents(changed.iter().map(String::as_str));
    let mut at_head = BTreeSet::new();
    let mut with_dev_cases = BTreeSet::new();
    for agent in &agents {
        if reads
            .file_in(definition_head, &definition_path(agent))?
            .is_some()
        {
            at_head.insert(agent.clone());
        }
        let dev = format!(
            "{DEFINITION_DIR}/{agent}/{EVALS_DIR}/{}",
            Split::Dev.file_name()
        );
        if reads.file_in(main.as_str(), &dev)?.is_some() {
            with_dev_cases.insert(agent.clone());
        }
    }
    Ok(before_landing::targets(
        agents,
        |a| at_head.contains(a),
        |a| with_dev_cases.contains(a),
    ))
}

/// Fixed routing for an unreadable selection, shared with the gate's error arm.
fn unreadable_eval(error: &anyhow::Error) -> Verdict {
    Verdict::Ask(format!(
        "whether the run changes an agent's definition could not be read for the agent eval before it lands: {error:#}"
    ))
}

/// The requested definition is the reviewed (accepted in team mode) head.
/// Cases are deliberately not pinned here: the eval round reads main's cases.
fn eval_request(agent: &str, head: &str, run: &str, supervisor: &str) -> Request {
    Request {
        agent: agent.to_owned(),
        split: Split::Dev,
        k: None,
        cases: None,
        set: None,
        rerun: false,
        requested_by: "supervisor".to_owned(),
        requested_by_id: supervisor.to_owned(),
        run_id: Some(run.to_owned()),
        before_landing: true,
        definition_commit: Some(head.to_owned()),
        launch: crate::domain::actor_model::AgentModel::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, MAX_REVISE_ATTEMPTS, TaskId};

    fn event(id: i64, kind: EventKind, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.as_str().into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    use crate::application::review_input::ReviewReads as _;
    struct Reads {
        fail: bool,
    }
    impl crate::application::review_input::ReviewReads for Reads {
        fn main_head(&self) -> Result<CommitSha> {
            Ok(CommitSha::try_from("a".repeat(40)).unwrap())
        }
        fn merge_base(&self, _: &str, _: &str) -> Result<Option<CommitSha>> {
            unreachable!()
        }
        fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
            assert_eq!((from, to), ("base", "accepted"));
            anyhow::ensure!(!self.fail, "paths unavailable or truncated");
            Ok(["changed", "removed", "no-cases"]
                .iter()
                .map(|a| definition_path(a))
                .collect())
        }
        fn file_in(&self, commit: &str, path: &str) -> Result<Option<String>> {
            assert!(commit == "accepted" || commit == "a".repeat(40));
            Ok(((commit == "accepted"
                && (path == definition_path("changed") || path == definition_path("no-cases")))
                || (commit == "a".repeat(40) && path == ".dagq/agents/changed/evals/dev.json"))
                .then(|| "committed content".into()))
        }
    }
    #[test]
    fn remote_eval_reads_definitions_at_accepted_head_and_cases_at_main() {
        let reads = Reads { fail: false };
        let range = crate::application::review::ReviewRange {
            base: "base".into(),
            head: "accepted".into(),
        };
        let targets =
            eval_targets(&reads, &reads.main_head().unwrap(), &range, "accepted").unwrap();
        assert_eq!(targets.agents, ["changed"]);
        let request = eval_request(&targets.agents[0], &range.head, "run", "supervisor");
        assert_eq!(request.definition_commit.as_deref(), Some("accepted"));
        assert_eq!(request.agent, "changed");
        assert_eq!(request.split, Split::Dev);
        assert!(request.before_landing);

        assert_eq!(
            targets.skipped,
            [
                ("no-cases".into(), before_landing::SkipReason::NoDevCases),
                (
                    "removed".into(),
                    before_landing::SkipReason::DefinitionRemoved
                ),
            ]
        );
    }
    #[test]
    fn remote_eval_refuses_unavailable_paths_before_the_empty_round_shortcut() {
        let reads = Reads { fail: true };
        let range = crate::application::review::ReviewRange {
            base: "base".into(),
            head: "accepted".into(),
        };
        let error =
            eval_targets(&reads, &reads.main_head().unwrap(), &range, "accepted").unwrap_err();
        let Verdict::Ask(why) = unreadable_eval(&error) else {
            panic!("unreadable selection must ask, never land")
        };
        assert!(why.contains("paths unavailable or truncated"));
    }

    /// The rounds asked at a head are those of its latest request; a new
    /// head (the run sent back and committed again) asks for its own.
    #[test]
    fn the_rounds_and_their_pass_are_read_at_the_runs_head() {
        let events = [
            event(
                1,
                EventKind::RunAgentEvalRequested,
                json!({"commit": "h1", "rounds": [{"agent": "a", "eval_id": 5}]}),
            ),
            event(
                2,
                EventKind::RunAgentEvalFinished,
                json!({"commit": "h1", "outcome": "send_back"}),
            ),
            event(
                3,
                EventKind::RunAgentEvalRequested,
                json!({"commit": "h2", "rounds": [{"agent": "a", "eval_id": 9}]}),
            ),
            event(
                4,
                EventKind::RunAgentEvalFinished,
                json!({"commit": "h2", "outcome": "passed"}),
            ),
        ];
        assert_eq!(asked_at(&events, "h1"), Some(vec![("a".to_owned(), 5)]));
        assert_eq!(asked_at(&events, "h2"), Some(vec![("a".to_owned(), 9)]));
        assert_eq!(asked_at(&events, "h3"), None);
        assert!(!passed_at(&events, "h1"));
        assert!(passed_at(&events, "h2"));
        assert!(!passed_at(&events[..3], "h2"));
    }

    #[test]
    fn a_send_back_past_the_revises_says_so_with_its_reasons() {
        let why = used_up_why(ReviseLimit::default(), &["case x failed".to_owned()]);
        assert!(why.contains(&format!("{MAX_REVISE_ATTEMPTS} revises are used up")));
        let why = used_up_why(ReviseLimit::new(5).unwrap(), &["case x failed".to_owned()]);
        assert!(why.contains("the run's 5 revises are used up"), "{why}");
        assert!(why.ends_with("case x failed"));
    }
}
