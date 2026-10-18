//! The agent jobs of a run's review (ADR-t1895-1 decisions 2 to 5 and 7).
//! Once its program reviews pass, a review runs its whole review as one
//! job and each agent its paths selected as a job of its own, side by side
//! within the supervisor's limit of agent jobs at once, which every run's
//! review shares. It judges only once every job ended: a job that gave no
//! answer runs once more on its own, one that gives none again fails the
//! review (never a pass), and once each answered, the heaviest of their
//! judgments decides where the run goes, by the machine
//! ([`super::review_subagents::route`]). No AI job joins the judgments.
//!
//! These are the decisions; `application::supervise::landing` starts,
//! waits for and records the jobs.

use serde_json::Value;

use super::ReviewVerdict;
use super::review_subagents::{AgentResult, COMPLETED};

/// Which job of a review: its whole review (the acceptance and the
/// documents), or one selected agent's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewJobId {
    Whole,
    Agent(String),
}

impl ReviewJobId {
    /// The agent the job is of; `None` for the whole review. Its
    /// `headless_jobs` row's `label` and its events' `agent` are this. An
    /// attempt of a review has the whole review's job and one per agent,
    /// each recording its own Execution, so an end is matched to a job by
    /// this as well as by its run and attempt.
    pub fn agent(&self) -> Option<&str> {
        match self {
            Self::Whole => None,
            Self::Agent(agent) => Some(agent),
        }
    }

    /// How a log line or an error names it.
    pub fn describe(&self) -> String {
        match self {
            Self::Whole => "the whole review".to_owned(),
            Self::Agent(agent) => format!("the agent {agent}"),
        }
    }

    /// The stem of its output files in the run directory (`.out` and
    /// `.err` follow) for review `attempt`: `review-<attempt>` for the
    /// whole review, as before its agents ran as jobs, and
    /// `review-<attempt>-agent-<agent>` for an agent's; its run once more
    /// (`again`) adds `.again`, which no agent's name holds, so it keeps
    /// the first run's output.
    pub fn output(&self, attempt: usize, again: bool) -> String {
        let stem = match self {
            Self::Whole => format!("review-{attempt}"),
            Self::Agent(agent) => format!("review-{attempt}-agent-{agent}"),
        };
        if again { format!("{stem}.again") } else { stem }
    }
}

/// How a run of a review job ended without an answer to judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobMiss {
    /// It ended but its reply held no readable verdict: no JSON, a broken
    /// one, an unknown field, or (an agent's) not its one completed result.
    Unreadable,
    /// Its process exited non-zero.
    Exited,
    /// It did not end within its timeout and was stopped.
    TimedOut,
    /// Its process could not start, for a reason that is not its
    /// provider's (that one moves or waits instead).
    StartFailed,
}

impl JobMiss {
    /// What `review_job_finished` records as its `outcome`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unreadable => "unreadable",
            Self::Exited => "job_failed",
            Self::TimedOut => "timed_out",
            Self::StartFailed => "start_failed",
        }
    }
}

/// Whether a job that missed runs once more with the same input
/// (ADR-t1895-1 decision 5): only while the retry is on (`retry`, off only
/// in tests), when neither the job ran once more already (`job_again`) nor
/// the review is itself the retry of its program reviews (`review_retried`,
/// one retry per review across its stages), and while a provider can run
/// it (`manual`: `--no-claude` left none, so a person reviews it). Every
/// kind of miss counts alike: a job that could not start, timed out, exited
/// non-zero or gave no readable answer. A `review_job_finished` that says
/// it runs again (or that it was `held` for its provider) is not the end of
/// its `headless_jobs` row: the job starts again in a row of its own, so a
/// supervisor that takes over does not count that end as the row's
/// Execution ([`crate::domain::headless_job::JobOn`]).
pub const fn runs_again(retry: bool, job_again: bool, review_retried: bool, manual: bool) -> bool {
    retry && !job_again && !review_retried && !manual
}

/// How many of `waiting` jobs start now when `running` agent jobs of the
/// supervisor's reviews run and at most `limit` may run at once
/// (`[review.jobs] concurrency`): the rest wait for a job to end.
pub fn startable(limit: usize, running: usize, waiting: usize) -> usize {
    waiting.min(limit.saturating_sub(running))
}

