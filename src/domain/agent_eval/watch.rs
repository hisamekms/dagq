//! The watch of an adopted definition (ADR-t1728-1 (g)): the supervisor
//! judges when an agent's definition on the landing branch is due to be
//! measured again on new production reviews, and records each step as a
//! finding a runtime planner takes. It never asks for a set or a round
//! itself (both are a person's, the inbox's or a planner's): the planner
//! the finding opens asks for them. The steps, each a finding of
//! [`WATCH_FINDING_KIND`] on the queue with its own subject, recorded once:
//!
//! - due ([`decide`], [`due_finding`]): the agent selected
//!   `watch_new_reviews` production reviews in the new period (from where
//!   the last one ended, [`agent_marks`], to before the earliest review in
//!   flight, [`period_end`]), or
//!   `watch_interval_days` passed with at least one; the planner asks for
//!   the production set of the period.
//! - hold-out ([`set_finding`]): a set a planner asked for is built and no
//!   round runs it; the planner asks for its hold-out.
//! - refused ([`refused_finding`]): such a round of a planner's was not
//!   started (the hold-out's once rule, the cost limit); the planner notes
//!   why and dismisses it, never asking to run it again.
//!
//! Apart from them, a hold-out or production round on the landing branch's
//! definition (its digest that of the `AGENT.md` there now) that ends
//! under the threshold is a finding of
//! [`SCORE_FINDING_KIND`] ([`below_finding`]), whoever asked for it.
//! Everything here is without side effects; the supervisor reads the
//! events and the landing branch's tree and records what it returns.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::production::{Candidate, CaseSet};
use super::record::Round;
use super::round::held_out;
use super::{EVALS_DIR, Split};
use crate::domain::marks::utc_text;
use crate::domain::review_subagents::{DEFINITION_DIR, valid_agent_name};
use crate::domain::run::restore_payload;
use crate::domain::{EventKind, RunEvent};

/// The kind of the watch's findings: one per step, by its subject.
pub const WATCH_FINDING_KIND: &str = "agent_eval_watch_due";

/// The kind of the finding of a held-out round that ended under the
/// threshold.
pub const SCORE_FINDING_KIND: &str = "agent_eval_below_threshold";

/// `[eval] watch_new_reviews` when it is not set.
pub const DEFAULT_WATCH_NEW_REVIEWS: usize = 10;

/// `[eval] watch_interval_days` when it is not set.
pub const DEFAULT_WATCH_INTERVAL_DAYS: u32 = 7;

/// The requester whose sets and rounds the watch follows: the runtime's
/// planner its findings open.
const PLANNER: &str = "planner";

const DAY_MILLIS: i64 = 86_400_000;

/// Whether the watch looks (`[eval]`'s `watch`) and when it is due
/// (`watch_new_reviews` and `watch_interval_days`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchConfig {
    /// Whether the supervisor looks at all: when false, each check of the
    /// watch returns before it reads an event or records anything.
    pub enabled: bool,
    /// The new production reviews that make it due at once.
    pub new_reviews: usize,
    /// The days (of 24 hours) after the last watch that make it due with
    /// at least one new review.
    pub interval_days: u32,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            new_reviews: DEFAULT_WATCH_NEW_REVIEWS,
            interval_days: DEFAULT_WATCH_INTERVAL_DAYS,
        }
    }
}

/// The agents watched at a commit, by name: each with a file under
/// `.dagq/agents/<name>/evals/` in `paths` (the commit's tree).
pub fn watched_agents<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let prefix = format!("{DEFINITION_DIR}/");
    let agents: BTreeSet<String> = paths
        .into_iter()
        .filter_map(|path| {
            let rest = path.strip_prefix(&prefix)?;
            let mut parts = rest.splitn(3, '/');
            let (name, dir, file) = (parts.next()?, parts.next()?, parts.next()?);
            (dir == EVALS_DIR && !file.is_empty() && valid_agent_name(name))
                .then(|| name.to_owned())
        })
        .collect();
    agents.into_iter().collect()
}

/// How long a review that started and did not finish holds a period's
/// end ([`period_end`]): longer than any review job runs, so one whose
/// supervisor or run went away does not hold the watch for ever.
pub const IN_FLIGHT_LIMIT_MILLIS: i64 = DAY_MILLIS;

/// What watched an agent last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Previous {
    /// A due finding of the watch, by its id, at its first time seen.
    Finding(i64),
    /// A production round of the agent, by its id, at its start.
    Round(i64),
    /// An interval with no new review (`agent_eval_watch_skipped`), by
    /// its event's id.
    Skipped(i64),
}

/// A watch of the agent: when it was (Unix milliseconds), from which the
/// interval counts, and `through`, the start of the next period's reviews
/// (those that started at or after it), when it ends a period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastWatch {
    pub at: i64,
    pub through: Option<i64>,
    pub previous: Previous,
}

impl LastWatch {
    pub fn record(&self) -> Value {
        let (kind, id) = match self.previous {
            Previous::Finding(id) => ("finding", id),
            Previous::Round(id) => ("production_round", id),
            Previous::Skipped(id) => ("skipped", id),
        };
        json!({"kind": kind, "id": id, "at": utc_text(self.at), "through": self.through.map(utc_text)})
    }
}

