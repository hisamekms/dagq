//! The quality of the plans (ADR-0079 decision 7, task 579): what a
//! proposal was like when plan review took it (its features, recorded on
//! `plan_review_started`), the model and effort of the plan review session
//! that judged it (its `session_closed`) and of the planner span that
//! submitted it ([`PlannerSubmission`]), and what came of it: the revises
//! of plan review, the tasks canceled as duplicates after they were ready,
//! the follow-ups adopted into it and canceled later, and the task-caused
//! rework of its tasks' runs. Pure: derived from `run_events` only, split
//! into strata by those models and efforts and each feature, for `kpi`.
use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::{
    EventId, RunEvent, TaskId, cancel_reason,
    event_kind::{
        DRAFT_PLANNER_OPENED, PLAN_REVIEW_STARTED, PLAN_REVISE_SENT, PROPOSAL_RESUBMITTED,
        PROPOSAL_SUBMITTED, TASK_SUBMITTED,
    },
    sessions::{PLAN_REVIEW, PLANNER, RUNTIME_PLANNER, SESSION_CLOSED, SESSION_OPENED},
};

/// Where a proposal came from: a follow-up draft, a goal's gap, an
/// observer's finding, a planner of the runtime's (a plan review sent it
/// back while its planner was closed, or a draft), or a person's planner.
pub const FOLLOW_UP: &str = "follow_up";
pub const GOAL_GAP: &str = "goal_gap";
pub const OBSERVER: &str = "observer";

/// Below it a proposal's closest existing task is `low`; see
/// [`related_tier`].
pub const RELATED_MID: f64 = 3.0;
/// From it on the closest existing task is `high`.
pub const RELATED_HIGH: f64 = 6.0;

/// The stratum of every proposal and plan review.
const ALL: &str = "all";
const UNKNOWN: &str = "unknown";

/// What a proposal was like when plan review took it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProposalFeatures {
    /// [`FOLLOW_UP`], [`GOAL_GAP`], [`OBSERVER`], or the owner's origin
    /// (`runtime` / `person`).
    pub origin: String,
    /// 0, 1 for a follow-up of a task, 2 for a follow-up of a follow-up...:
    /// the deepest of its tasks.
    pub follow_up_depth: i64,
    /// The highest `dagq related` score between one of its tasks and a task
    /// outside it; `None` when none scores.
    pub related_score: Option<f64>,
    /// How many times plan review had sent it back.
    pub revise_count: i64,
}

impl ProposalFeatures {
    /// The `features` of a `plan_review_started`.
    pub fn payload(&self) -> Value {
        json!({
            "origin": self.origin,
            "follow_up_depth": self.follow_up_depth,
            "related_score": self.related_score,
            "related": related_tier(self.related_score),
            "revise_count": self.revise_count,
        })
    }
}

/// The origin of a proposal from what its tasks and goals came from, the
/// most specific first.
pub fn origin(
    follow_up: bool,
    goal_gap: bool,
    finding: bool,
    owner: super::PlannerOrigin,
) -> String {
    if follow_up {
        FOLLOW_UP
    } else if goal_gap {
        GOAL_GAP
    } else if finding {
        OBSERVER
    } else {
        owner.as_str()
    }
    .to_owned()
}

/// `low` (below [`RELATED_MID`]), `mid` or `high` (from [`RELATED_HIGH`]);
/// `low` too when nothing scores.
pub fn related_tier(score: Option<f64>) -> &'static str {
    match score {
        Some(score) if score >= RELATED_HIGH => "high",
        Some(score) if score >= RELATED_MID => "mid",
        _ => "low",
    }
}

/// The measures of one stratum of a window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Quality {
    /// Plan reviews that finished with a verdict in the window.
    pub reviews: usize,
    /// Of them, those that sent the proposal back.
    pub revises: usize,
    /// Proposals whose tasks first became `ready` in the window.
    pub proposals: usize,
    /// Their tasks canceled as duplicates after being `ready` (so far).
    pub duplicate_cancels_after_ready: usize,
    /// Their follow-up tasks (adopted by being submitted with them).
    pub follow_ups: usize,
    /// Of them, those canceled since.
    pub follow_ups_canceled_after_adoption: usize,
    /// Their tasks that have a run.
    pub tasks_run: usize,
    /// Of them, those with task-caused rework (ADR-0079 decision 1).
    pub tasks_reworked: usize,
}

/// One plan review, as its events recorded it.
#[derive(Default)]
struct Review {
    proposal: Option<i64>,
    /// Its `plan_review_started`.
    started: Option<EventId>,
    features: Value,
    session: Option<EventId>,
    model: Option<String>,
    effort: Option<String>,
    planner: PlannerSubmission,
    /// Its `plan_review_finished`: the event and its decision.
    finished: Option<(EventId, String)>,
    anchor: Option<TaskId>,
}