/// Where one job of a review stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStanding<'a> {
    /// It waits to start or runs.
    Open,
    /// It gave an answer to judge.
    Answered,
    /// It gave none and will not run again; why.
    Missed(&'a str),
}

/// Where a review stands from its jobs'.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewStanding {
    /// A job waits or runs: nothing is judged yet.
    Open,
    /// Every job answered: the review is judged.
    Answered,
    /// Every job ended and these gave no answer (why each): the review
    /// fails to a person and does not pass.
    Missed(Vec<String>),
}

/// Where a review whose jobs stand as `jobs` stands: open while any job is
/// (whatever the others gave), else failed when any missed, else answered.
pub fn standing<'a>(jobs: impl IntoIterator<Item = JobStanding<'a>>) -> ReviewStanding {
    let mut missed = Vec::new();
    for job in jobs {
        match job {
            JobStanding::Open => return ReviewStanding::Open,
            JobStanding::Answered => {}
            JobStanding::Missed(why) => missed.push(why.to_owned()),
        }
    }
    if missed.is_empty() {
        ReviewStanding::Answered
    } else {
        ReviewStanding::Missed(missed)
    }
}

/// The result of `agent`'s job from its reply, read as one JSON object
/// (`reply`, none when it held none): the one completed result of that
/// agent with a judgment, else why it is unreadable.
pub fn agent_result(agent: &str, reply: Option<Value>) -> Result<AgentResult, String> {
    let reply = reply.ok_or_else(|| format!("the agent {agent} printed no verdict JSON"))?;
    let result: AgentResult = serde_json::from_value(reply)
        .map_err(|error| format!("the agent {agent} printed no readable verdict: {error}"))?;
    if result.agent != agent {
        Err(format!(
            "the agent {agent} printed no result of {agent} but one of {}",
            result.agent
        ))
    } else if result.status != COMPLETED {
        Err(format!(
            "the agent {agent} did not complete ({})",
            result.status
        ))
    } else if result.verdict.is_none() {
        Err(format!("the agent {agent} gave no verdict"))
    } else {
        Ok(result)
    }
}