/// Where an agent's watch stands: its latest watch, from which the
/// interval counts, and `since`, where the new period starts (the latest
/// `through`; open when none ended a period).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marks {
    pub last: Option<LastWatch>,
    pub since: Option<i64>,
}

/// The [`Marks`] of an agent's `watches`.
pub fn marks(watches: impl IntoIterator<Item = LastWatch>) -> Marks {
    let watches: Vec<LastWatch> = watches.into_iter().collect();
    Marks {
        since: watches.iter().filter_map(|watch| watch.through).max(),
        last: watches.into_iter().max_by_key(|watch| watch.at),
    }
}

/// A finding of the watch's kind as [`agent_marks`] reads it.
#[derive(Debug, Clone, Copy)]
pub struct WatchRecord<'a> {
    pub id: i64,
    /// Its `detail` text.
    pub detail: &'a str,
    /// When it was first seen, Unix milliseconds.
    pub first_seen: i64,
}

/// The mark of a finding of the watch: a due one at its first time seen,
/// ending its period at its `until`, so the reviews while the set is made
/// and run are the next period's; the hold-out and refused steps none
/// (they hold the watch only while open).
pub fn finding_mark(record: &WatchRecord<'_>) -> Option<LastWatch> {
    let detail: restored::Due = restore_payload(&serde_json::from_str(record.detail).ok()?);
    (detail.step.as_deref() == Some("due")).then(|| LastWatch {
        at: record.first_seen,
        through: Some(
            detail
                .until
                .as_deref()
                .and_then(super::production::boundary)
                .unwrap_or(record.first_seen),
        ),
        previous: Previous::Finding(record.id),
    })
}