impl Review {
    /// Its strata: `all`; `model=` / `effort=`, those of the plan review
    /// job's session that judged it; `planner_model=` / `planner_effort=`
    /// ([`PlannerSubmission::strata`]); and each of the proposal's
    /// features. A missing record is `unknown`.
    fn strata(&self) -> Vec<String> {
        let text = |value: Option<&str>| value.unwrap_or(UNKNOWN).to_owned();
        let feature = |key: &str| match &self.features[key] {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            _ => UNKNOWN.to_owned(),
        };
        vec![
            ALL.to_owned(),
            format!("model={}", text(self.model.as_deref())),
            format!("effort={}", text(self.effort.as_deref())),
            format!("origin={}", feature("origin")),
            format!("follow_up_depth={}", feature("follow_up_depth")),
            format!("related={}", feature("related")),
            format!("revise_count={}", feature("revise_count")),
        ]
        .into_iter()
        .chain(self.planner.strata())
        .collect()
    }
}

/// The planner span a proposal's plan review is attributed to: that of
/// the last submission before the review started, so when several
/// planners made and remade the proposal the last one to submit it
/// counts, whoever owns the proposal now. `submit` links it on
/// `proposal_submitted` (the submitting planner and its open span); a
/// retry with no planner (the supervisor's restart of a held review, a
/// person's resubmission) leaves the last planner's span. Older records
/// without that link fall back to the submitting actor, the last
/// `plan_revise_sent` or `draft_planner_opened`, or a span of the
/// proposal, open at the submission. Without a span or its
/// `session_closed`'s model / effort, they are `None`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PlannerSubmission {
    /// The submission it is read from.
    pub submitted: Option<EventId>,
    pub planner_id: Option<i64>,
    /// The planner span's `session_opened`.
    pub opened_event_id: Option<EventId>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl PlannerSubmission {
    /// Its strata: `planner_model=` / `planner_effort=`, the model and
    /// effort of the span, `unknown` without them.
    fn strata(&self) -> [String; 2] {
        let text = |value: Option<&str>| value.unwrap_or(UNKNOWN).to_owned();
        [
            format!("planner_model={}", text(self.model.as_deref())),
            format!("planner_effort={}", text(self.effort.as_deref())),
        ]
    }
}

/// Read the last planner submission before `before` and its span.
/// Retries without a planner leave the last planner's attribution intact.
/// Closed metadata can arrive after review; later submissions and spans
/// must never change which span is attributed to this review.
pub fn planner_submission(
    events: &[RunEvent],
    proposal: i64,
    before: EventId,
) -> PlannerSubmission {
    Submissions::new(events).before(proposal, before)
}

/// The records [`planner_submission`] reads, indexed in one pass so that
/// `kpi` reads every review's planner without scanning the events again.
#[derive(Default)]
struct Submissions<'a> {
    /// By proposal: its `proposal_submitted`, `task_submitted`,
    /// `proposal_resubmitted`, `plan_revise_sent` and `plan_review_started`.
    by_proposal: HashMap<i64, Vec<&'a RunEvent>>,
    /// By task: its `draft_planner_opened`.
    drafts: HashMap<Option<TaskId>, Vec<&'a RunEvent>>,
    /// The `session_opened` of the planner spans, by its ID, by planner
    /// and by the proposal it names.
    spans: HashMap<i64, &'a RunEvent>,
    spans_of_planner: HashMap<i64, Vec<&'a RunEvent>>,
    spans_of_proposal: HashMap<i64, Vec<&'a RunEvent>>,
    /// By the span it closes: the first `session_closed` read and the
    /// earliest one's ID.
    closes: HashMap<i64, (&'a RunEvent, EventId)>,
}

impl<'a> Submissions<'a> {
    fn new(events: &'a [RunEvent]) -> Self {
        let mut index = Self::default();
        for event in events {
            let id = |key: &str| event.payload[key].as_i64();
            match event.kind.as_str() {
                PROPOSAL_SUBMITTED | TASK_SUBMITTED | PROPOSAL_RESUBMITTED | PLAN_REVISE_SENT
                | PLAN_REVIEW_STARTED => {
                    if let Some(proposal) = id("proposal_id") {
                        index.by_proposal.entry(proposal).or_default().push(event);
                    }
                }
                DRAFT_PLANNER_OPENED => {
                    index.drafts.entry(event.task_id).or_default().push(event);
                }
                SESSION_OPENED
                    if matches!(
                        event.payload["kind"].as_str(),
                        Some(PLANNER | RUNTIME_PLANNER)
                    ) =>
                {
                    index.spans.insert(event.id.as_i64(), event);
                    if let Some(planner) = id("planner_id") {
                        index
                            .spans_of_planner
                            .entry(planner)
                            .or_default()
                            .push(event);
                    }
                    if let Some(proposal) = id("proposal_id") {
                        index
                            .spans_of_proposal
                            .entry(proposal)
                            .or_default()
                            .push(event);
                    }
                }
                SESSION_CLOSED => {
                    if let Some(opened) = id("opened_event_id") {
                        let close = index.closes.entry(opened).or_insert((event, event.id));
                        close.1 = close.1.min(event.id);
                    }
                }
                _ => {}
            }
        }
        index
    }

