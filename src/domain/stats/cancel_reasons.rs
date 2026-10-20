//! The tasks canceled in a window by the reason their cancel recorded
//! (ADR-t947-4 decision 4): who canceled them, how long after they were
//! registered, how far the plan had taken them (submitted, ready, the plan
//! reviews and the runtime's planners), the runs they used and for how
//! long, and the category of the follow_ups among them (ADR-t947-3). A
//! cancel recorded before reasons is read as
//! [`cancel_reason::Recorded::reason`] reads it, by whether the task it
//! names was completed by then, and one that names none is
//! [`cancel_reason::UNRECORDED`]. The task's history before the window is
//! read too, so a task registered or run before the window counts what it
//! used. The events are of planning and execution, named by
//! [`crate::domain::event_kind`] and read as typed values (C6 of
//! docs/design/architecture.md). Derived from `run_events`; no table of
//! its own.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

use super::{EventId, RunEvent, Summary, TaskId, summary, timestamp_millis};
use crate::domain::run::payload::restore;
use crate::domain::{
    EventKind, ProposalId, RunStatus, TaskStatus, UNLABELED_CATEGORY, cancel_reason,
};

/// The cancels of one reason in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CancelReason {
    /// Tasks canceled in the window with this reason.
    pub count: i64,
    /// `count` by the role of the event's actor (`unrecorded` for a cancel
    /// written before the queue recorded actors).
    pub by_actor: BTreeMap<String, i64>,
    /// From the task's `task_created` to its cancel, in seconds.
    pub registered_secs: Summary,
    /// The seconds `registered_secs` sums, for `kpi`'s spread.
    #[serde(skip)]
    pub registered_secs_values: Vec<i64>,
    /// Canceled after it was submitted.
    pub after_submitted: i64,
    /// Canceled after it was ready.
    pub after_ready: i64,
    /// Canceled after the plan took it on: submitted, ready, or a runtime
    /// planner opened for it (the plan's waste).
    pub plan_waste: i64,
    /// The plan reviews of the proposals it was submitted in while it was
    /// in them, before the cancel.
    pub plan_reviews: i64,
    /// The runtime's planners opened for it before the cancel.
    pub runtime_planners: i64,
    /// The runs claimed for it before the cancel.
    pub runs: i64,
    /// Canceled after a run of it was claimed.
    pub run_waste: i64,
    /// The seconds of those runs: each from its claim to its last end
    /// before the cancel, or to the cancel for one not ended.
    pub run_waste_secs: i64,
    /// `count` of the follow_up drafts among them by the category their
    /// worker gave (`unlabeled` without one); other tasks are not counted.
    pub by_follow_up_category: BTreeMap<String, i64>,
}

/// What a task went through before its cancel.
#[derive(Default)]
struct History {
    created: Option<i64>,
    submitted: bool,
    ready: bool,
    /// The proposal it is in since its latest submit, with how many plan
    /// reviews that proposal had started by then; none once it went back
    /// to `draft`.
    proposal: Option<(ProposalId, i64)>,
    /// The plan reviews of the proposals it was in before.
    plan_reviews: i64,
    runtime_planners: i64,
    runs: HashMap<String, RunSpan>,
    category: Option<String>,
}

/// A run's claim and, while it rests in a status that ended it, when it
/// ended last (unix milliseconds).
#[derive(Default)]
struct RunSpan {
    claimed: Option<i64>,
    ended: Option<i64>,
}