/// Every mark of `agent`'s watch: its due findings ([`finding_mark`]), its
/// production rounds that started (at their start; one of a set ends a
/// period at the set's `--until`, after which no set holds the reviews,
/// one of the repository's list ends none), and its skipped intervals
/// (at their time, ending a period at their `through`). `events` are the
/// queue's events of the eval and of the watch, `rounds` and `sets` folded
/// from them.
pub fn agent_marks(
    agent: &str,
    findings: &[WatchRecord<'_>],
    events: &[RunEvent],
    rounds: &[Round],
    sets: &[CaseSet],
) -> Vec<LastWatch> {
    let mut marks: Vec<LastWatch> = findings.iter().filter_map(finding_mark).collect();
    for event in events {
        let Some(at) = super::production::event_instant(&event.created_at) else {
            continue;
        };
        if event.kind == EventKind::AgentEvalStarted {
            let Some(round) = restore_payload::<restored::EvalOf>(&event.payload)
                .eval_id
                .and_then(|id| rounds.iter().find(|round| round.id == id))
            else {
                continue;
            };
            if round.request.agent != agent || round.request.split != Split::Production {
                continue;
            }
            let until = round
                .request
                .set
                .and_then(|set| sets.iter().find(|found| found.id == set))
                .and_then(|found| found.request.until.as_deref())
                .and_then(super::production::boundary);
            marks.push(LastWatch {
                at,
                through: until,
                previous: Previous::Round(round.id),
            });
        } else if event.kind == EventKind::AgentEvalWatchSkipped {
            let skipped: restored::Skipped = restore_payload(&event.payload);
            if skipped.agent.as_deref() != Some(agent) {
                continue;
            }
            marks.push(LastWatch {
                at,
                through: Some(
                    skipped
                        .through
                        .as_deref()
                        .and_then(super::production::boundary)
                        .unwrap_or(at),
                ),
                previous: Previous::Skipped(event.id.as_i64()),
            });
        }
    }
    marks
}

/// Where the period from `since` ends at `now` (Unix milliseconds):
/// before the earliest review still in flight (`in_flight`, their starts,
/// [`super::production::in_flight`]) that started within
/// [`IN_FLIGHT_LIMIT_MILLIS`], else now, and never before `since`. A
/// review counts by its start, so one that finishes after a watch is the
/// next period's, never lost between two.
pub fn period_end(since: Option<i64>, now: i64, in_flight: &[i64]) -> i64 {
    let held = in_flight
        .iter()
        .copied()
        .filter(|start| now.saturating_sub(*start) < IN_FLIGHT_LIMIT_MILLIS)
        .min()
        .map_or(now, |start| start.min(now));
    since.map_or(held, |since| held.max(since))
}

/// The id of the event of `kind` that names round `eval_id`.
pub fn eval_event(events: &[RunEvent], kind: EventKind, eval_id: i64) -> Option<i64> {
    events
        .iter()
        .find(|event| {
            event.kind == kind
                && restore_payload::<restored::EvalOf>(&event.payload).eval_id == Some(eval_id)
        })
        .map(|event| event.id.as_i64())
}

/// The id of the event of `kind` that names production set `set_id`.
pub fn set_event(events: &[RunEvent], kind: EventKind, set_id: i64) -> Option<i64> {
    events
        .iter()
        .find(|event| {
            event.kind == kind
                && restore_payload::<restored::SetOf>(&event.payload).set_id == Some(set_id)
        })
        .map(|event| event.id.as_i64())
}

/// Whether a finding's subject is of `agent`'s watch (`<agent>/...`).
pub fn of_agent(subject: &str, agent: &str) -> bool {
    subject
        .strip_prefix(agent)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// What the watch does for an agent now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watch {
    /// A finding of the agent's watch is open (by its id): nothing is
    /// recorded until it is settled.
    Open(i64),
    /// Neither the reviews nor the interval reached: nothing is recorded.
    NotYet,
    /// The interval passed with no new production review: no finding,
    /// and the reason is recorded (`agent_eval_watch_skipped`), from which
    /// the next interval counts.
    NoNewReviews,
    /// Record the due finding ([`due_finding`]).
    Due,
}

/// What the watch does for an agent at `now` (Unix milliseconds): `last`
/// its last watch (none for an agent never watched, whose interval has
/// passed), `reviews` the production reviews of its new period, and
/// `open` its open watch finding.
pub fn decide(
    config: WatchConfig,
    last: Option<&LastWatch>,
    reviews: usize,
    open: Option<i64>,
    now: i64,
) -> Watch {
    if let Some(open) = open {
        return Watch::Open(open);
    }
    let interval = i64::from(config.interval_days).saturating_mul(DAY_MILLIS);
    let passed = last.is_none_or(|last| now.saturating_sub(last.at) >= interval);
    if reviews >= config.new_reviews.max(1) || (passed && reviews > 0) {
        Watch::Due
    } else if passed {
        Watch::NoNewReviews
    } else {
        Watch::NotYet
    }
}

/// The `agent_eval_watch_skipped` payload of [`Watch::NoNewReviews`] at
/// `now`: the period it closes ends at `through` ([`period_end`]).
pub fn skipped_record(agent: &str, marks: &Marks, through: i64, now: i64) -> Value {
    json!({
        "agent": agent,
        "reason": "no_new_reviews",
        "previous": marks.last.as_ref().map(LastWatch::record),
        "since": marks.since.map(utc_text),
        "through": utc_text(through),
        "at": utc_text(now),
    })
}

/// The payloads the watch decides by, restored as typed values
/// (`run::payload::restore`): a missing key, `null` or a value of another
/// type reads as absent.
mod restored {
    use serde::Deserialize;
    use serde_json::Value;

    use crate::domain::run::payload::typed;

    /// An event of a round of the eval, by `eval_id`.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct EvalOf {
        #[serde(default, deserialize_with = "typed")]
        pub(super) eval_id: Option<i64>,
    }

    /// An event of a production set, by `set_id`.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct SetOf {
        #[serde(default, deserialize_with = "typed")]
        pub(super) set_id: Option<i64>,
    }

    /// `agent_eval_watch_skipped`.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct Skipped {
        #[serde(default, deserialize_with = "typed")]
        pub(super) agent: Option<String>,
        #[serde(default, deserialize_with = "typed")]
        pub(super) through: Option<String>,
    }

    /// A finding of the watch's detail.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct Due {
        #[serde(default, deserialize_with = "typed")]
        pub(super) step: Option<String>,
        #[serde(default, deserialize_with = "typed")]
        pub(super) until: Option<String>,
    }

    /// `agent_eval_refused`.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct Refused {
        #[serde(default, deserialize_with = "typed")]
        pub(super) reason: Option<String>,
        #[serde(default)]
        pub(super) detail: Value,
    }

    /// One value under the threshold of `agent_eval_finished`.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct Below {
        #[serde(default, deserialize_with = "typed")]
        pub(super) metric: Option<String>,
    }

    /// `agent_eval_finished`: what the score finding decides by; the
    /// scores it carries are copied as they are.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct Finished {
        #[serde(default, deserialize_with = "typed")]
        pub(super) outcome: Option<String>,
        #[serde(default, deserialize_with = "typed")]
        pub(super) below: Option<Vec<Below>>,
        #[serde(default, deserialize_with = "typed")]
        pub(super) definition_digest: Option<String>,
    }
}

/// A finding of the watch to record: its subject, summary, detail (a JSON
/// object, recorded as its text), why a planner is wanted, and the event
/// it is about, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct WatchFinding {
    pub kind: &'static str,
    pub subject: String,
    pub summary: String,
    pub detail: Value,
    pub propose: String,
    pub evidence: Option<i64>,
}

/// The due finding of `agent`: the period from `marks.since` (open when
/// none) to `until` ([`period_end`]), the reviews in it, and the set to ask
/// for (`agent cases <agent> --since --until`) on the landing branch's
/// `commit`. The subject names where the period starts, so two
/// supervisors record one finding.
pub fn due_finding(
    agent: &str,
    marks: &Marks,
    reviews: &[Candidate],
    until: i64,
    commit: &str,
) -> WatchFinding {
    let since = marks.since.map(utc_text);
    let until = utc_text(until);
    let command = match &since {
        Some(since) => format!("agent cases {agent} --since {since} --until {until}"),
        None => format!("agent cases {agent} --until {until}"),
    };
    WatchFinding {
        kind: WATCH_FINDING_KIND,
        subject: format!(
            "{agent}/due since {}",
            since.as_deref().unwrap_or("the start")
        ),
        summary: format!(
            "{agent}'s definition is due to be watched: {} new production review(s)",
            reviews.len()
        ),
        detail: json!({
            "step": "due",
            "agent": agent,
            "since": since,
            "until": until,
            "previous": marks.last.as_ref().map(LastWatch::record),
            "reviews": reviews.len(),
            "first_review_at": reviews.first().map(|review| &review.reviewed_at),
            "last_review_at": reviews.last().map(|review| &review.reviewed_at),
            "landing_commit": commit,
            "ask": command,
        }),
        propose: format!(
            "ask for the production set of {agent}'s new reviews ({command}); its hold-out follows once it is built"
        ),
        evidence: None,
    }
}