    /// [`planner_submission`] of `proposal` before `before`.
    fn before(&self, proposal: i64, before: EventId) -> PlannerSubmission {
        let planner_of = |e: &RunEvent| {
            e.payload["planner_id"]
                .as_i64()
                .or_else(|| e.actor.as_ref()?.id.strip_prefix("planner:")?.parse().ok())
        };
        let records: Vec<&RunEvent> = self
            .by_proposal
            .get(&proposal)
            .into_iter()
            .flatten()
            .copied()
            .filter(|e| e.id < before)
            .collect();
        let explicit = records
            .iter()
            // A retry without a planner records the operation, not a new planner submission.
            .filter(|e| e.kind == PROPOSAL_SUBMITTED && e.payload["planner_id"].as_i64().is_some())
            .max_by_key(|e| e.id);
        // A modern submit writes its task_submitted / proposal_resubmitted and
        // then its explicit link in one transaction, before any later review of
        // the proposal starts: those records are the link's, even when it has no
        // planner, and only older ones may infer a submitter. Future links must
        // not change an earlier review.
        let linked = |record: &RunEvent| {
            records
                .iter()
                .filter(|e| e.id > record.id)
                .filter(|e| e.kind == PROPOSAL_SUBMITTED || e.kind == PLAN_REVIEW_STARTED)
                .min_by_key(|e| e.id)
                .is_some_and(|e| e.kind == PROPOSAL_SUBMITTED)
        };
        let legacy = records
            .iter()
            .filter(|e| {
                e.kind == TASK_SUBMITTED
                    || (e.kind == PROPOSAL_RESUBMITTED && planner_of(e).is_some())
            })
            .filter(|e| !linked(e))
            .max_by_key(|e| e.id);
        let Some(&submission) = explicit.into_iter().chain(legacy).max_by_key(|e| e.id) else {
            return PlannerSubmission::default();
        };
        let direct = submission.kind == PROPOSAL_SUBMITTED;
        let mut planner = planner_of(submission);
        if !direct && planner.is_none() {
            // Older revise and draft-planner records predate explicit submission links.
            let drafts = self.drafts.get(&submission.task_id).into_iter().flatten();
            planner = records
                .iter()
                .filter(|e| e.kind == PLAN_REVISE_SENT)
                .chain(drafts)
                .filter(|e| e.id < submission.id)
                .max_by_key(|e| e.id)
                .and_then(|e| e.payload["planner_id"].as_i64());
        }
        let closed_before = |span: &RunEvent| {
            self.closes
                .get(&span.id.as_i64())
                .is_some_and(|(_, first)| *first < submission.id)
        };
        let opened = if direct {
            submission.payload["opened_event_id"]
                .as_i64()
                .and_then(|id| self.spans.get(&id))
                .copied()
                .filter(|e| e.id < submission.id)
        } else {
            match planner {
                Some(planner) => self.spans_of_planner.get(&planner),
                None => self.spans_of_proposal.get(&proposal),
            }
            .into_iter()
            .flatten()
            .copied()
            .filter(|e| e.id < submission.id && !closed_before(e))
            .max_by_key(|e| e.id)
        };
        let closed = opened
            .and_then(|o| self.closes.get(&o.id.as_i64()))
            .map(|(e, _)| *e);
        PlannerSubmission {
            submitted: Some(submission.id),
            planner_id: planner.or_else(|| opened?.payload["planner_id"].as_i64()),
            opened_event_id: opened.map(|e| e.id),
            model: closed
                .and_then(|e| e.payload["model"].as_str())
                .map(str::to_owned),
            effort: closed
                .and_then(|e| e.payload["effort"].as_str())
                .map(str::to_owned),
        }
    }
}

/// What became of a task, over every event.
#[derive(Default)]
struct TaskFate {
    proposal: Option<i64>,
    ready: bool,
    /// Left `draft` without being canceled (task 470's adoption).
    adopted: bool,
    follow_up: bool,
    duplicate_after_ready: bool,
    canceled_after_adoption: bool,
    run: bool,
    reworked: bool,
}

/// Task-caused rework (ADR-0079 decision 1): `integrate`'s verification
/// failed, review raised a concern, or the run was sent back to revise.
/// Conflicts and kills are not the task's.
pub fn rework(event: &RunEvent) -> bool {
    match event.kind.as_str() {
        "integration_deferred" => event.payload["code"] == "verification_failed",
        "review_finished" => event.payload["verdict"] == "concern",
        "revise_requested" => true,
        _ => false,
    }
}

