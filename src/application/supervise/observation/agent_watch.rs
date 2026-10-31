//! The watch of the adopted agent definitions (ADR-t1728-1 (g)): at most
//! once every [`AGENT_WATCH_CHECK`], a supervisor at work reads the
//! agents with case lists on the landing branch, the production reviews,
//! the production sets, the eval's rounds and the watch's findings from
//! the queue (the reviews only from where the agents' periods start,
//! [`crate::domain::agent_eval::watch::watch_reads_from`]), and records
//! what [`crate::domain::agent_eval::watch`] says: the steps of each
//! agent's watch and the scores under the threshold as findings on the
//! queue (each once, by its subject, so of two
//! supervisors one records it), and an interval with no new review as
//! `agent_eval_watch_skipped`. A score is judged against each agent's
//! `AGENT.md` on the landing branch's head; one whose definition there
//! could not be read is logged with why and judged again at the next
//! check. It asks for no set and no round: a runtime
//! planner the findings open does. It takes no run slot and uses no LLM; a
//! failure is logged and looked at again at the next check. Each check
//! reads `[eval]` again, and with its `watch` off returns before it reads
//! anything.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::application::RepositoryReads;
use crate::domain::agent_eval::production::{self, Period, ReviewEvent};
use crate::domain::agent_eval::record;
use crate::domain::agent_eval::round::{EvalConfig, definition_digest};
use crate::domain::agent_eval::watch::{
    self, Adopted, SCORE_FINDING_KIND, WATCH_FINDING_KIND, Watch, WatchFinding, WatchRecord,
};
use crate::domain::review_subagents::definition_path;
use crate::domain::{FindingQuery, FindingTarget, FindingView, NewFinding, RunEvent};

/// How often the watch looks when the supervisor's CLI runs it.
pub const AGENT_WATCH_CHECK: Duration = Duration::from_secs(3600);

/// When the watch last looked.
#[derive(Default)]
pub(in crate::application::supervise) struct AgentWatch {
    looked: Option<Instant>,
}

/// What one look recorded.
#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::application::supervise) struct Looked {
    /// The findings recorded, by subject.
    pub(in crate::application::supervise) findings: Vec<String>,
    /// The agents whose interval passed with no new review.
    pub(in crate::application::supervise) skipped: Vec<String>,
    /// Why rounds under the threshold were not judged: their agent's
    /// definition on the landing branch could not be read.
    pub(in crate::application::supervise) unjudged: Vec<String>,
}

impl ObservationState {
    /// Look when it is time, with `start`: a supervisor at work only.
    pub(in crate::application::supervise) fn agent_watch_pass(
        &mut self,
        env: &mut ObservationEnv<'_>,
        start: bool,
    ) {
        let Some(check) = self.agent_watch_check else {
            return;
        };
        let clock = &env.generators.clock;
        if !start
            || self
                .agent_watch
                .looked
                .is_some_and(|at| clock.since(at) < check)
        {
            return;
        }
        self.agent_watch.looked = Some(clock.monotonic());
        let config = match env.verifier.eval_config() {
            Ok(config) => config,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the agents' watch could not read [eval]: {error:#}");
                return;
            }
        };
        let now = env.generators.clock.now().saturating_mul(1000);
        let Some(looked) = look_when_on(&config, |config| {
            look(&*env.queue, env.repository, config, now, env.token)
        }) else {
            return;
        };
        match looked {
            Ok(looked) => {
                for subject in &looked.findings {
                    info!("agents' watch recorded the finding {subject}");
                }
                for agent in &looked.skipped {
                    info!(
                        "agents' watch of {agent}: no new production review since its last watch"
                    );
                }
                for why in &looked.unjudged {
                    warn!("agents' watch: {why}");
                }
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the agents' watch could not look: {error:#}");
            }
        }
    }
}

/// `look` with `config` when its `[eval]` `watch` is on; `None` without
/// calling it when off, so the check reads no event and records nothing.
fn look_when_on(
    config: &EvalConfig,
    look: impl FnOnce(&EvalConfig) -> Result<Looked>,
) -> Option<Result<Looked>> {
    config.watch.enabled.then(|| look(config))
}