/// The hold-out finding of `set`: a set a planner asked for, built, that
/// no round of `rounds` runs. `built` is its `agent_cases_built` event.
pub fn set_finding(set: &CaseSet, rounds: &[Round], built: Option<i64>) -> Option<WatchFinding> {
    let made = set.built.as_ref()?;
    if set.request.requested_by != PLANNER
        || rounds.iter().any(|round| round.request.set == Some(set.id))
    {
        return None;
    }
    let agent = &set.request.agent;
    let command = format!("agent eval {agent} --set {}", set.id);
    Some(WatchFinding {
        kind: WATCH_FINDING_KIND,
        subject: format!("{agent}/set {}", set.id),
        summary: format!(
            "{agent}'s production set {} is built with no hold-out yet",
            set.id
        ),
        detail: json!({
            "step": "holdout",
            "agent": agent,
            "set": set.id,
            "digest": made.digest,
            "cases": made.cases.len(),
            "ask": command,
        }),
        propose: format!(
            "ask for the hold-out of set {} on the landing branch's definition ({command})",
            set.id
        ),
        evidence: built,
    })
}

/// The refused finding of `round`: a planner's round of a production set
/// that was not started. `refused` is its `agent_eval_refused` event.
pub fn refused_finding(round: &Round, refused: Option<i64>) -> Option<WatchFinding> {
    let why = round.refused.as_ref()?;
    let set = round.request.set?;
    if round.request.requested_by != PLANNER {
        return None;
    }
    let agent = &round.request.agent;
    let why: restored::Refused = restore_payload(why);
    let reason = why.reason.as_deref().unwrap_or("unknown");
    Some(WatchFinding {
        kind: WATCH_FINDING_KIND,
        subject: format!("{agent}/eval {} refused", round.id),
        summary: format!(
            "{agent}'s hold-out of set {set} (eval {}) was not started: {reason}",
            round.id
        ),
        detail: json!({
            "step": "refused",
            "agent": agent,
            "eval_id": round.id,
            "set": set,
            "reason": reason,
            "why": why.detail,
        }),
        propose: format!(
            "eval {} was refused ({reason}): note why and dismiss this finding; a planner does not ask to run it again",
            round.id
        ),
        evidence: refused,
    })
}