/// The plan quality of the window `after < id <= upto`, per stratum, of
/// the events whose task `counts` accepts. Plan reviews count in the window
/// they finished in; a proposal counts in the window its first task became
/// `ready` in, with the strata of the last plan review that started before
/// that. A proposal readied without a review counts in `all` and the
/// planner strata of its last submission before that.
/// Its tasks' fates are read so far: not cut at the window's end, so a
/// recent window's counts can still grow.
pub fn plan_quality(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> BTreeMap<String, Quality> {
    let mut reviews: BTreeMap<i64, Review> = BTreeMap::new();
    let mut sessions: HashMap<EventId, i64> = HashMap::new();
    let mut tasks: HashMap<TaskId, TaskFate> = HashMap::new();
    let follow_ups: HashSet<i64> = events
        .iter()
        .filter(|e| e.kind == "follow_up_registered")
        .filter_map(|e| e.payload.get("task_id").and_then(Value::as_i64))
        .collect();
    // Proposal → the first time one of its tasks became ready.
    let mut accepted: BTreeMap<i64, EventId> = BTreeMap::new();
    for event in events {
        let id = |key: &str| event.payload.get(key).and_then(Value::as_i64);
        match event.kind.as_str() {
            "plan_review_started" => {
                if let Some(review_id) = id("plan_review_id") {
                    let review = reviews.entry(review_id).or_default();
                    review.proposal = id("proposal_id");
                    review.started = Some(event.id);
                    review.features = event.payload["features"].clone();
                    review.anchor = event.task_id;
                }
            }
            "plan_review_finished" => {
                if let Some(review_id) = id("plan_review_id") {
                    let decision = event.payload["decision"]
                        .as_str()
                        .unwrap_or(UNKNOWN)
                        .to_owned();
                    let review = reviews.entry(review_id).or_default();
                    review.finished = Some((event.id, decision));
                    review.proposal = review.proposal.or(id("proposal_id"));
                    review.anchor = review.anchor.or(event.task_id);
                }
            }
            SESSION_OPENED if event.payload["kind"] == PLAN_REVIEW => {
                if let Some(review_id) = id("plan_review_id") {
                    sessions.insert(event.id, review_id);
                    reviews.entry(review_id).or_default().session = Some(event.id);
                }
            }
            SESSION_CLOSED if event.payload["kind"] == PLAN_REVIEW => {
                let review = id("opened_event_id")
                    .and_then(|opened| sessions.get(&EventId::new(opened)))
                    .and_then(|review_id| reviews.get_mut(review_id));
                if let Some(review) = review {
                    let text = |key: &str| event.payload[key].as_str().map(str::to_owned);
                    review.model = text("model");
                    review.effort = text("effort");
                }
            }
            _ => {}
        }
        let Some(task_id) = event.task_id else {
            continue;
        };
        let fate = tasks.entry(task_id).or_default();
        match event.kind.as_str() {
            "task_submitted" => fate.proposal = id("proposal_id"),
            "task_created" => fate.follow_up = follow_ups.contains(&task_id.as_i64()),
            "task_status_changed" => {
                let from = event.payload["from"].as_str();
                let to = event.payload["to"].as_str();
                if from == Some("draft") && to != Some("canceled") {
                    fate.adopted = true;
                }
                if to == Some("canceled") {
                    if fate.ready
                        && cancel_reason::Recorded::of(&event.payload).pairs_as_duplicate()
                    {
                        fate.duplicate_after_ready = true;
                    }
                    if fate.adopted {
                        fate.canceled_after_adoption = true;
                    }
                }
                if to == Some("ready") && !fate.ready {
                    fate.ready = true;
                    if let Some(proposal) = fate.proposal {
                        accepted.entry(proposal).or_insert(event.id);
                    }
                }
            }
            "task_canceled_as_duplicate" if fate.ready => fate.duplicate_after_ready = true,
            "run_claimed" => fate.run = true,
            _ if rework(event) => fate.reworked = true,
            _ => {}
        }
    }
    let submissions = Submissions::new(events);
    for review in reviews.values_mut() {
        if let (Some(proposal), Some(started)) = (review.proposal, review.started) {
            review.planner = submissions.before(proposal, started);
        }
    }
    let mut quality: BTreeMap<String, Quality> = BTreeMap::new();
    quality.insert(ALL.to_owned(), Quality::default());
    let window = |id: EventId| id > after && id <= upto;
    for review in reviews.values() {
        let Some((finished, decision)) = &review.finished else {
            continue;
        };
        if !window(*finished) || !counts(review.anchor) {
            continue;
        }
        for stratum in review.strata() {
            let entry = quality.entry(stratum).or_default();
            entry.reviews += 1;
            entry.revises += usize::from(decision == "revise");
        }
    }
    for (&proposal, &at) in accepted.iter().filter(|(_, at)| window(**at)) {
        let members: Vec<(&TaskId, &TaskFate)> = tasks
            .iter()
            .filter(|(task, fate)| fate.proposal == Some(proposal) && counts(Some(**task)))
            .collect();
        if members.is_empty() {
            continue;
        }
        // A pass readies the tasks before it records its finish, in one
        // transaction: the judge is the last review started before.
        let unjudged = Review {
            planner: submissions.before(proposal, at),
            ..Review::default()
        };
        let judge = reviews
            .values()
            .filter(|review| review.proposal == Some(proposal))
            .filter(|review| review.finished.is_some())
            .filter(|review| review.started.is_some_and(|id| id < at))
            .max_by_key(|review| review.started)
            .unwrap_or(&unjudged);
        let strata = if std::ptr::eq(judge, &unjudged) {
            std::iter::once(ALL.to_owned())
                .chain(judge.planner.strata())
                .collect()
        } else {
            judge.strata()
        };
        let count = |test: fn(&TaskFate) -> bool| members.iter().filter(|(_, f)| test(f)).count();
        for stratum in strata {
            let entry = quality.entry(stratum).or_default();
            entry.proposals += 1;
            entry.duplicate_cancels_after_ready += count(|f| f.duplicate_after_ready);
            entry.follow_ups += count(|f| f.follow_up && f.adopted);
            entry.follow_ups_canceled_after_adoption +=
                count(|f| f.follow_up && f.canceled_after_adoption);
            entry.tasks_run += count(|f| f.run);
            entry.tasks_reworked += count(|f| f.run && f.reworked);
        }
    }
    quality
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{PlannerOrigin, RunId};

    fn event(id: i64, task: Option<i64>, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: format!("1970-01-01T00:00:{:02}.000Z", id % 60),
            actor: None,
        }
    }

    #[test]
    fn features_name_the_origin_and_the_tier_of_the_closest_task() {
        assert_eq!(origin(true, true, true, PlannerOrigin::Person), FOLLOW_UP);
        assert_eq!(origin(false, true, true, PlannerOrigin::Person), GOAL_GAP);
        assert_eq!(origin(false, false, true, PlannerOrigin::Person), OBSERVER);
        assert_eq!(
            origin(false, false, false, PlannerOrigin::Runtime),
            "runtime"
        );
        assert_eq!(related_tier(None), "low");
        assert_eq!(related_tier(Some(2.9)), "low");
        assert_eq!(related_tier(Some(3.0)), "mid");
        assert_eq!(related_tier(Some(6.0)), "high");
        let features = ProposalFeatures {
            origin: "person".into(),
            follow_up_depth: 0,
            related_score: Some(4.5),
            revise_count: 1,
        };
        assert_eq!(
            features.payload(),
            json!({"origin": "person", "follow_up_depth": 0, "related_score": 4.5,
                   "related": "mid", "revise_count": 1})
        );
    }

    #[test]
    fn submissions_keep_the_last_planners_span_and_unknown_history() {
        let events = vec![
            event(
                1,
                None,
                SESSION_OPENED,
                json!({"kind":"planner", "planner_id":1}),
            ),
            event(2, Some(10), "task_submitted", json!({"proposal_id":7})),
            event(
                3,
                Some(10),
                "proposal_submitted",
                json!({"proposal_id":7,"planner_id":1,"opened_event_id":1}),
            ),
            event(
                4,
                Some(10),
                "plan_review_started",
                json!({"proposal_id":7,"plan_review_id":1}),
            ),
            event(
                5,
                Some(10),
                "plan_review_finished",
                json!({"proposal_id":7,"plan_review_id":1,"decision":"revise"}),
            ),
            event(
                6,
                None,
                SESSION_CLOSED,
                json!({"kind":"planner","opened_event_id":1,"model":"first","effort":"medium"}),
            ),
            event(
                7,
                None,
                SESSION_OPENED,
                json!({"kind":"runtime_planner","planner_id":2,"proposal_id":7}),
            ),
            event(
                8,
                Some(10),
                "proposal_submitted",
                json!({"proposal_id":7,"planner_id":2,"opened_event_id":7}),
            ),
            event(
                9,
                Some(10),
                "plan_review_started",
                json!({"proposal_id":7,"plan_review_id":2}),
            ),
            event(
                10,
                Some(10),
                "task_status_changed",
                json!({"from":"submitted","to":"ready"}),
            ),
            event(
                11,
                Some(10),
                "plan_review_finished",
                json!({"proposal_id":7,"plan_review_id":2,"decision":"pass"}),
            ),
            event(
                12,
                None,
                SESSION_CLOSED,
                json!({"kind":"runtime_planner","opened_event_id":7,"model":"last","effort":"high"}),
            ),
            // A later span and submission do not rewrite the earlier review.
            event(
                13,
                None,
                SESSION_OPENED,
                json!({"kind":"runtime_planner","planner_id":2}),
            ),
            event(
                14,
                Some(10),
                "proposal_submitted",
                json!({"proposal_id":7,"planner_id":2,"opened_event_id":13}),
            ),
        ];
        let last = planner_submission(&events, 7, EventId::new(9));
        assert_eq!(last.planner_id, Some(2));
        assert_eq!(last.opened_event_id, Some(EventId::new(7)));
        assert_eq!(last.model.as_deref(), Some("last"));
        assert_eq!(last.effort.as_deref(), Some("high"));
        let quality = plan_quality(&events, EventId::new(0), EventId::new(20), |_| true);
        assert_eq!(quality["all"].reviews, 2);
        assert_eq!(quality["planner_model=first"].revises, 1);
        assert_eq!(quality["planner_effort=high"].proposals, 1);
        assert_eq!(quality["model=unknown"].reviews, 2);
        assert_eq!(planner_submission(&events, 7, EventId::new(15)).model, None);
        // Old draft and revise links can identify the planner without a new event.
        let legacy = vec![
            event(1, Some(10), "draft_planner_opened", json!({"planner_id":1})),
            event(
                2,
                None,
                SESSION_OPENED,
                json!({"kind":"planner", "planner_id":1}),
            ),
            event(3, Some(10), "task_submitted", json!({"proposal_id":7})),
            event(
                4,
                None,
                SESSION_CLOSED,
                json!({"opened_event_id":2,"model":"first","effort":"medium"}),
            ),
            event(
                5,
                Some(10),
                "plan_revise_sent",
                json!({"proposal_id":7,"planner_id":2}),
            ),
            event(
                6,
                None,
                SESSION_OPENED,
                json!({"kind":"runtime_planner","planner_id":2,"proposal_id":7}),
            ),
            event(7, Some(10), "task_submitted", json!({"proposal_id":7})),
            event(
                8,
                None,
                SESSION_CLOSED,
                json!({"opened_event_id":6,"model":"last","effort":"high"}),
            ),
        ];
        assert_eq!(
            planner_submission(&legacy, 7, EventId::new(4))
                .model
                .as_deref(),
            Some("first")
        );
        assert_eq!(
            planner_submission(&legacy, 7, EventId::new(8))
                .model
                .as_deref(),
            Some("last")
        );
        let old = vec![event(
            1,
            Some(1),
            "task_submitted",
            json!({"proposal_id":1}),
        )];
        assert_eq!(
            planner_submission(&old, 1, EventId::new(2)).opened_event_id,
            None
        );
    }

    #[test]
    fn retries_without_a_planner_keep_attribution_but_a_planner_without_a_span_is_unknown() {
        let mut events = vec![
            event(
                1,
                None,
                SESSION_OPENED,
                json!({"kind":PLANNER,"planner_id":1,"proposal_id":7}),
            ),
            event(2, Some(10), "task_submitted", json!({"proposal_id":7})),
            event(
                3,
                Some(10),
                "proposal_submitted",
                json!({"proposal_id":7,"planner_id":1,"opened_event_id":1}),
            ),
            event(
                4,
                None,
                SESSION_CLOSED,
                json!({"opened_event_id":1,"model":"first","effort":"high"}),
            ),
            // The supervisor and then a person retry unchanged content without a planner.
            event(
                5,
                Some(10),
                "proposal_resubmitted",
                json!({"proposal_id":7}),
            ),
            event(
                6,
                Some(10),
                "proposal_submitted",
                json!({"proposal_id":7,"planner_id":null,"opened_event_id":null}),
            ),
            event(
                7,
                Some(10),
                "proposal_resubmitted",
                json!({"proposal_id":7}),
            ),
            event(
                8,
                Some(10),
                "proposal_submitted",
                json!({"proposal_id":7,"planner_id":null,"opened_event_id":null}),
            ),
        ];
        let submitted = planner_submission(&events, 7, EventId::new(9));
        assert_eq!(submitted.submitted, Some(EventId::new(3)));
        assert_eq!(submitted.opened_event_id, Some(EventId::new(1)));
        assert_eq!(submitted.model.as_deref(), Some("first"));
        assert_eq!(submitted.effort.as_deref(), Some("high"));
        // Old retries without explicit links preserve the original open-at-submit span too.
        let legacy: Vec<_> = events
            .iter()
            .filter(|e| e.kind != "proposal_submitted")
            .cloned()
            .collect();
        let old = planner_submission(&legacy, 7, EventId::new(9));
        assert_eq!(old.submitted, Some(EventId::new(2)));
        assert_eq!(old.model.as_deref(), Some("first"));
        // A real later planner submission supersedes it even when the span is unavailable.
        events.push(event(
            9,
            Some(10),
            "proposal_submitted",
            json!({"proposal_id":7,"planner_id":2,"opened_event_id":null}),
        ));
        let missing = planner_submission(&events, 7, EventId::new(10));
        assert_eq!(missing.planner_id, Some(2));
        assert_eq!(missing.opened_event_id, None);
        assert_eq!(missing.model, None);
        assert_eq!(missing.effort, None);
        let no_planner = planner_submission(&events[4..8], 7, EventId::new(9));
        assert_eq!(no_planner, PlannerSubmission::default());
    }

    #[test]
    fn a_modern_resubmission_without_a_planner_never_uses_legacy_inference() {
        let mut events = vec![
            event(
                1,
                None,
                SESSION_OPENED,
                json!({"kind":PLANNER,"planner_id":1}),
            ),
            event(2, Some(10), "task_submitted", json!({"proposal_id":7})),
            event(
                3,
                Some(10),
                "proposal_submitted",
                json!({"proposal_id":7,"planner_id":1,"opened_event_id":1}),
            ),
            event(
                4,
                None,
                SESSION_CLOSED,
                json!({"opened_event_id":1,"model":"original","effort":"high"}),
            ),
            event(
                5,
                Some(10),
                "plan_revise_sent",
                json!({"proposal_id":7,"planner_id":2}),
            ),
            event(
                6,
                None,
                SESSION_OPENED,
                json!({"kind":RUNTIME_PLANNER,"planner_id":2,"proposal_id":7}),
            ),
            // A person moves new draft tasks, with no planner in the submitting workspace.
            event(7, Some(10), "task_submitted", json!({"proposal_id":7})),
            event(
                8,
                Some(10),
                "proposal_submitted",
                json!({"proposal_id":7,"planner_id":null,"opened_event_id":null}),
            ),
            event(
                9,
                None,
                SESSION_CLOSED,
                json!({"opened_event_id":6,"model":"reviser","effort":"xhigh"}),
            ),
        ];
        let before = EventId::new(10);
        let original = planner_submission(&events, 7, EventId::new(5));
        assert_eq!(original.submitted, Some(EventId::new(3)));
        assert_eq!(original.model.as_deref(), Some("original"));
        assert_eq!(original.effort.as_deref(), Some("high"));
        assert_eq!(planner_submission(&events, 7, before), original);
        // A draft-planner link must not infer a different submitter either.
        events[4].kind = "draft_planner_opened".into();
        assert_eq!(planner_submission(&events, 7, before), original);
        // An explicit operation also supersedes legacy retry records with a named actor.
        events[6].kind = "proposal_resubmitted".into();
        events[6].actor = Some(crate::domain::EventActor {
            role: "planner".into(),
            id: "planner:2".into(),
            requested_by: None,
        });
        assert_eq!(planner_submission(&events, 7, before), original);
        events[6].actor = None;
        events[6].kind = "task_submitted".into();
        // No earlier planner submission means unknown, even with an inferable reviser.
        assert_eq!(
            planner_submission(&events[4..], 7, before),
            PlannerSubmission::default()
        );
        // A link for another proposal must not suppress a historical submission.
        events[7].payload["proposal_id"] = json!(8);
        assert_eq!(
            planner_submission(&events, 7, before).model.as_deref(),
            Some("reviser")
        );
        // Nor may a future link change a review that had already started.
        events[7] = event(
            11,
            Some(10),
            "proposal_submitted",
            json!({"proposal_id":7,"planner_id":null,"opened_event_id":null}),
        );
        events.swap(7, 8);
        assert_eq!(
            planner_submission(&events, 7, before).model.as_deref(),
            Some("reviser")
        );
        // Without explicit records, the same old submission still uses legacy inference.
        events.retain(|e| e.kind != "proposal_submitted");
        assert_eq!(
            planner_submission(&events, 7, before).model.as_deref(),
            Some("reviser")
        );
    }

    #[test]
    fn a_retry_without_a_planner_keeps_the_planner_of_an_older_submission() {
        let mut submitted = event(2, Some(10), "task_submitted", json!({"proposal_id":7}));
        submitted.actor = Some(crate::domain::EventActor {
            role: "planner".into(),
            id: "planner:3".into(),
            requested_by: None,
        });
        // An older binary submitted it; its review stopped and a newer one retries it.
        let mut events = vec![
            event(
                1,
                None,
                SESSION_OPENED,
                json!({"kind":PLANNER,"planner_id":3}),
            ),
            submitted,
            event(
                3,
                Some(10),
                "plan_review_started",
                json!({"proposal_id":7,"plan_review_id":1}),
            ),
            event(
                4,
                None,
                SESSION_CLOSED,
                json!({"opened_event_id":1,"model":"older","effort":"medium"}),
            ),
            event(
                5,
                Some(10),
                "proposal_resubmitted",
                json!({"proposal_id":7}),
            ),
            event(
                6,
                Some(10),
                "proposal_submitted",
                json!({"proposal_id":7,"planner_id":null,"opened_event_id":null}),
            ),
        ];
        let retried = planner_submission(&events, 7, EventId::new(7));
        assert_eq!(retried.submitted, Some(EventId::new(2)));
        assert_eq!(retried.planner_id, Some(3));
        assert_eq!(retried.opened_event_id, Some(EventId::new(1)));
        assert_eq!(retried.model.as_deref(), Some("older"));
        assert_eq!(planner_submission(&events, 7, EventId::new(3)), retried);
        // With no review between them, the records are the link's own.
        events.remove(2);
        assert_eq!(
            planner_submission(&events, 7, EventId::new(7)),
            PlannerSubmission::default()
        );
    }

    #[test]
    fn legacy_submissions_use_the_actor_or_proposal_and_skip_closed_spans() {
        let mut events = vec![
            event(
                1,
                None,
                SESSION_OPENED,
                json!({"kind":"runtime_planner", "planner_id":2, "proposal_id":7}),
            ),
            event(
                2,
                None,
                SESSION_CLOSED,
                json!({"opened_event_id":1,"model":"old","effort":"medium"}),
            ),
            event(
                3,
                None,
                SESSION_OPENED,
                json!({"kind":"runtime_planner", "planner_id":2, "proposal_id":7,"route":"headless"}),
            ),
            event(4, Some(10), "task_submitted", json!({"proposal_id":7})),
            event(
                5,
                None,
                SESSION_CLOSED,
                json!({"opened_event_id":3,"model":"last","effort":"high"}),
            ),
        ];
        let before = EventId::new(5);
        let by_proposal = planner_submission(&events, 7, before);
        assert_eq!(by_proposal.planner_id, Some(2));
        assert_eq!(by_proposal.opened_event_id, Some(EventId::new(3)));
        assert_eq!(by_proposal.model.as_deref(), Some("last"));
        events[3].actor = Some(crate::domain::EventActor {
            role: "planner".into(),
            id: "planner:2".into(),
            requested_by: None,
        });
        // Actor attribution works even if the span opened before it had a proposal.
        events[2]
            .payload
            .as_object_mut()
            .unwrap()
            .remove("proposal_id");
        assert_eq!(planner_submission(&events, 7, before), by_proposal);
        // A legacy retry by a named planner still counts as that planner's submission.
        events[3].kind = "proposal_resubmitted".into();
        assert_eq!(planner_submission(&events, 7, before), by_proposal);
        events[4].payload.as_object_mut().unwrap().remove("model");
        assert_eq!(planner_submission(&events, 7, before).model, None);
        // With only a span closed before submission, do not infer its model.
        events.remove(2);
        assert_eq!(planner_submission(&events, 7, before).opened_event_id, None);
    }

    /// Two proposals: 1 was sent back once by a medium review and then
    /// passed by a high one; its tasks 10 (a follow-up canceled later as a
    /// duplicate) and 11 (reworked) are judged by the high review. 2 was
    /// readied without a review: `all` and unknown planner strata.
    #[test]
    fn the_quality_of_a_window_is_split_by_the_judging_session_and_the_features() {
        let features = |revise_count: i64| {
            json!({"origin": "follow_up", "follow_up_depth": 1, "related_score": 7.0,
                   "related": "high", "revise_count": revise_count})
        };
        let review = |id: i64, review: i64, f: Value| {
            event(
                id,
                Some(10),
                "plan_review_started",
                json!({"proposal_id": 1, "plan_review_id": review, "features": f}),
            )
        };
        let opened = |id: i64, review: i64| {
            event(
                id,
                Some(10),
                SESSION_OPENED,
                json!({"kind": PLAN_REVIEW, "plan_review_id": review}),
            )
        };
        let closed = |id: i64, opened: i64, effort: &str| {
            event(
                id,
                Some(10),
                SESSION_CLOSED,
                json!({"kind": PLAN_REVIEW, "opened_event_id": opened,
                       "model": "claude-opus-5-5", "effort": effort}),
            )
        };
        let finished = |id: i64, review: i64, decision: &str| {
            event(
                id,
                Some(10),
                "plan_review_finished",
                json!({"proposal_id": 1, "plan_review_id": review, "decision": decision}),
            )
        };
        let changed = |id: i64, task: i64, from: &str, to: &str| {
            event(
                id,
                Some(task),
                "task_status_changed",
                json!({"from": from, "to": to}),
            )
        };
        let submitted = |id: i64, task: i64, proposal: i64| {
            event(
                id,
                Some(task),
                "task_submitted",
                json!({"proposal_id": proposal}),
            )
        };
        let run_event = |id: i64, task: i64, kind: &str, payload: Value| {
            let mut e = event(id, Some(task), kind, payload);
            e.run_id = Some(RunId::new(format!("run-{task}")).unwrap());
            e
        };
        let events = vec![
            event(1, Some(9), "follow_up_registered", json!({"task_id": 10})),
            event(2, Some(10), "task_created", json!({})),
            event(3, Some(11), "task_created", json!({})),
            changed(4, 10, "draft", "submitted"),
            submitted(5, 10, 1),
            changed(6, 11, "draft", "submitted"),
            submitted(7, 11, 1),
            review(8, 1, features(0)),
            opened(9, 1),
            finished(10, 1, "revise"),
            closed(11, 9, "medium"),
            // The window starts here.
            review(20, 2, features(1)),
            opened(21, 2),
            // A pass readies the tasks before it records its finish.
            changed(22, 10, "submitted", "ready"),
            changed(23, 11, "submitted", "ready"),
            finished(24, 2, "pass"),
            closed(25, 21, "high"),
            run_event(26, 11, "run_claimed", json!({})),
            run_event(
                27,
                11,
                "integration_deferred",
                json!({"code": "rebase_conflict"}),
            ),
            run_event(
                28,
                11,
                "integration_deferred",
                json!({"code": "verification_failed"}),
            ),
            run_event(29, 10, "run_claimed", json!({})),
            run_event(30, 10, "review_finished", json!({"verdict": "pass"})),
            event(
                31,
                Some(10),
                "task_status_changed",
                json!({"from": "ready", "to": "canceled", "duplicate_of": 3}),
            ),
            // Proposal 2, readied past no review.
            submitted(32, 12, 2),
            changed(33, 12, "submitted", "ready"),
            run_event(34, 12, "run_claimed", json!({})),
            run_event(35, 12, "revise_requested", json!({})),
        ];
        let quality = plan_quality(&events, EventId::new(19), EventId::new(40), |_| true);
        let expected_high = Quality {
            reviews: 1,
            revises: 0,
            proposals: 1,
            duplicate_cancels_after_ready: 1,
            follow_ups: 1,
            follow_ups_canceled_after_adoption: 1,
            tasks_run: 2,
            tasks_reworked: 1,
        };
        assert_eq!(quality["effort=high"], expected_high);
        assert_eq!(quality["model=claude-opus-5-5"], expected_high);
        assert_eq!(quality["origin=follow_up"], expected_high);
        assert_eq!(quality["related=high"], expected_high);
        assert_eq!(quality["revise_count=1"], expected_high);
        assert_eq!(quality["follow_up_depth=1"], expected_high);
        assert_eq!(
            quality["all"],
            Quality {
                proposals: 2,
                tasks_run: 3,
                tasks_reworked: 2,
                ..expected_high
            }
        );
        assert!(!quality.contains_key("effort=medium"));
        // No proposal has a planner span: both, reviewed or not, are unknown.
        assert_eq!(quality["planner_model=unknown"], quality["all"]);
        assert_eq!(quality["planner_effort=unknown"], quality["all"]);
        // The earlier window has the revise of the medium review only.
        let earlier = plan_quality(&events, EventId::new(0), EventId::new(19), |_| true);
        assert_eq!(
            earlier["effort=medium"],
            Quality {
                reviews: 1,
                revises: 1,
                ..Quality::default()
            }
        );
        assert_eq!(earlier["revise_count=0"].revises, 1);
        // A goal's tasks only.
        let none = plan_quality(&events, EventId::new(0), EventId::new(40), |_| false);
        assert_eq!(none.len(), 1);
        assert_eq!(none["all"], Quality::default());
    }
}