/// The cancels (`task_status_changed` to `canceled`) with
/// `after < id <= upto` whose task `counts` accepts, by reason, each with
/// what its task went through from its first event.
pub fn cancel_reasons(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> BTreeMap<String, CancelReason> {
    let mut tasks: HashMap<TaskId, History> = HashMap::new();
    let mut completed: HashSet<TaskId> = HashSet::new();
    // The plan reviews started per proposal.
    let mut reviews: HashMap<ProposalId, i64> = HashMap::new();
    let mut reasons: BTreeMap<String, CancelReason> = BTreeMap::new();
    for event in events.iter().filter(|event| event.id <= upto) {
        let at = timestamp_millis(&event.created_at);
        match event.kind.known() {
            // The follow_up's event is its source run's; the draft is named.
            Some(EventKind::FollowUpRegistered) => {
                let registered: restored::FollowUpRegistered = restore(&event.payload);
                if let Some(task_id) = registered.task_id {
                    tasks.entry(task_id).or_default().category = Some(
                        registered
                            .category
                            .unwrap_or_else(|| UNLABELED_CATEGORY.to_owned()),
                    );
                }
                continue;
            }
            // The plan review's event is on the proposal's first task.
            Some(EventKind::PlanReviewStarted) => {
                let started: restored::OfProposal = restore(&event.payload);
                if let Some(proposal) = started.proposal_id {
                    *reviews.entry(proposal).or_default() += 1;
                }
                continue;
            }
            _ => {}
        }
        let Some(task_id) = event.task_id else {
            continue;
        };
        let history = tasks.entry(task_id).or_default();
        if let Some(run_id) = &event.run_id {
            let span = history.runs.entry(run_id.to_string()).or_default();
            if event.kind == EventKind::RunClaimed {
                span.claimed = span.claimed.or(at);
            }
            // A run's status ends it, or opens it again (a resume).
            let status = if event.kind == EventKind::RunIntegrated {
                Some(RunStatus::Integrated)
            } else {
                restore::<restored::RunStatusOf>(&event.payload).status
            };
            if let Some(status) = status {
                span.ended = (!RunStatus::is_unfinished(status)).then_some(at).flatten();
            }
        }
        match event.kind.known() {
            Some(EventKind::TaskCreated) => history.created = at,
            // A submit, or a reopen into a proposal of its own (which
            // records no `task_submitted`), moves it into a proposal.
            Some(EventKind::TaskSubmitted | EventKind::TaskReopened) => {
                let joined: restored::OfProposal = restore(&event.payload);
                history.submitted = true;
                history.leave(&reviews);
                history.proposal = joined
                    .proposal_id
                    .map(|proposal| (proposal, reviews.get(&proposal).copied().unwrap_or(0)));
            }
            Some(EventKind::DraftPlannerOpened) => history.runtime_planners += 1,
            Some(EventKind::TaskStatusChanged) => {
                let changed = cancel_reason::Recorded::of(&event.payload);
                match changed.to {
                    Some(TaskStatus::Submitted) => history.submitted = true,
                    Some(TaskStatus::Ready) => history.ready = true,
                    // Back to `draft`, it is in its proposal no more: a
                    // revise submits it again, as a withdraw or `draft` do
                    // not.
                    Some(TaskStatus::Draft) => history.leave(&reviews),
                    Some(TaskStatus::Completed) => {
                        completed.insert(task_id);
                    }
                    Some(TaskStatus::Canceled) if event.id > after && counts(Some(task_id)) => {
                        let target_completed = changed
                            .duplicate_of
                            .is_some_and(|target| completed.contains(&target));
                        let reason = changed
                            .reason(target_completed)
                            .unwrap_or_else(|| cancel_reason::UNRECORDED.to_owned());
                        let actor = event
                            .actor
                            .as_ref()
                            .map_or(cancel_reason::UNRECORDED, |actor| actor.role.as_str());
                        let plan_reviews = history.reviews(&reviews);
                        count(
                            reasons.entry(reason).or_default(),
                            history,
                            plan_reviews,
                            actor,
                            at,
                        );
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    for reason in reasons.values_mut() {
        reason.registered_secs = summary(reason.registered_secs_values.iter().copied().map(Some));
    }
    reasons
}

impl History {
    /// The plan reviews of its proposals so far, given the reviews started
    /// per proposal.
    fn reviews(&self, reviews: &HashMap<ProposalId, i64>) -> i64 {
        self.plan_reviews
            + self.proposal.map_or(0, |(proposal, before)| {
                reviews.get(&proposal).copied().unwrap_or(0) - before
            })
    }

    /// Leave its proposal, keeping the reviews it had while in it.
    fn leave(&mut self, reviews: &HashMap<ProposalId, i64>) {
        self.plan_reviews = self.reviews(reviews);
        self.proposal = None;
    }
}

/// The payloads [`cancel_reasons`] reads of planning's and execution's
/// events, restored as typed values with `run::payload::restore`: a missing
/// key, `null` or a value of another type (an older or broken writer)
/// reads as absent, so no recorded event is refused. A cancel's own fields
/// are [`cancel_reason::Recorded`].
mod restored {
    use serde::Deserialize;

    use crate::domain::run::payload::typed;
    use crate::domain::{ProposalId, RunStatus, TaskId};

    /// `follow_up_registered`: the draft it registered and its category.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct FollowUpRegistered {
        #[serde(default, deserialize_with = "typed")]
        pub(super) task_id: Option<TaskId>,
        #[serde(default, deserialize_with = "typed")]
        pub(super) category: Option<String>,
    }

    /// `task_submitted`, `task_reopened` and `plan_review_started`: the
    /// proposal.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct OfProposal {
        #[serde(default, deserialize_with = "typed")]
        pub(super) proposal_id: Option<ProposalId>,
    }

    /// A run's event that records the status it moved to.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct RunStatusOf {
        #[serde(default, deserialize_with = "typed")]
        pub(super) status: Option<RunStatus>,
    }
}

/// Add to `reason` the cancel by `actor` at `at` of the task of `history`,
/// after `plan_reviews` reviews of its proposals.
fn count(
    reason: &mut CancelReason,
    history: &History,
    plan_reviews: i64,
    actor: &str,
    at: Option<i64>,
) {
    reason.count += 1;
    *reason.by_actor.entry(actor.to_owned()).or_default() += 1;
    if let (Some(created), Some(at)) = (history.created, at) {
        reason
            .registered_secs_values
            .push((at - created).max(0) / 1000);
    }
    reason.after_submitted += i64::from(history.submitted);
    reason.after_ready += i64::from(history.ready);
    reason.plan_waste +=
        i64::from(history.submitted || history.ready || history.runtime_planners > 0);
    reason.plan_reviews += plan_reviews;
    reason.runtime_planners += history.runtime_planners;
    let claimed: Vec<&RunSpan> = history
        .runs
        .values()
        .filter(|span| span.claimed.is_some())
        .collect();
    reason.runs += i64::try_from(claimed.len()).unwrap_or(i64::MAX);
    reason.run_waste += i64::from(!claimed.is_empty());
    if let Some(at) = at {
        reason.run_waste_secs += claimed
            .iter()
            .filter_map(|span| {
                let claimed = span.claimed?;
                Some((span.ended.unwrap_or(at).min(at) - claimed).max(0) / 1000)
            })
            .sum::<i64>();
    }
    if let Some(category) = &history.category {
        *reason
            .by_follow_up_category
            .entry(category.clone())
            .or_default() += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::event_kind::PLAN_REVIEW_STARTED;
    use crate::domain::event_kind::TASK_CREATED;
    use crate::domain::event_kind::TASK_REOPENED;
    use crate::domain::event_kind::TASK_STATUS_CHANGED;
    use crate::domain::event_kind::TASK_SUBMITTED;
    use crate::domain::{EventActor, RunId};
    use serde_json::{Value, json};

    fn event(id: i64, task: i64, kind: &str, payload: Value, second: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task)),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: crate::domain::marks::utc_text(second * 1000),
            actor: None,
        }
    }

    fn on(mut event: RunEvent, run: &RunId) -> RunEvent {
        event.run_id = Some(run.clone());
        event
    }

    fn by(mut event: RunEvent, role: &str) -> RunEvent {
        event.actor = Some(EventActor {
            role: role.into(),
            id: format!("{role}:1"),
            requested_by: None,
        });
        event
    }

    fn canceled(id: i64, task: i64, payload: Value, second: i64) -> RunEvent {
        let mut payload = payload;
        payload["to"] = json!("canceled");
        event(id, task, "task_status_changed", payload, second)
    }

    /// Four cancels: one absorbed after it went through the plan and a
    /// run, one recorded before reasons that names a task completed by
    /// then, one before reasons that names none, and one of a follow_up a
    /// runtime planner dropped; each by its reason with what it used.
    #[test]
    fn groups_the_cancels_by_reason_with_what_each_task_used() {
        let run = RunId::new("11111111-1111-4111-8111-111111111111").unwrap();
        let events = vec![
            event(1, 1, "task_created", json!({}), 10),
            event(2, 2, "task_created", json!({}), 11),
            event(3, 3, "task_created", json!({}), 12),
            event(4, 4, "task_created", json!({}), 13),
            event(
                5,
                9,
                "follow_up_registered",
                json!({"task_id": 4, "category": "defect"}),
                13,
            ),
            event(6, 1, "task_submitted", json!({"proposal_id": 7}), 20),
            event(7, 1, "plan_review_started", json!({"proposal_id": 7}), 20),
            event(8, 1, "task_status_changed", json!({"to": "ready"}), 21),
            on(event(9, 1, "run_claimed", json!({}), 23), &run),
            event(10, 2, "task_status_changed", json!({"to": "completed"}), 24),
            event(11, 4, "draft_planner_opened", json!({}), 25),
            by(
                canceled(12, 1, json!({"reason": "absorbed", "duplicate_of": 2}), 30),
                "planner",
            ),
            canceled(13, 3, json!({"duplicate_of": 2}), 32),
            canceled(14, 5, json!({}), 33),
            by(
                canceled(15, 4, json!({"reason": "not_worth"}), 35),
                "planner",
            ),
        ];
        let all = cancel_reasons(&events, EventId::new(0), EventId::new(15), |_| true);
        let absorbed = &all["absorbed"];
        assert_eq!(absorbed.count, 1);
        assert_eq!(absorbed.by_actor["planner"], 1);
        assert_eq!(absorbed.registered_secs.total, 20);
        assert_eq!(
            (
                absorbed.after_submitted,
                absorbed.after_ready,
                absorbed.plan_waste,
                absorbed.plan_reviews
            ),
            (1, 1, 1, 1)
        );
        // Claimed at 23 and not ended: up to the cancel at 30.
        assert_eq!(
            (absorbed.runs, absorbed.run_waste, absorbed.run_waste_secs),
            (1, 1, 7)
        );
        assert_eq!(all["already_done"].count, 1);
        assert_eq!(all["already_done"].by_actor["unrecorded"], 1);
        assert_eq!(all["already_done"].plan_waste, 0);
        assert_eq!(all["unrecorded"].count, 1);
        let dropped = &all["not_worth"];
        assert_eq!((dropped.plan_waste, dropped.runtime_planners), (1, 1));
        assert_eq!(dropped.run_waste, 0);
        assert_eq!(dropped.by_follow_up_category["defect"], 1);
        // A window after the first cancel counts the later ones only, with
        // what their tasks used before it.
        let later = cancel_reasons(&events, EventId::new(12), EventId::new(15), |_| true);
        assert!(!later.contains_key("absorbed"));
        assert_eq!(later["not_worth"].runtime_planners, 1);
        let one = cancel_reasons(&events, EventId::new(0), EventId::new(15), |task| {
            task == Some(TaskId::new(3))
        });
        assert_eq!(one.keys().collect::<Vec<_>>(), ["already_done"]);
    }

    /// A task counts the plan reviews of each proposal it was in while it
    /// was in it: not those before its submit, nor those after it left.
    #[test]
    fn a_task_counts_the_reviews_of_its_proposals_while_it_was_in_them() {
        let review = |id: i64, proposal: i64| {
            event(
                id,
                9,
                "plan_review_started",
                json!({"proposal_id": proposal}),
                id,
            )
        };
        let submitted = |id: i64, proposal: i64| {
            event(
                id,
                1,
                "task_submitted",
                json!({"proposal_id": proposal}),
                id,
            )
        };
        let events = vec![
            event(1, 1, "task_created", json!({}), 1),
            review(2, 7),
            submitted(3, 7),
            review(4, 7),
            review(5, 7),
            submitted(6, 8),
            review(7, 7),
            review(8, 8),
            canceled(9, 1, json!({"reason": "superseded", "note": "x"}), 9),
            review(10, 8),
        ];
        let rows = cancel_reasons(&events, EventId::new(0), EventId::new(10), |_| true);
        assert_eq!(rows["superseded"].plan_reviews, 3);
    }

    /// A task taken back to `draft` leaves its proposal: the proposal's
    /// reviews after that are not its own, and a submit again (a revise's)
    /// counts the reviews from then on; a reopen moves it into the reopened
    /// proposal, whose reviews count.
    #[test]
    fn a_task_taken_back_to_draft_counts_no_review_of_its_proposal_after() {
        let review = |id: i64| event(id, 9, PLAN_REVIEW_STARTED, json!({"proposal_id": 7}), id);
        let changed = |id: i64, task: i64, to: &str| {
            event(id, task, TASK_STATUS_CHANGED, json!({"to": to}), id)
        };
        let submitted =
            |id: i64, task: i64| event(id, task, TASK_SUBMITTED, json!({"proposal_id": 7}), id);
        let events = vec![
            event(1, 1, TASK_CREATED, json!({}), 1),
            event(2, 2, TASK_CREATED, json!({}), 2),
            submitted(3, 1),
            submitted(4, 2),
            review(5),
            changed(6, 1, "draft"),
            review(7),
            review(8),
            canceled(9, 1, json!({"reason": "not_worth"}), 9),
            // Back to draft by a revise, a review of the proposal while it
            // is out, and submitted again.
            changed(10, 2, "draft"),
            review(11),
            submitted(12, 2),
            review(13),
            canceled(14, 2, json!({"reason": "stale"}), 14),
            // Approved, then reopened into a proposal of its own (8),
            // whose reviews are its own.
            event(15, 3, TASK_CREATED, json!({}), 15),
            submitted(16, 3),
            review(17),
            changed(18, 3, "ready"),
            event(
                19,
                3,
                TASK_REOPENED,
                json!({"proposal_id": 8, "reviewed_proposal_id": 9}),
                19,
            ),
            event(20, 9, PLAN_REVIEW_STARTED, json!({"proposal_id": 8}), 20),
            event(21, 9, PLAN_REVIEW_STARTED, json!({"proposal_id": 8}), 21),
            review(22),
            canceled(23, 3, json!({"reason": "decision_moot"}), 23),
        ];
        let rows = cancel_reasons(&events, EventId::new(0), EventId::new(23), |_| true);
        assert_eq!(rows["not_worth"].plan_reviews, 1);
        assert_eq!(rows["stale"].plan_reviews, 4);
        assert_eq!(rows["decision_moot"].plan_reviews, 3);
    }

    /// A run counts from its claim to its last end before the cancel: a
    /// failed run resumed and failed again ends at its second failure, and
    /// a run claimed after it until the cancel.
    #[test]
    fn a_run_counts_from_its_claim_to_its_last_end_before_the_cancel() {
        let first = RunId::new("11111111-1111-4111-8111-111111111111").unwrap();
        let second = RunId::new("22222222-2222-4222-8222-222222222222").unwrap();
        let events = vec![
            event(1, 1, "task_created", json!({}), 0),
            on(event(2, 1, "run_claimed", json!({}), 10), &first),
            on(
                event(3, 1, "recovery_finished", json!({"status": "failed"}), 20),
                &first,
            ),
            on(
                event(
                    4,
                    1,
                    "resume_started",
                    json!({"status": "needs_session"}),
                    25,
                ),
                &first,
            ),
            on(
                event(5, 1, "recovery_finished", json!({"status": "failed"}), 40),
                &first,
            ),
            on(event(6, 1, "run_claimed", json!({}), 50), &second),
            canceled(7, 1, json!({"reason": "stale"}), 80),
        ];
        let stale = &cancel_reasons(&events, EventId::new(0), EventId::new(7), |_| true)["stale"];
        assert_eq!((stale.runs, stale.run_waste), (2, 1));
        assert_eq!(stale.run_waste_secs, 30 + 30);
        assert_eq!(stale.registered_secs.total, 80);
    }
}