/// The agent's definition on the landing branch's head, as the score
/// finding compares a round with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adopted<'a> {
    /// The digest ([`super::round::definition_digest`]) of its `AGENT.md`.
    Digest(&'a str),
    /// The landing branch has no `AGENT.md` of the agent: no definition is
    /// adopted.
    Absent,
    /// Its `AGENT.md` could not be read, and why.
    Unread(&'a str),
}

/// The score finding of `round`: a hold-out or production round, not a
/// run's dev before its landing, on the landing branch's definition (none
/// read from another commit), that ran every run and has a value under its
/// threshold, with its failed cases' ids and reasons. Only a round whose
/// `definition_digest` is `adopted`'s now is one: a round of a definition
/// replaced since, or of none adopted, is none, and one whose definition
/// on the landing branch could not be read is not judged, with why (the
/// error). `finished` is its `agent_eval_finished` event.
pub fn below_finding(
    round: &Round,
    finished: Option<i64>,
    adopted: Adopted<'_>,
) -> Result<Option<WatchFinding>, String> {
    let Some(scores) = round.finished.as_ref() else {
        return Ok(None);
    };
    let read: restored::Finished = restore_payload(scores);
    let Some(below) = read.below.filter(|below| !below.is_empty()) else {
        return Ok(None);
    };
    if !held_out(round.request.split)
        || round.request.before_landing
        || round.request.definition_commit.is_some()
        || read.outcome.as_deref() != Some("complete")
    {
        return Ok(None);
    }
    match adopted {
        Adopted::Digest(digest) if read.definition_digest.as_deref() == Some(digest) => {}
        Adopted::Digest(_) | Adopted::Absent => return Ok(None),
        Adopted::Unread(why) => {
            return Err(format!(
                "eval {} of {} is not judged: its definition on the landing branch could not be read: {why}",
                round.id, round.request.agent
            ));
        }
    }
    let agent = &round.request.agent;
    let failed: Vec<Value> = scores["failed_cases"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|case| json!({"id": case["id"], "reasons": case["reasons"]}))
        .collect();
    let metrics: Vec<&str> = below
        .iter()
        .filter_map(|value| value.metric.as_deref())
        .collect();
    let split = match (round.request.split, round.request.set) {
        (Split::Production, Some(set)) => format!("production set {set}"),
        (split, _) => split.as_str().to_owned(),
    };
    Ok(Some(WatchFinding {
        kind: SCORE_FINDING_KIND,
        subject: format!("{agent}/eval {}", round.id),
        summary: format!(
            "{agent}'s {split} (eval {}) is under the threshold: {}",
            round.id,
            metrics.join(", ")
        ),
        detail: json!({
            "agent": agent,
            "eval_id": round.id,
            "split": round.request.split.as_str(),
            "set": round.request.set,
            "threshold": scores["threshold"],
            "scores": scores["scores"],
            "below": scores["below"],
            "per_code": scores["per_code"],
            "failed_cases": failed,
            "definition_commit": round.started.as_ref().map(|started| &started.definition_commit),
            "definition_digest": scores["definition_digest"],
            "case_set_digest": scores["case_set_digest"],
        }),
        propose: format!(
            "improve {agent}'s definition on its failed cases ({} of eval {})",
            failed.len(),
            round.id
        ),
        evidence: finished,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent_eval::production::{Built, CasesRequest};
    use crate::domain::agent_eval::record::Request;

    const NOW: i64 = 1_790_000_000_000;

    /// The landing branch's definition the score finding's rounds ran.
    const ADOPTED: Adopted<'static> = Adopted::Digest("dd");

    fn config() -> WatchConfig {
        WatchConfig {
            enabled: true,
            new_reviews: 10,
            interval_days: 7,
        }
    }

    fn at(days_ago: i64) -> LastWatch {
        LastWatch {
            at: NOW - days_ago * DAY_MILLIS,
            through: None,
            previous: Previous::Finding(1),
        }
    }

    fn event(id: i64, kind: EventKind, payload: Value, created_at: &str) -> RunEvent {
        RunEvent {
            id: crate::domain::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: created_at.to_owned(),
            actor: None,
        }
    }

    fn request(split: Split, set: Option<i64>, by: &str) -> Request {
        Request {
            agent: "rules".to_owned(),
            split,
            k: None,
            cases: None,
            set,
            rerun: false,
            requested_by: by.to_owned(),
            requested_by_id: "planner-1".to_owned(),
            run_id: None,
            before_landing: false,
            definition_commit: None,
        }
    }

    fn round(id: i64, request: Request) -> Round {
        Round {
            id,
            request,
            refused: None,
            waiting: None,
            started: None,
            started_payload: None,
            runs: Vec::new(),
            checks: Default::default(),
            base_failing: Default::default(),
            finished: None,
            owner: None,
        }
    }

    fn set(id: i64, by: &str, built: bool) -> CaseSet {
        CaseSet {
            id,
            request: CasesRequest {
                agent: "rules".to_owned(),
                since: None,
                until: None,
                requested_by: by.to_owned(),
                requested_by_id: "planner-1".to_owned(),
            },
            refused: None,
            frame: None,
            prepared: None,
            skipped: Vec::new(),
            labels: Default::default(),
            built: built.then(|| Built {
                digest: "d".to_owned(),
                cases: vec!["prod-r-a1".to_owned()],
            }),
            decisions: Default::default(),
            owner: None,
        }
    }

    /// The agents watched are those with a case list under their
    /// definition's evals/, by a valid name, each once.
    #[test]
    fn the_agents_watched_are_those_with_case_lists() {
        let paths = [
            ".dagq/agents/rules/AGENT.md",
            ".dagq/agents/rules/evals/dev.json",
            ".dagq/agents/rules/evals/holdout.json",
            ".dagq/agents/plain/AGENT.md",
            ".dagq/agents/Bad_Name/evals/dev.json",
            ".dagq/agents/other/evals/production.json",
            ".dagq/agent-cases/patches/ab.patch",
        ];
        assert_eq!(watched_agents(paths), ["other", "rules"]);
    }

    /// A finding is of an agent's watch by the name before its subject's
    /// first `/`, never by a longer name that starts the same.
    #[test]
    fn a_subject_is_of_the_agent_it_starts_with() {
        assert!(of_agent("rules/due since the start", "rules"));
        assert!(of_agent("rules/set 3", "rules"));
        assert!(!of_agent("rules-extra/set 3", "rules"));
        assert!(!of_agent("rules", "rules"));
    }

    /// The watch is due at the reviews' count, or once the interval passed
    /// with a review; an interval with none records why, an open finding
    /// holds it, and before either nothing is recorded.
    #[test]
    fn the_watch_is_due_at_the_count_or_the_interval_with_a_review() {
        let config = config();
        assert_eq!(decide(config, Some(&at(1)), 10, None, NOW), Watch::Due);
        assert_eq!(decide(config, Some(&at(1)), 9, None, NOW), Watch::NotYet);
        assert_eq!(decide(config, Some(&at(7)), 1, None, NOW), Watch::Due);
        assert_eq!(
            decide(config, Some(&at(7)), 0, None, NOW),
            Watch::NoNewReviews
        );
        assert_eq!(decide(config, Some(&at(6)), 0, None, NOW), Watch::NotYet);
        // Never watched: its interval has passed.
        assert_eq!(decide(config, None, 1, None, NOW), Watch::Due);
        assert_eq!(decide(config, None, 0, None, NOW), Watch::NoNewReviews);
        // An open finding of its watch holds every case.
        assert_eq!(
            decide(config, Some(&at(7)), 12, Some(5), NOW),
            Watch::Open(5)
        );
        assert_eq!(
            decide(config, Some(&at(7)), 0, Some(5), NOW),
            Watch::Open(5)
        );
        // The interval is measured to the millisecond.
        let just = LastWatch {
            at: NOW - 7 * DAY_MILLIS + 1,
            through: None,
            previous: Previous::Skipped(3),
        };
        assert_eq!(decide(config, Some(&just), 1, None, NOW), Watch::NotYet);
    }

    /// The interval counts from the latest watch and the new period from
    /// the latest end of one: a due finding ends its period at its `until`
    /// (the reviews while its set is made and run are the next period's),
    /// a round of a set at the set's `--until`, one of the repository's
    /// list none, a skip at its `through`; the hold-out and refused steps
    /// and another agent's records are no marks.
    #[test]
    fn the_new_period_starts_where_the_last_one_ended() {
        let t = |text: &str| super::super::production::boundary(text).unwrap();
        let due = json!({"step": "due", "until": "2026-10-05T00:00:00.000Z"}).to_string();
        let holdout = json!({"step": "holdout", "set": 3}).to_string();
        let findings = [
            WatchRecord {
                id: 1,
                detail: &due,
                first_seen: t("2026-10-05T00:00:05.000Z"),
            },
            WatchRecord {
                id: 2,
                detail: &holdout,
                first_seen: t("2026-10-06T00:00:00.000Z"),
            },
            WatchRecord {
                id: 3,
                detail: "not json",
                first_seen: t("2026-10-06T00:00:00.000Z"),
            },
        ];
        let mut of_set = set(20, "planner", true);
        of_set.request.until = Some("2026-10-05T00:00:00.000Z".to_owned());
        let rounds = [
            round(30, request(Split::Production, Some(20), "planner")),
            round(31, request(Split::Holdout, None, "user")),
        ];
        let events = [
            event(
                40,
                EventKind::AgentEvalStarted,
                json!({"eval_id": 30}),
                "2026-10-05T08:00:00.000Z",
            ),
            event(
                41,
                EventKind::AgentEvalStarted,
                json!({"eval_id": 31}),
                "2026-10-09T00:00:00.000Z",
            ),
            event(
                42,
                EventKind::AgentEvalWatchSkipped,
                json!({"agent": "other", "through": "2026-10-09T00:00:00.000Z"}),
                "2026-10-09T00:00:00.000Z",
            ),
        ];
        let found = agent_marks("rules", &findings, &events, &rounds, &[of_set]);
        assert_eq!(
            found,
            [
                LastWatch {
                    at: t("2026-10-05T00:00:05.000Z"),
                    through: Some(t("2026-10-05T00:00:00.000Z")),
                    previous: Previous::Finding(1),
                },
                LastWatch {
                    at: t("2026-10-05T08:00:00.000Z"),
                    through: Some(t("2026-10-05T00:00:00.000Z")),
                    previous: Previous::Round(30),
                },
            ]
        );
        let read = marks(found);
        assert_eq!(read.since, Some(t("2026-10-05T00:00:00.000Z")));
        assert_eq!(read.last.unwrap().previous, Previous::Round(30));
        // A skip ends its period at its through, its interval at its time.
        let skip = event(
            43,
            EventKind::AgentEvalWatchSkipped,
            json!({"agent": "rules", "through": "2026-10-07T12:00:00.000Z"}),
            "2026-10-07T12:01:00.000Z",
        );
        let read = marks(agent_marks("rules", &[], &[skip], &[], &[]));
        assert_eq!(read.since, Some(t("2026-10-07T12:00:00.000Z")));
        assert_eq!(read.last.unwrap().at, t("2026-10-07T12:01:00.000Z"));
        assert_eq!(marks([]), Marks::default());
        assert_eq!(
            eval_event(&events, EventKind::AgentEvalStarted, 31),
            Some(41)
        );
        assert_eq!(set_event(&events, EventKind::AgentCasesBuilt, 20), None);
    }

    /// A review in flight when a watch closes a period is the next
    /// period's: the period ends before it starts, so once it finishes it
    /// is counted, never lost between the two (a review started at 12:00,
    /// a skip at 12:01, its end at 12:02). One in flight longer than the
    /// limit no longer holds the end, and the end is never before `since`.
    #[test]
    fn a_review_in_flight_at_a_watch_is_counted_in_the_next_period() {
        use super::super::production::{Period, ReviewEvent, boundary, candidates, in_flight};
        let started = json!({"attempt": 1, "subagents": {"base": "b", "head": "h", "agents": [{"agent": "rules"}]}});
        let finished =
            json!({"attempt": 1, "agents": [{"agent": "rules", "verdict": "pass", "reasons": []}]});
        let kinds: [crate::domain::RecordedKind; 2] = [
            EventKind::ReviewStarted.into(),
            EventKind::ReviewFinished.into(),
        ];
        let start = ReviewEvent {
            run_id: "r1",
            kind: &kinds[0],
            payload: &started,
            created_at: "2026-10-07T12:00:00.000Z",
        };
        let end = ReviewEvent {
            run_id: "r1",
            kind: &kinds[1],
            payload: &finished,
            created_at: "2026-10-07T12:02:00.000Z",
        };
        let noon = boundary("2026-10-07T12:00:00.000Z").unwrap();
        let skip_at = boundary("2026-10-07T12:01:00.000Z").unwrap();
        // At 12:01 the review runs: the period ends before it, none counted.
        let running = in_flight("rules", [start]);
        assert_eq!(running, [noon]);
        let through = period_end(None, skip_at, &running);
        assert_eq!(through, noon);
        let counted = |since, until, events: Vec<ReviewEvent<'_>>| {
            candidates(
                "rules",
                Period {
                    since,
                    until: Some(until),
                },
                events,
            )
            .len()
        };
        assert_eq!(counted(None, through, vec![start]), 0);
        // At 12:03 it finished: it is in the period that starts at through.
        let later = boundary("2026-10-07T12:03:00.000Z").unwrap();
        assert_eq!(in_flight("rules", [start, end]), Vec::<i64>::new());
        let until = period_end(Some(through), later, &[]);
        assert_eq!(until, later);
        assert_eq!(counted(Some(through), until, vec![start, end]), 1);
        // Another agent's review holds nothing.
        assert_eq!(in_flight("other", [start]), Vec::<i64>::new());
        // Past the limit a review never finished holds the end no more.
        let stale = later + IN_FLIGHT_LIMIT_MILLIS;
        assert_eq!(period_end(Some(noon), stale, &[noon]), stale);
        // Never before since.
        assert_eq!(period_end(Some(later), later + 5, &[noon]), later);
    }

    /// The due finding names the period from the last watch to now and the
    /// set to ask for in it; the subject names where it starts.
    #[test]
    fn the_due_finding_names_its_period_and_the_set_to_ask_for() {
        let review = |at: &str| Candidate {
            run_id: "r".to_owned(),
            attempt: 1,
            base: "b".to_owned(),
            head: "h".to_owned(),
            verdict: "pass".to_owned(),
            reasons: Vec::new(),
            reviewed_at: at.to_owned(),
        };
        let reviews = [
            review("2026-10-01T00:00:00.000Z"),
            review("2026-10-02T00:00:00.000Z"),
        ];
        let last = at(7);
        let marks = Marks {
            since: Some(last.at),
            last: Some(last.clone()),
        };
        let finding = due_finding("rules", &marks, &reviews, NOW, "c0ffee");
        let since = utc_text(last.at);
        let until = utc_text(NOW);
        assert_eq!(finding.kind, WATCH_FINDING_KIND);
        assert_eq!(finding.subject, format!("rules/due since {since}"));
        assert_eq!(finding.detail["reviews"], 2);
        assert_eq!(
            finding.detail["first_review_at"],
            "2026-10-01T00:00:00.000Z"
        );
        assert_eq!(finding.detail["last_review_at"], "2026-10-02T00:00:00.000Z");
        assert_eq!(finding.detail["previous"]["kind"], "finding");
        assert_eq!(
            finding.detail["ask"],
            format!("agent cases rules --since {since} --until {until}")
        );
        // The period's ends read back as the set's boundaries.
        assert_eq!(super::super::production::boundary(&since), Some(last.at));
        let first = due_finding("rules", &Marks::default(), &reviews, NOW, "c0ffee");
        assert_eq!(first.subject, "rules/due since the start");
        assert_eq!(
            first.detail["ask"],
            format!("agent cases rules --until {until}")
        );
    }

    /// A planner's built set with no round on it asks for its hold-out; a
    /// set of another requester, one not built, or one a round runs does
    /// not.
    #[test]
    fn a_planners_built_set_with_no_round_asks_for_its_holdout() {
        let finding = set_finding(&set(20, "planner", true), &[], Some(21)).unwrap();
        assert_eq!(finding.subject, "rules/set 20");
        assert_eq!(finding.detail["ask"], "agent eval rules --set 20");
        assert_eq!(finding.evidence, Some(21));
        assert!(set_finding(&set(20, "inbox", true), &[], None).is_none());
        assert!(set_finding(&set(20, "planner", false), &[], None).is_none());
        let on_it = round(30, request(Split::Production, Some(20), "planner"));
        assert!(set_finding(&set(20, "planner", true), &[on_it], None).is_none());
    }

    /// A planner's round of a set that was refused is told with its
    /// reason, to be noted and dismissed; another's, or one of no set, is
    /// not.
    #[test]
    fn a_planners_refused_set_round_is_told_with_its_reason() {
        let mut refused = round(30, request(Split::Production, Some(20), "planner"));
        refused.refused = Some(json!({"reason": "holdout_used", "detail": "run already"}));
        let finding = refused_finding(&refused, Some(31)).unwrap();
        assert_eq!(finding.subject, "rules/eval 30 refused");
        assert_eq!(finding.detail["reason"], "holdout_used");
        assert!(finding.propose.contains("dismiss"));
        let mut person = refused.clone();
        person.request.requested_by = "user".to_owned();
        assert!(refused_finding(&person, None).is_none());
        let mut no_set = refused.clone();
        no_set.request.set = None;
        assert!(refused_finding(&no_set, None).is_none());
        let started = round(30, request(Split::Production, Some(20), "planner"));
        assert!(refused_finding(&started, None).is_none());
    }

    /// A held-out round on the landing branch's definition that ended
    /// under the threshold is a finding with its failed cases' ids and
    /// reasons, whoever asked for it; a dev round, a run's dev before its
    /// landing, an incomplete round and one that passed are not.
    #[test]
    fn a_held_out_round_under_the_threshold_names_its_failed_cases() {
        let finished = json!({
            "outcome": "complete",
            "threshold": 0.9,
            "scores": {"judgment_recall": 0.67},
            "below": [{"metric": "judgment_recall", "value": 0.67}],
            "failed_cases": [{"id": "h-3", "runs": 1, "correct": 0, "errors": 0, "reasons": [["MIG-2: no down"]]}],
            "per_code": [],
            "definition_digest": "dd",
            "case_set_digest": "cd",
        });
        for (split, set, by) in [
            (Split::Holdout, None, "user"),
            (Split::Production, Some(20), "planner"),
        ] {
            let mut ended = round(40, request(split, set, by));
            ended.finished = Some(finished.clone());
            let finding = below_finding(&ended, Some(41), ADOPTED).unwrap().unwrap();
            assert_eq!(finding.kind, SCORE_FINDING_KIND);
            assert_eq!(finding.subject, "rules/eval 40");
            assert_eq!(finding.detail["failed_cases"][0]["id"], "h-3");
            assert_eq!(
                finding.detail["failed_cases"][0]["reasons"][0][0],
                "MIG-2: no down"
            );
            assert_eq!(finding.detail["below"][0]["metric"], "judgment_recall");
            assert!(
                finding.summary.contains("judgment_recall"),
                "{}",
                finding.summary
            );
        }
        let mut dev = round(40, request(Split::Dev, None, "worker"));
        dev.finished = Some(finished.clone());
        assert_eq!(below_finding(&dev, None, ADOPTED), Ok(None));
        let mut before_landing = round(40, request(Split::Holdout, None, "supervisor"));
        before_landing.request.before_landing = true;
        before_landing.finished = Some(finished.clone());
        assert_eq!(below_finding(&before_landing, None, ADOPTED), Ok(None));
        let mut runs_head = round(40, request(Split::Holdout, None, "user"));
        runs_head.request.definition_commit = Some("c0ffee".to_owned());
        runs_head.finished = Some(finished.clone());
        assert_eq!(below_finding(&runs_head, None, ADOPTED), Ok(None));
        let mut incomplete = round(40, request(Split::Holdout, None, "user"));
        incomplete.finished = Some(json!({"outcome": "incomplete", "below": finished["below"]}));
        assert_eq!(below_finding(&incomplete, None, ADOPTED), Ok(None));
        let mut passed = round(40, request(Split::Holdout, None, "user"));
        passed.finished = Some(json!({"outcome": "complete", "below": []}));
        assert_eq!(below_finding(&passed, None, ADOPTED), Ok(None));
        assert_eq!(
            below_finding(
                &round(40, request(Split::Holdout, None, "user")),
                None,
                ADOPTED
            ),
            Ok(None)
        );
    }

    /// The finished round of [`ADOPTED`]'s definition under the threshold.
    fn below_round() -> Round {
        let mut ended = round(40, request(Split::Holdout, None, "user"));
        ended.finished = Some(json!({
            "outcome": "complete",
            "below": [{"metric": "judgment_recall", "value": 0.67}],
            "failed_cases": [{"id": "h-3", "reasons": [["MIG-2: no down"]]}],
            "definition_digest": "dd",
        }));
        ended
    }

    /// A round under the threshold is a finding only while its definition
    /// is the landing branch's now: one of a definition replaced since, or
    /// of an agent the landing branch has no definition of, is none.
    #[test]
    fn a_round_of_a_replaced_definition_is_no_finding() {
        let ended = below_round();
        let finding = below_finding(&ended, Some(41), ADOPTED).unwrap().unwrap();
        assert_eq!(finding.detail["failed_cases"][0]["id"], "h-3");
        assert_eq!(
            finding.detail["failed_cases"][0]["reasons"][0][0],
            "MIG-2: no down"
        );
        assert_eq!(
            below_finding(&ended, Some(41), Adopted::Digest("ee")),
            Ok(None)
        );
        assert_eq!(below_finding(&ended, Some(41), Adopted::Absent), Ok(None));
        let mut undigested = below_round();
        undigested.finished.as_mut().unwrap()["definition_digest"] = Value::Null;
        assert_eq!(below_finding(&undigested, Some(41), ADOPTED), Ok(None));
    }

    /// A round under the threshold whose agent's definition on the landing
    /// branch could not be read is no finding, and says why; one that
    /// would be no finding anyway says nothing.
    #[test]
    fn a_round_whose_definition_could_not_be_read_is_not_judged_with_why() {
        let why = below_finding(&below_round(), Some(41), Adopted::Unread("bad blob")).unwrap_err();
        assert!(why.contains("eval 40 of rules"), "{why}");
        assert!(why.contains("bad blob"), "{why}");
        let mut passed = below_round();
        passed.finished.as_mut().unwrap()["below"] = json!([]);
        assert_eq!(
            below_finding(&passed, None, Adopted::Unread("bad blob")),
            Ok(None)
        );
    }
}