/// The verdict a review is judged by once every job answered: the whole
/// review's own (`whole`) beside each agent's result (`agents`, in their
/// configured order), so that the heaviest of their judgments decides
/// where the run goes ([`ReviewVerdict::route`]) and the reasons of every
/// judgment that sends it back go in one revise.
pub fn assemble(whole: ReviewVerdict, agents: Vec<AgentResult>) -> ReviewVerdict {
    ReviewVerdict { agents, ..whole }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ReviewDecision;
    use crate::domain::review_subagents::Destination;
    use serde_json::json;

    fn whole(decision: &str, reasons: &[&str]) -> ReviewVerdict {
        ReviewVerdict::parse(
            &json!({"verdict": decision, "reasons": reasons, "summary": "whole"}).to_string(),
        )
        .unwrap()
    }

    fn agent(name: &str, decision: &str, reasons: &[&str]) -> AgentResult {
        agent_result(
            name,
            Some(json!({"agent": name, "status": "completed", "verdict": decision,
                        "reasons": reasons.iter().map(|r| json!({"text": r, "codes": []})).collect::<Vec<_>>(),
                        "summary": format!("{name} looked")})),
        )
        .unwrap()
    }

    /// The reasons a route sends back with, as the judgment past the
    /// revise limit reads them (ADR-t2599-1): the whole review's when its
    /// own judgment goes where the route does, then each deciding agent's
    /// under its name with the codes it gave, a security agent's marked,
    /// and an agent's summary unlabeled when it gave no reason.
    #[test]
    fn a_routes_findings_carry_each_agents_codes_and_mark_the_security_ones() {
        let coded = |name: &str, decision: &str, reasons: Value| {
            agent_result(
                name,
                Some(
                    json!({"agent": name, "status": "completed", "verdict": decision,
                            "reasons": reasons, "summary": format!("{name} looked")}),
                ),
            )
            .unwrap()
        };
        let verdict = assemble(
            whole("revise", &["whole finding"]),
            vec![
                coded(
                    "design",
                    "revise",
                    json!([{"text": "stale", "codes": ["docs_drift"]}]),
                ),
                coded(
                    "security",
                    "revise",
                    json!([{"text": "leak", "codes": ["local_slip"]}]),
                ),
                coded("builtin:security", "revise", json!([])),
                coded("tests", "pass", json!([])),
            ],
        );
        // Recorded and read back, the codes stay.
        let recorded: crate::domain::review_subagents::AgentResult =
            serde_json::from_value(json!(verdict.agents[0])).unwrap();
        assert_eq!(recorded.reason_codes, [vec!["docs_drift".to_owned()]]);
        let route = verdict.route(true);
        let findings = verdict.sent_back_findings(Some(&route));
        let got: Vec<(&str, Vec<&str>, bool)> = findings
            .iter()
            .map(|f| {
                (
                    f.text.as_str(),
                    f.codes.iter().map(String::as_str).collect(),
                    f.security,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("whole finding", vec!["unlabeled"], false),
                ("design: stale", vec!["docs_drift"], false),
                ("security: leak", vec!["local_slip"], true),
                (
                    "builtin:security: builtin:security looked",
                    vec!["unlabeled"],
                    true
                ),
            ]
        );
        assert!(!crate::domain::review_reason::converged(
            &findings[1..3],
            None
        ));
        assert!(crate::domain::review_reason::converged(
            &findings[1..2],
            None
        ));
        // Without a route, only the verdict's own reasons.
        assert_eq!(verdict.sent_back_findings(None).len(), 1);
        // A whole review that passes while an agent sends back gives only
        // the agent's.
        let lighter = assemble(
            whole("pass", &[]),
            vec![coded(
                "design",
                "revise",
                json!([{"text": "stale", "codes": ["docs_drift"]}]),
            )],
        );
        let route = lighter.route(true);
        assert_eq!(
            lighter
                .sent_back_findings(Some(&route))
                .iter()
                .map(|f| f.text.as_str())
                .collect::<Vec<_>>(),
            ["design: stale"]
        );
    }

    /// A whole review that sends the run back with no reason keeps a
    /// finding of its own, unlabeled, beside an agent's: past the revise
    /// limit an agent's `local_slip` alone does not converge the review
    /// when the whole review also sends the run back (ADR-t2599-1 decision
    /// 3, a send-back with no reason). With no route the whole review's
    /// empty send-back is unlabeled too.
    #[test]
    fn a_whole_review_sending_back_with_no_reason_keeps_an_unlabeled_finding() {
        let verdict = assemble(
            whole("revise", &[]),
            vec![
                agent_result(
                    "design",
                    Some(
                        json!({"agent": "design", "status": "completed", "verdict": "revise",
                            "reasons": [{"text": "a typo", "codes": ["local_slip"]}],
                            "summary": "design looked"}),
                    ),
                )
                .unwrap(),
            ],
        );
        let route = verdict.route(true);
        assert_eq!(route.parent, route.destination);
        let findings = verdict.sent_back_findings(Some(&route));
        let got: Vec<(&str, Vec<&str>)> = findings
            .iter()
            .map(|f| {
                (
                    f.text.as_str(),
                    f.codes.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("whole", vec!["unlabeled"]),
                ("design: a typo", vec!["local_slip"])
            ]
        );
        assert!(!crate::domain::review_reason::converged(&findings, None));
        let alone = whole("revise", &[]).sent_back_findings(None);
        assert_eq!(alone.len(), 1);
        assert_eq!(alone[0].codes, ["unlabeled"]);
        assert!(!crate::domain::review_reason::converged(&alone, None));
    }

    /// The whole review and an agent job may give follow_ups with any
    /// verdict (ADR-t2352-1); a verdict without them reads as before, and
    /// a follow_ups of a broken shape leaves out what does not read
    /// instead of making the verdict unreadable. They never change where
    /// the run goes: a pass with follow_ups lands.
    #[test]
    fn a_verdicts_follow_ups_are_read_and_never_send_the_run_back() {
        let stale = json!([{"text": "stale", "path": "docs/a.md", "section": "s", "codes": ["docs_drift"]}]);
        for decision in ["pass", "revise", "concern"] {
            let verdict = ReviewVerdict::parse(
                &json!({"verdict": decision, "reasons": [], "summary": "w", "follow_ups": stale})
                    .to_string(),
            )
            .unwrap();
            assert_eq!(verdict.follow_ups.len(), 1, "{decision}");
            assert_eq!(verdict.follow_ups[0].path, "docs/a.md");
        }
        assert_eq!(whole("pass", &[]).follow_ups, Vec::new());
        let broken = ReviewVerdict::parse(
            &json!({"verdict": "pass", "reasons": [], "summary": "w",
                    "follow_ups": [{"text": "no path"}, 3, {"text": "t", "path": "docs/b.md"}]})
            .to_string(),
        )
        .unwrap();
        assert_eq!(broken.follow_ups.len(), 1);
        assert_eq!(broken.follow_ups[0].path, "docs/b.md");
        let not_a_list = ReviewVerdict::parse(
            &json!({"verdict": "pass", "reasons": [], "summary": "w", "follow_ups": "docs/a.md"})
                .to_string(),
        )
        .unwrap();
        assert_eq!(not_a_list.follow_ups, Vec::new());

        let with_agent = assemble(
            ReviewVerdict::parse(
                &json!({"verdict": "pass", "reasons": [], "summary": "w", "follow_ups": stale})
                    .to_string(),
            )
            .unwrap(),
            vec![
                agent_result(
                    "design",
                    Some(
                        json!({"agent": "design", "status": "completed", "verdict": "pass",
                                "reasons": [], "summary": "d",
                                "follow_ups": [{"text": "old", "path": "docs/c.md", "codes": []}]}),
                    ),
                )
                .unwrap(),
                agent("adr", "pass", &[]),
            ],
        );
        assert_eq!(with_agent.agents[1].follow_ups, Vec::new());
        let found = with_agent.found_follow_ups();
        assert_eq!(
            found
                .iter()
                .map(|f| (f.agent.as_deref(), f.follow_up.path.as_str()))
                .collect::<Vec<_>>(),
            [(None, "docs/a.md"), (Some("design"), "docs/c.md")]
        );
        for revise_left in [true, false] {
            let route = with_agent.route(revise_left);
            assert_eq!(route.destination, Destination::Land);
            assert_eq!(route.parent, Destination::Land);
        }
        // What review_finished records reads back with them.
        let recorded: ReviewVerdict =
            serde_json::from_value(serde_json::to_value(&with_agent).unwrap()).unwrap();
        assert_eq!(recorded, with_agent);
    }

    /// An agent job's follow_ups are read as the whole review's
    /// (ADR-t2352-1): a follow_ups that is not an array reads as none, and
    /// broken entries mixed with good ones leave only the good ones; the
    /// result stays readable and its judgment is unchanged.
    #[test]
    fn an_agents_broken_follow_ups_leave_its_result_readable() {
        let result = |follow_ups: Value| {
            agent_result(
                "design",
                Some(
                    json!({"agent": "design", "status": "completed", "verdict": "pass",
                            "reasons": [], "summary": "d", "follow_ups": follow_ups}),
                ),
            )
            .unwrap()
        };
        for not_a_list in [
            json!("docs/a.md"),
            json!({"text": "t", "path": "p"}),
            json!(3),
            Value::Null,
        ] {
            let read = result(not_a_list);
            assert_eq!(read.follow_ups, Vec::new());
            assert_eq!(read.verdict, Some(ReviewDecision::Pass));
        }
        let mixed = result(json!([
            {"text": "no path"},
            "a bare string",
            {"text": " ", "path": "docs/b.md"},
            {"text": "stale", "path": "docs/c.md", "section": 7, "codes": ["docs_drift", 1]},
        ]));
        assert_eq!(mixed.follow_ups.len(), 1);
        assert_eq!(mixed.follow_ups[0].path, "docs/c.md");
        assert_eq!(mixed.follow_ups[0].section, "");
        assert_eq!(mixed.follow_ups[0].codes, vec!["docs_drift".to_owned()]);
        assert_eq!(mixed.verdict, Some(ReviewDecision::Pass));
    }

    #[test]
    fn a_job_is_named_by_its_agent_and_writes_its_own_output() {
        let design = ReviewJobId::Agent("design".into());
        assert_eq!(ReviewJobId::Whole.agent(), None);
        assert_eq!(design.agent(), Some("design"));
        assert_eq!(ReviewJobId::Whole.output(2, false), "review-2");
        assert_eq!(ReviewJobId::Whole.output(2, true), "review-2.again");
        assert_eq!(design.output(2, false), "review-2-agent-design");
        assert_eq!(design.output(2, true), "review-2-agent-design.again");
        assert_eq!(design.describe(), "the agent design");
    }

    /// Any miss runs once more, once per job, with the retry on, when the
    /// review is not its programs' retry and a provider is left.
    #[test]
    fn a_job_that_missed_runs_once_more_only_once() {
        assert!(runs_again(true, false, false, false));
        assert!(!runs_again(true, true, false, false), "its one run again");
        assert!(!runs_again(true, false, true, false), "the review's retry");
        assert!(!runs_again(true, false, false, true), "no provider left");
        assert!(!runs_again(false, false, false, false), "retry off");
        let kinds = [
            JobMiss::Unreadable,
            JobMiss::Exited,
            JobMiss::TimedOut,
            JobMiss::StartFailed,
        ];
        let outcomes: Vec<_> = kinds.iter().map(|miss| miss.as_str()).collect();
        assert_eq!(
            outcomes,
            ["unreadable", "job_failed", "timed_out", "start_failed"]
        );
    }

    /// The jobs past the limit wait; the limit counts every review's
    /// running jobs, so one review's jobs leave less room for another's.
    #[test]
    fn jobs_past_the_limit_wait_and_the_reviews_share_it() {
        assert_eq!(startable(10, 0, 3), 3);
        assert_eq!(startable(2, 0, 3), 2);
        assert_eq!(startable(2, 1, 3), 1);
        assert_eq!(startable(2, 2, 3), 0);
        assert_eq!(startable(2, 5, 3), 0, "a lowered limit starts none");
        // Two reviews of three jobs each under a limit of 4: the first
        // starts 3, the second the 1 left, and its other 2 wait.
        let first = startable(4, 0, 3);
        let second = startable(4, first, 3);
        assert_eq!((first, second), (3, 1));
        assert_eq!(startable(4, first + second - 1, 2), 1, "one ended");
    }

    /// Nothing is judged while any job is open, even one that missed
    /// already; once all ended, any miss fails the review with each why.
    #[test]
    fn a_review_is_judged_only_once_every_job_ended() {
        use JobStanding::{Answered, Missed, Open};
        assert_eq!(standing([Answered, Open]), ReviewStanding::Open);
        assert_eq!(standing([Missed("a"), Open]), ReviewStanding::Open);
        assert_eq!(standing([Answered, Answered]), ReviewStanding::Answered);
        assert_eq!(
            standing([
                Missed("whole: no verdict"),
                Answered,
                Missed("design: timed out")
            ]),
            ReviewStanding::Missed(vec![
                "whole: no verdict".to_owned(),
                "design: timed out".to_owned()
            ])
        );
    }

    /// An agent's reply is its one completed result with a judgment, of
    /// that agent; anything else is unreadable, so retried, never a pass.
    #[test]
    fn an_agents_reply_is_its_one_completed_result() {
        let result = agent("design", "revise", &["the map is stale"]);
        assert_eq!(result.verdict, Some(ReviewDecision::Revise));
        assert_eq!(result.reasons, ["the map is stale"]);
        for (reply, error) in [
            (None, "printed no verdict JSON"),
            (
                Some(json!({"verdict": "pass"})),
                "printed no readable verdict",
            ),
            (
                Some(json!({"agent": "other", "status": "completed", "verdict": "pass"})),
                "printed no result of design but one of other",
            ),
            (
                Some(json!({"agent": "design", "status": "failed"})),
                "design did not complete",
            ),
            (
                Some(json!({"agent": "design", "status": "completed"})),
                "design gave no verdict",
            ),
            (
                Some(
                    json!({"agent": "design", "status": "completed", "verdict": "pass", "extra": 1}),
                ),
                "printed no readable verdict",
            ),
        ] {
            let got = agent_result("design", reply).unwrap_err();
            assert!(got.contains(error), "{got}");
        }
        // The whole review judges alone: a verdict naming an agent's
        // result is unreadable.
        let naming = json!({"verdict": "pass", "reasons": [], "summary": "whole",
                            "agents": [{"agent": "design", "status": "completed", "verdict": "pass"}]});
        assert!(ReviewVerdict::parse(&naming.to_string()).is_err());
    }

    /// The judgments are taken one by one and the heaviest wins: an
    /// agent's send-back under the whole review's pass sends the run back,
    /// its reasons beside the others' that send it back; an ask outweighs
    /// both. No judgment is folded into another's (decision 4).
    #[test]
    fn the_heaviest_judgment_of_the_jobs_decides() {
        let assembled = assemble(
            whole("pass", &[]),
            vec![
                agent("design", "revise", &["stale map"]),
                agent("tests", "pass", &[]),
            ],
        );
        assert_eq!(assembled.verdict, ReviewDecision::Pass);
        let route = assembled.route(true);
        assert_eq!(route.destination, Destination::SendBack);
        assert_eq!(route.parent, Destination::Land);
        let deciding: Vec<_> = route.deciding().iter().map(|a| a.agent.clone()).collect();
        assert_eq!(deciding, ["design"]);
        let concern = assemble(
            whole("revise", &["acceptance 2 unmet"]),
            vec![
                agent_result(
                    "design",
                    Some(
                        json!({"agent": "design", "status": "completed", "verdict": "concern",
                            "reasons": [], "summary": "scope", "recommendation": "land",
                            "confidence": "high", "reason_category": "scope"}),
                    ),
                )
                .unwrap(),
            ],
        );
        assert_eq!(concern.route(true).destination, Destination::Ask);
        assert_eq!(
            assemble(whole("pass", &[]), Vec::new())
                .route(true)
                .destination,
            Destination::Land
        );
    }

    /// The paths of each reason (ADR-t2599-1) are kept with the whole
    /// review's and each agent's reasons, read back from the record, and
    /// reach the findings the judgment past the revise limit reads.
    #[test]
    fn the_paths_of_each_reason_reach_the_record_and_the_findings() {
        let whole = ReviewVerdict::parse(
            r#"{"verdict":"revise","reasons":[{"text":"w","codes":["acceptance_unmet"],"paths":["docs/a.md"]},"old"],"summary":"s"}"#,
        )
        .unwrap();
        assert_eq!(
            whole.recorded_paths(),
            [vec!["docs/a.md".to_owned()], vec![]]
        );
        let agent: AgentResult = serde_json::from_value(json!({
            "agent": "design", "status": "completed", "verdict": "revise",
            "reasons": [{"text": "d", "codes": ["acceptance_unmet"], "paths": ["src/x.rs"]}],
            "summary": "looked",
        }))
        .unwrap();
        let verdict = assemble(whole, vec![agent]);
        // Recorded and read back, the paths stay.
        let recorded: AgentResult = serde_json::from_value(json!(verdict.agents[0])).unwrap();
        assert_eq!(recorded.reason_paths, [vec!["src/x.rs".to_owned()]]);
        assert_eq!(
            json!(verdict.agents[0])["reason_paths"],
            json!([["src/x.rs"]])
        );
        let route = verdict.route(true);
        let findings = verdict.sent_back_findings(Some(&route));
        let paths: Vec<&[String]> = findings.iter().map(|f| f.paths.as_slice()).collect();
        assert_eq!(
            paths,
            [
                &["docs/a.md".to_owned()][..],
                &[][..],
                &["src/x.rs".to_owned()][..]
            ]
        );
        // Without paths nothing is recorded.
        let bare =
            ReviewVerdict::parse(r#"{"verdict":"revise","reasons":["a"],"summary":"s"}"#).unwrap();
        assert!(bare.recorded_paths().is_empty());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("reason_paths")
                .is_none()
        );
    }
}