/// One look at `now` (Unix milliseconds) as supervisor `token`.
fn look(
    queue: &(dyn ObservationQueue + Send),
    repository: &(dyn RepositoryReads + Send + Sync),
    config: &EvalConfig,
    now: i64,
    token: &LeaseToken,
) -> Result<Looked> {
    let commit = repository.main_head()?;
    let paths = repository.tree_paths(commit.as_str())?;
    let agents = watch::watched_agents(paths.iter().map(String::as_str));
    let mut kinds: Vec<&str> = record::KINDS
        .iter()
        .chain(production::KINDS.iter())
        .map(|kind| kind.as_str())
        .collect();
    kinds.push(EventKind::AgentEvalWatchSkipped.as_str());
    let upto = queue.latest_event_id()?;
    let every = usize::try_from(i64::MAX).unwrap_or(usize::MAX);
    // The small events first: the reviews are read only from where the
    // agents' periods they decide start.
    let events = queue.events_of_between(&kinds, EventId::new(0), upto, every)?;
    let triples = || {
        events
            .iter()
            .map(|event| (event.id.as_i64(), &event.kind, &event.payload))
    };
    let rounds = record::rounds(triples());
    let sets = production::sets(triples());
    let findings = queue.findings(&FindingQuery {
        all: true,
        kinds: vec![WATCH_FINDING_KIND.to_owned(), SCORE_FINDING_KIND.to_owned()],
        target: Some(FindingTarget::Queue),
        ..FindingQuery::default()
    })?;
    let mut known: BTreeSet<String> = findings
        .iter()
        .map(|view| format!("{}\t{}", view.finding.kind, view.finding.subject))
        .collect();
    let mut looked = Looked::default();
    let mut record = |found: WatchFinding, looked: &mut Looked| -> Result<bool> {
        if !known.insert(format!("{}\t{}", found.kind, found.subject)) {
            return Ok(false);
        }
        let recorded = queue.record_finding_once(NewFinding {
            kind: found.kind.to_owned(),
            target: FindingTarget::Queue,
            subject: found.subject.clone(),
            summary: found.summary,
            detail: Some(found.detail.to_string()),
            impact: None,
            evidence: found.evidence.map(EventId::new).into_iter().collect(),
            propose: Some(found.propose),
            by: crate::domain::ActorRole::Supervisor.as_str().to_owned(),
        })?;
        if recorded.is_some() {
            looked.findings.push(found.subject);
        }
        Ok(recorded.is_some())
    };
    // What became of the sets and rounds a planner asked for, and the
    // scores under the threshold.
    let mut stepped: BTreeSet<String> = BTreeSet::new();
    let mut definitions: BTreeMap<String, std::result::Result<Option<String>, String>> =
        BTreeMap::new();
    let mut unread: BTreeSet<String> = BTreeSet::new();
    for set in &sets {
        let built = watch::set_event(&events, EventKind::AgentCasesBuilt, set.id);
        if let Some(found) = watch::set_finding(set, &rounds, built)
            && record(found, &mut looked)?
        {
            stepped.insert(set.request.agent.clone());
        }
    }
    for round in &rounds {
        let refused = watch::eval_event(&events, EventKind::AgentEvalRefused, round.id);
        if let Some(found) = watch::refused_finding(round, refused)
            && record(found, &mut looked)?
        {
            stepped.insert(round.request.agent.clone());
        }
        let finished = watch::eval_event(&events, EventKind::AgentEvalFinished, round.id);
        let agent = &round.request.agent;
        let adopted = definitions.entry(agent.clone()).or_insert_with(|| {
            repository
                .file_in(commit.as_str(), &definition_path(agent))
                .map(|text| text.as_deref().map(definition_digest))
                .map_err(|error| format!("{error:#}"))
        });
        let adopted = match adopted {
            Ok(Some(digest)) => Adopted::Digest(digest),
            Ok(None) => Adopted::Absent,
            Err(why) => Adopted::Unread(why),
        };
        match watch::below_finding(round, finished, adopted) {
            Ok(Some(found)) => {
                record(found, &mut looked)?;
            }
            Ok(None) => {}
            // One reason per agent: its other rounds wait on the same read.
            Err(why) if unread.insert(agent.clone()) => looked.unjudged.push(why),
            Err(_) => {}
        }
    }
    // Each agent's watch, from where its last period ended.
    let mut watched = Vec::new();
    for agent in &agents {
        let mine: Vec<&FindingView> = findings
            .iter()
            .filter(|view| {
                view.finding.kind == WATCH_FINDING_KIND
                    && watch::of_agent(&view.finding.subject, agent)
            })
            .collect();
        let open = mine
            .iter()
            .find(|view| view.finding.status.is_unsettled())
            .map(|view| view.finding.id.as_i64());
        // A step recorded now is open too.
        if stepped.contains(agent) {
            continue;
        }
        let records: Vec<WatchRecord<'_>> = mine
            .iter()
            .map(|view| WatchRecord {
                id: view.finding.id.as_i64(),
                detail: &view.finding.detail,
                first_seen: view.finding.first_seen_at.saturating_mul(1000),
            })
            .collect();
        let marks = watch::marks(watch::agent_marks(agent, &records, &events, &rounds, &sets));
        watched.push((agent, marks, open));
    }
    let from = watch::watch_reads_from(watched.iter().map(|(_, marks, _)| marks.since), now);
    let limit = usize::try_from(upto.as_i64()).unwrap_or_default();
    let reviews = queue.events_between(EventId::new(0), upto, &watch::review_reads(from), limit)?;
    for (agent, marks, open) in watched {
        let (verdict, until, reviews) = judge(agent, config, &marks, open, &reviews, now);
        match verdict {
            Watch::Due => {
                let found = watch::due_finding(agent, &marks, &reviews, until, commit.as_str());
                record(found, &mut looked)?;
            }
            Watch::NoNewReviews => {
                let mut payload = watch::skipped_record(agent, &marks, until, now);
                payload["supervisor"] = json!(token);
                queue.record_queue_event(EventKind::AgentEvalWatchSkipped, payload)?;
                looked.skipped.push(agent.clone());
            }
            Watch::Open(_) | Watch::NotYet => {}
        }
    }
    Ok(looked)
}

