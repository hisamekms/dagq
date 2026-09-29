//! The headless jobs other than the worker (goal 73): per kind of job
//! (review, recovery, plan review, goal review, observer) how many ended in
//! the window, how many failed and at what rate, how long they took and the
//! verdicts they gave, and the same per provider the job was launched on
//! (its start's `launch.provider`, `claude` for a launch recorded before it
//! named one) and per model its session used (the `model` of the
//! `session_closed` of the start's `session_id`, `unknown` without one), so
//! that the providers' jobs can be read side by side.
use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::Value;

use super::{EventId, RunEvent, Summary, summary, timestamp_millis};
use crate::domain::event_kind;

/// The kinds of job, in the order `stats` lists them.
pub const JOB_KINDS: [&str; 5] = [
    "review",
    "recovery",
    "plan_review",
    "goal_review",
    "observer",
];

/// The provider of a job whose start recorded none (every job ran on
/// Claude before launches named their provider).
pub const DEFAULT_PROVIDER: &str = "claude";

/// The model or verdict of a job that recorded none.
pub const UNKNOWN: &str = "unknown";

/// The jobs of one kind, or of one provider or model of one kind.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct JobGroup {
    /// The jobs that ended in the window, the failed ones too.
    pub count: i64,
    pub failed: i64,
    /// `failed` over `count`, three decimals; null without jobs.
    pub failed_rate: Option<f64>,
    /// How long the jobs took (their end's `duration_secs`, else from
    /// their start to their end).
    pub secs: Summary,
    /// The jobs that ended with a verdict, per verdict (a review whose
    /// verdict is not recorded is `unknown`). Failed jobs, the observer, a
    /// stopped recovery job and a discarded plan review give none.
    pub verdicts: BTreeMap<String, i64>,
    /// The seconds behind `secs`, for `kpi`'s spread.
    #[serde(skip)]
    pub secs_values: Vec<i64>,
}

impl JobGroup {
    fn add(&mut self, job: &Job) {
        self.count += 1;
        self.failed += i64::from(job.failed);
        if let Some(secs) = job.secs {
            self.secs_values.push(secs);
        }
        if let Some(verdict) = &job.verdict {
            *self.verdicts.entry(verdict.clone()).or_default() += 1;
        }
    }

    fn close(&mut self) {
        #[allow(clippy::cast_precision_loss)]
        let rate = (self.count > 0)
            .then(|| ((self.failed as f64 / self.count as f64) * 1000.0).round() / 1000.0);
        self.failed_rate = rate;
        self.secs = summary(self.secs_values.iter().copied().map(Some));
    }
}

/// The jobs of one kind: all of them, and per provider and per model.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct JobStats {
    #[serde(flatten)]
    pub all: JobGroup,
    pub by_provider: BTreeMap<String, JobGroup>,
    pub by_model: BTreeMap<String, JobGroup>,
}

/// One job that ended.
struct Job {
    provider: String,
    model: String,
    failed: bool,
    verdict: Option<String>,
    secs: Option<i64>,
}