/// What the watch does for `agent` at `now` (Unix milliseconds), its
/// `marks` and its `open` finding, of the production `reviews` read: the
/// decision, the end of its new period and the reviews in it. A period
/// with no start counts the reviews from [`watch::reviews_from`], as far
/// back as they are read.
fn judge(
    agent: &str,
    config: &EvalConfig,
    marks: &watch::Marks,
    open: Option<i64>,
    reviews: &[RunEvent],
    now: i64,
) -> (Watch, i64, Vec<production::Candidate>) {
    let running = production::in_flight(agent, reviews.iter().filter_map(review_event));
    let until = watch::period_end(marks.since, now, &running);
    let found = production::candidates(
        agent,
        Period {
            since: Some(watch::reviews_from(marks.since, now)),
            until: Some(until),
        },
        reviews.iter().filter_map(review_event),
    );
    let verdict = watch::decide(config.watch, marks.last.as_ref(), found.len(), open, now);
    (verdict, until, found)
}

fn review_event(event: &RunEvent) -> Option<ReviewEvent<'_>> {
    Some(ReviewEvent {
        run_id: event.run_id.as_ref()?.as_str(),
        kind: &event.kind,
        payload: &event.payload,
        created_at: &event.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RunId;
    use crate::domain::agent_eval::production::event_instant;
    use crate::domain::agent_eval::watch::{
        IN_FLIGHT_LIMIT_MILLIS, LastWatch, Marks, Previous, REVIEW_LOOKBACK_MILLIS,
    };
    use crate::domain::marks::utc_text;

    const NOW: i64 = 1_790_000_000_000;
    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 24 * HOUR;

    /// A production review of `agent` by run `run` started at `start` and,
    /// with `end`, finished then with a verdict.
    fn review(id: &mut i64, run: &str, agent: &str, start: i64, end: Option<i64>) -> Vec<RunEvent> {
        let event = |id: i64, kind: EventKind, payload: serde_json::Value, at: i64| RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: Some(RunId::new(run).unwrap()),
            kind: kind.into(),
            payload,
            created_at: utc_text(at),
            actor: None,
        };
        *id += 1;
        let mut events = vec![event(
            *id,
            EventKind::ReviewStarted,
            json!({"attempt": 1, "subagents": {"base": "b", "head": "h", "agents": [{"agent": agent}]}}),
            start,
        )];
        if let Some(end) = end {
            *id += 1;
            events.push(event(
                *id,
                EventKind::ReviewFinished,
                json!({"attempt": 1, "agents": [{"agent": agent, "verdict": "pass", "reasons": []}]}),
                end,
            ));
        }
        events
    }

    fn marks(since: Option<i64>, last: Option<i64>) -> Marks {
        Marks {
            last: last.map(|at| LastWatch {
                at,
                through: since,
                previous: Previous::Skipped(1),
            }),
            since,
        }
    }

    /// The look reads the reviews from the earliest agent's start or the
    /// in-flight limit, by their time, and judges each agent's period, its
    /// end with the reviews in flight and its due or skipped interval as a
    /// look that read every review: of reviews long before every period,
    /// one never finished, ones in flight and ones in the periods.
    #[test]
    fn a_look_from_the_earliest_start_judges_as_one_that_read_every_review() {
        let mut id = 0;
        let mut every = Vec::new();
        // Long before every period, finished and not.
        every.extend(review(
            &mut id,
            "old1",
            "rules",
            NOW - 40 * DAY,
            Some(NOW - 40 * DAY + HOUR),
        ));
        every.extend(review(
            &mut id,
            "old2",
            "other",
            NOW - 20 * DAY,
            Some(NOW - 20 * DAY + HOUR),
        ));
        every.extend(review(&mut id, "old3", "rules", NOW - 9 * DAY, None));
        every.extend(review(
            &mut id,
            "week4",
            "idle",
            NOW - 6 * DAY,
            Some(NOW - 6 * DAY + HOUR),
        ));
        // In the periods, in flight within the limit and past it.
        every.extend(review(
            &mut id,
            "new1",
            "rules",
            NOW - 2 * DAY,
            Some(NOW - 2 * DAY + HOUR),
        ));
        every.extend(review(
            &mut id,
            "new2",
            "other",
            NOW - 3 * DAY,
            Some(NOW - 3 * DAY + HOUR),
        ));
        every.extend(review(&mut id, "new3", "rules", NOW - 30 * HOUR, None));
        every.extend(review(&mut id, "new4", "rules", NOW - 2 * HOUR, None));
        every.extend(review(
            &mut id,
            "new5",
            "rules",
            NOW - HOUR,
            Some(NOW - HOUR / 2),
        ));
        every.sort_by_key(|event| event.id);
        let config = EvalConfig::default();
        // rules: watched 3 days ago; other: never watched; idle: watched a
        // week and a day ago, with no review since 5 days ago.
        let agents = [
            ("rules", marks(Some(NOW - 3 * DAY), Some(NOW - 3 * DAY))),
            ("other", marks(None, None)),
            ("idle", marks(Some(NOW - 5 * DAY), Some(NOW - 8 * DAY))),
        ];
        let from = watch::watch_reads_from(agents.iter().map(|(_, marks)| marks.since), NOW);
        assert_eq!(from, NOW - REVIEW_LOOKBACK_MILLIS);
        let filter = watch::review_reads(from);
        assert_eq!(filter.since, Some(utc_text(from)));
        let read: Vec<RunEvent> = every
            .iter()
            .filter(|event| event_instant(&event.created_at).is_some_and(|at| at >= from))
            .cloned()
            .collect();
        assert!(read.len() < every.len());
        assert!(
            read.iter()
                .all(|event| !event.run_id.as_ref().unwrap().as_str().starts_with("old"))
        );
        for (agent, marks) in &agents {
            let all = judge(agent, &config, marks, None, &every, NOW);
            let part = judge(agent, &config, marks, None, &read, NOW);
            assert_eq!(all, part, "{agent}");
        }
        let (rules, until, found) = judge("rules", &config, &agents[0].1, None, &read, NOW);
        assert_eq!(until, NOW - 2 * HOUR);
        assert_eq!(
            found.iter().map(|c| c.run_id.as_str()).collect::<Vec<_>>(),
            ["new1"]
        );
        assert_eq!(rules, Watch::NotYet);
        let (other, _, found) = judge("other", &config, &agents[1].1, None, &read, NOW);
        assert_eq!((other, found.len()), (Watch::Due, 1));
        let (idle, until, found) = judge("idle", &config, &agents[2].1, None, &read, NOW);
        assert_eq!((idle, until, found.len()), (Watch::NoNewReviews, NOW, 0));
        // An open finding holds the watch, as before.
        assert_eq!(
            judge("other", &config, &agents[1].1, Some(7), &read, NOW).0,
            Watch::Open(7)
        );
        // Agents watched within the limit read only what may be in flight.
        let near = [Some(NOW - HOUR)];
        assert_eq!(
            watch::watch_reads_from(near, NOW),
            NOW - IN_FLIGHT_LIMIT_MILLIS
        );
    }

    /// With `[eval]` `watch = false` the check does not look: it reads no
    /// event and records no finding and no `agent_eval_watch_skipped`; on
    /// (the default) it looks as before.
    #[test]
    fn the_watch_looks_only_when_eval_watch_is_on() {
        let mut off = EvalConfig::default();
        off.watch.enabled = false;
        let mut called = false;
        assert!(
            look_when_on(&off, |_| {
                called = true;
                Ok(Looked::default())
            })
            .is_none()
        );
        assert!(!called);

        let on = EvalConfig::default();
        let looked = look_when_on(&on, |_| {
            Ok(Looked {
                skipped: vec!["demo".to_owned()],
                ..Looked::default()
            })
        });
        assert_eq!(looked.unwrap().unwrap().skipped, vec!["demo".to_owned()]);
    }
}