fn text(payload: &Value, key: &str) -> Option<String> {
    let value = payload.get(key)?;
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// The kind of job an event starts, and the key its end is matched by.
fn start_of(event: &RunEvent) -> Option<(&'static str, String)> {
    let run = || event.run_id.as_ref().map(|run| run.as_str().to_owned());
    match event.kind.as_str() {
        event_kind::REVIEW_STARTED => Some(("review", run()?)),
        event_kind::TRIAGE_STARTED | event_kind::RECOVERY_REQUESTED => Some(("recovery", run()?)),
        event_kind::PLAN_REVIEW_STARTED => {
            Some(("plan_review", text(&event.payload, "plan_review_id")?))
        }
        event_kind::GOAL_REVIEW_STARTED => {
            Some(("goal_review", text(&event.payload, "goal_review_id")?))
        }
        event_kind::OBSERVE_STARTED => Some(("observer", String::new())),
        _ => None,
    }
}

/// The kind of job an event ends, its key, whether it failed and its
/// verdict; `None` for an event that ends no job (an observation skipped
/// without its agent).
fn end_of(event: &RunEvent) -> Option<(&'static str, String, bool, Option<String>)> {
    let run = || event.run_id.as_ref().map(|run| run.as_str().to_owned());
    let payload = &event.payload;
    let verdict = || Some(text(payload, "verdict").unwrap_or_else(|| UNKNOWN.to_owned()));
    match event.kind.as_str() {
        event_kind::REVIEW_FINISHED => Some(("review", run()?, false, verdict())),
        event_kind::REVIEW_FAILED => Some(("review", run()?, true, None)),
        // A job stopped before its verdict (`session_ended`,
        // `dialog_cleared`, ...) ran without failing and gave none.
        event_kind::RECOVERY_FINISHED => {
            let failed = payload["outcome"] == "job_failed";
            let verdict = text(payload, "verdict").filter(|_| !failed);
            Some(("recovery", run()?, failed, verdict))
        }
        event_kind::PLAN_REVIEW_FINISHED => Some((
            "plan_review",
            text(payload, "plan_review_id")?,
            false,
            verdict(),
        )),
        event_kind::PLAN_REVIEW_FAILED => {
            Some(("plan_review", text(payload, "plan_review_id")?, true, None))
        }
        // Its proposal was edited while it ran: the job ran, and its
        // verdict was not applied.
        event_kind::PLAN_REVIEW_DISCARDED => {
            Some(("plan_review", text(payload, "plan_review_id")?, false, None))
        }
        event_kind::GOAL_REVIEW_FINISHED => Some((
            "goal_review",
            text(payload, "goal_review_id")?,
            false,
            verdict(),
        )),
        event_kind::GOAL_REVIEW_FAILED => {
            Some(("goal_review", text(payload, "goal_review_id")?, true, None))
        }
        event_kind::OBSERVE_FINISHED => match payload["outcome"].as_str() {
            Some("skipped") => None,
            outcome => Some((
                "observer",
                String::new(),
                outcome != Some("succeeded"),
                None,
            )),
        },
        _ => None,
    }
}

/// The jobs whose end has `after < id <= upto` and which `counts` accepts,
/// per kind; every kind is listed, with no job too.
pub fn jobs(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(&RunEvent) -> bool,
) -> BTreeMap<&'static str, JobStats> {
    // The model each session used, read from its close whenever it was.
    let models: HashMap<&str, &str> = events
        .iter()
        .filter(|event| event.kind == event_kind::SESSION_CLOSED)
        .filter_map(|event| {
            Some((
                event.payload.get("session_id")?.as_str()?,
                event.payload.get("model")?.as_str()?,
            ))
        })
        .collect();
    let mut stats: BTreeMap<&'static str, JobStats> = JOB_KINDS
        .iter()
        .map(|kind| (*kind, JobStats::default()))
        .collect();
    let mut started: HashMap<(&'static str, String), &RunEvent> = HashMap::new();
    for event in events.iter().filter(|event| event.id <= upto) {
        if let Some(key) = start_of(event) {
            started.insert(key, event);
            continue;
        }
        let Some((kind, key, failed, verdict)) = end_of(event) else {
            continue;
        };
        let start = started.remove(&(kind, key));
        // A recovery round that used up its jobs records its end with no
        // job started.
        if event.id <= after || !counts(event) || (kind == "recovery" && start.is_none()) {
            continue;
        }
        let launch = start.and_then(|start| start.payload.get("launch"));
        let provider = launch
            .and_then(|launch| launch.get("provider")?.as_str())
            .unwrap_or(DEFAULT_PROVIDER)
            .to_owned();
        let model = start
            .and_then(|start| models.get(start.payload.get("session_id")?.as_str()?))
            .map_or(UNKNOWN, |model| model)
            .to_owned();
        let secs = event
            .payload
            .get("duration_secs")
            .and_then(Value::as_i64)
            .or_else(|| {
                let from = timestamp_millis(&start?.created_at)?;
                Some((timestamp_millis(&event.created_at)? - from) / 1000)
            });
        let job = Job {
            provider,
            model,
            failed,
            verdict,
            secs,
        };
        let entry = stats.entry(kind).or_default();
        entry.all.add(&job);
        entry
            .by_provider
            .entry(job.provider.clone())
            .or_default()
            .add(&job);
        entry
            .by_model
            .entry(job.model.clone())
            .or_default()
            .add(&job);
    }
    for entry in stats.values_mut() {
        entry.all.close();
        entry.by_provider.values_mut().for_each(JobGroup::close);
        entry.by_model.values_mut().for_each(JobGroup::close);
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{GoalId, RunId, TaskId};
    use serde_json::json;

    fn event(id: i64, kind: &str, run: Option<&str>, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: run.map(|_| TaskId::new(1)),
            goal_id: None,
            run_id: run.map(|r| RunId::new(r).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: format!("2026-09-29T00:00:{:02}Z", id.min(59)),
            actor: None,
        }
    }

    fn goal_event(id: i64, kind: &str, goal: i64, payload: Value) -> RunEvent {
        RunEvent {
            goal_id: Some(GoalId::new(goal)),
            ..event(id, kind, None, payload)
        }
    }

    fn launch(provider: &str) -> Value {
        json!({"role": "goal_review", "provider": provider, "model": null, "effort": null, "source": "default"})
    }

    /// Claude's and Codex's goal reviews side by side: how many, how many
    /// failed, how long they took and what they said, per provider and per
    /// model; a start recorded before launches named a provider is Claude.
    #[test]
    fn goal_reviews_are_split_by_provider_and_model() {
        let events = [
            // An old Claude goal review without a launch.
            goal_event(
                1,
                event_kind::GOAL_REVIEW_STARTED,
                7,
                json!({"goal_review_id": 1}),
            ),
            goal_event(
                2,
                event_kind::GOAL_REVIEW_FINISHED,
                7,
                json!({"goal_review_id": 1, "verdict": "achieved", "duration_secs": 100}),
            ),
            goal_event(
                3,
                event_kind::GOAL_REVIEW_STARTED,
                7,
                json!({"goal_review_id": 2, "session_id": "s-claude", "launch": launch("claude")}),
            ),
            goal_event(
                4,
                event_kind::GOAL_REVIEW_FINISHED,
                7,
                json!({"goal_review_id": 2, "verdict": "gaps", "duration_secs": 300}),
            ),
            event(
                5,
                event_kind::SESSION_CLOSED,
                Some("r"),
                json!({"session_id": "s-claude", "model": "claude-opus-5-5"}),
            ),
            goal_event(
                6,
                event_kind::GOAL_REVIEW_STARTED,
                8,
                json!({"goal_review_id": 3, "session_id": "s-codex", "launch": launch("codex")}),
            ),
            goal_event(
                7,
                event_kind::GOAL_REVIEW_FAILED,
                8,
                json!({"goal_review_id": 3, "duration_secs": 40}),
            ),
            goal_event(
                8,
                event_kind::GOAL_REVIEW_STARTED,
                8,
                json!({"goal_review_id": 4, "session_id": "s-codex-2", "launch": launch("codex")}),
            ),
            goal_event(
                9,
                event_kind::GOAL_REVIEW_FINISHED,
                8,
                json!({"goal_review_id": 4, "verdict": "achieved"}),
            ),
            event(
                10,
                event_kind::SESSION_CLOSED,
                Some("r"),
                json!({"session_id": "s-codex-2", "model": "gpt-5.5"}),
            ),
        ];
        let stats = jobs(&events, EventId::new(0), EventId::new(10), |_| true);
        assert_eq!(stats.keys().copied().collect::<Vec<_>>(), {
            let mut kinds = JOB_KINDS.to_vec();
            kinds.sort_unstable();
            kinds
        });
        let goal = &stats["goal_review"];
        assert_eq!((goal.all.count, goal.all.failed), (4, 1));
        assert_eq!(goal.all.failed_rate, Some(0.25));
        let claude = &goal.by_provider["claude"];
        assert_eq!(
            (claude.count, claude.failed, claude.failed_rate),
            (2, 0, Some(0.0))
        );
        assert_eq!(
            (claude.secs.count, claude.secs.total, claude.secs.median),
            (2, 400, Some(200))
        );
        assert_eq!(claude.verdicts["achieved"], 1);
        assert_eq!(claude.verdicts["gaps"], 1);
        let codex = &goal.by_provider["codex"];
        assert_eq!(
            (codex.count, codex.failed, codex.failed_rate),
            (2, 1, Some(0.5))
        );
        // Without a duration, from its start to its end.
        assert_eq!((codex.secs.count, codex.secs.total), (2, 41));
        assert_eq!(codex.verdicts, BTreeMap::from([("achieved".to_owned(), 1)]));
        assert_eq!(goal.by_model["claude-opus-5-5"].count, 1);
        assert_eq!(goal.by_model["gpt-5.5"].verdicts["achieved"], 1);
        assert_eq!(goal.by_model[UNKNOWN].count, 2);
        assert_eq!(stats["review"].all.count, 0);
        assert_eq!(stats["review"].all.failed_rate, None);
        // The window counts the ends after `after`, and `counts` filters.
        let later = jobs(&events, EventId::new(4), EventId::new(10), |_| true);
        assert_eq!(
            later["goal_review"].by_provider.keys().collect::<Vec<_>>(),
            ["codex"]
        );
        let goal_seven = jobs(&events, EventId::new(0), EventId::new(10), |event| {
            event.goal_id == Some(GoalId::new(7))
        });
        assert_eq!(goal_seven["goal_review"].all.count, 2);
    }

    /// Each kind pairs its ends with its starts: the review per run, the
    /// recovery per run from `triage_started` or `recovery_requested`, the
    /// plan review per its id, the observer in order, its skipped
    /// observations no job.
    #[test]
    fn every_kind_pairs_its_ends_with_its_starts() {
        let codex = json!({"provider": "codex"});
        let events = [
            event(
                1,
                event_kind::REVIEW_STARTED,
                Some("a"),
                json!({"launch": codex}),
            ),
            event(
                2,
                event_kind::REVIEW_FINISHED,
                Some("a"),
                json!({"verdict": "revise", "duration_secs": 30}),
            ),
            event(3, event_kind::REVIEW_STARTED, Some("a"), json!({})),
            event(
                4,
                event_kind::REVIEW_FAILED,
                Some("a"),
                json!({"duration_secs": 5}),
            ),
            event(
                5,
                event_kind::TRIAGE_STARTED,
                Some("b"),
                json!({"launch": codex}),
            ),
            event(
                6,
                event_kind::RECOVERY_FINISHED,
                Some("b"),
                json!({"verdict": "repair", "duration_secs": 12}),
            ),
            event(7, event_kind::RECOVERY_REQUESTED, Some("b"), json!({})),
            event(
                8,
                event_kind::RECOVERY_FINISHED,
                Some("b"),
                json!({"outcome": "job_failed", "duration_secs": 3}),
            ),
            event(
                9,
                event_kind::PLAN_REVIEW_STARTED,
                Some("c"),
                json!({"plan_review_id": 5, "launch": codex}),
            ),
            event(
                10,
                event_kind::PLAN_REVIEW_FINISHED,
                Some("c"),
                json!({"plan_review_id": 5, "verdict": "pass", "duration_secs": 60}),
            ),
            event(
                11,
                event_kind::PLAN_REVIEW_FAILED,
                Some("c"),
                json!({"plan_review_id": 6}),
            ),
            event(
                12,
                event_kind::OBSERVE_STARTED,
                None,
                json!({"launch": codex}),
            ),
            event(
                13,
                event_kind::OBSERVE_FINISHED,
                None,
                json!({"outcome": "succeeded", "duration_secs": 90}),
            ),
            event(
                14,
                event_kind::OBSERVE_FINISHED,
                None,
                json!({"outcome": "skipped"}),
            ),
            event(15, event_kind::OBSERVE_STARTED, None, json!({})),
            event(
                16,
                event_kind::OBSERVE_FINISHED,
                None,
                json!({"outcome": "failed"}),
            ),
            // Stopped without a verdict, then a round that used up its jobs.
            event(17, event_kind::RECOVERY_REQUESTED, Some("b"), json!({})),
            event(
                18,
                event_kind::RECOVERY_FINISHED,
                Some("b"),
                json!({"outcome": "session_ended"}),
            ),
            event(
                19,
                event_kind::RECOVERY_FINISHED,
                Some("b"),
                json!({"escalated": true}),
            ),
            event(
                20,
                event_kind::PLAN_REVIEW_STARTED,
                Some("c"),
                json!({"plan_review_id": 7}),
            ),
            event(
                21,
                event_kind::PLAN_REVIEW_DISCARDED,
                Some("c"),
                json!({"plan_review_id": 7, "verdict": "pass"}),
            ),
        ];
        let stats = jobs(&events, EventId::new(0), EventId::new(21), |_| true);
        let review = &stats["review"];
        assert_eq!((review.all.count, review.all.failed), (2, 1));
        assert_eq!(review.by_provider["codex"].verdicts["revise"], 1);
        assert_eq!(review.by_provider["claude"].failed, 1);
        let recovery = &stats["recovery"];
        assert_eq!((recovery.all.count, recovery.all.failed), (3, 1));
        assert_eq!(
            recovery.all.verdicts,
            BTreeMap::from([("repair".to_owned(), 1)])
        );
        assert_eq!(recovery.by_provider["codex"].verdicts["repair"], 1);
        assert_eq!(recovery.by_provider["claude"].failed, 1);
        // The stopped job took from its start to its end.
        assert_eq!(recovery.all.secs.total, 16);
        let plan = &stats["plan_review"];
        assert_eq!((plan.all.count, plan.all.failed), (3, 1));
        assert_eq!(plan.all.verdicts, BTreeMap::from([("pass".to_owned(), 1)]));
        assert_eq!(plan.by_provider["codex"].verdicts["pass"], 1);
        // A failure without its start (and without a duration) is Claude's,
        // with no time; the discarded one took from its start to its end.
        assert_eq!(
            (
                plan.by_provider["claude"].count,
                plan.by_provider["claude"].secs.count
            ),
            (2, 1)
        );
        let observer = &stats["observer"];
        assert_eq!((observer.all.count, observer.all.failed), (2, 1));
        assert!(observer.all.verdicts.is_empty());
        assert_eq!(observer.by_provider["codex"].count, 1);
        assert_eq!(observer.by_provider["claude"].failed, 1);
        // The failed observation took from its start to its end.
        assert_eq!(observer.by_provider["claude"].secs.total, 1);
        let json = serde_json::to_value(&stats["goal_review"]).unwrap();
        assert_eq!(json["count"], 0);
        assert!(json.get("secs_values").is_none());
        assert_eq!(json["by_provider"], json!({}));
    }
}
