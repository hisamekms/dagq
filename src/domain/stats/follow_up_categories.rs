//! The follow_up drafts of a window by the category their worker gave them
//! (ADR-t947-3): how many were registered, how they left `draft` (adopted,
//! canceled, canceled as a duplicate), what the runtime's planners decided
//! of them (`draft_planner_settled`, `draft_planner_exhausted`, their
//! `planner_question` asks and the answers), how many of them landed, and
//! how long they stayed drafts. Derived from `run_events` like
//! [`super::drafts`]: a draft's category is the one its
//! `follow_up_registered` recorded, `unlabeled` for one registered before
//! categories were kept, and a value outside the list is a row of its own.
use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::Value;

use super::{EventId, RunEvent, Summary, TaskId, summary, timestamp_millis};
use crate::domain::UNLABELED_CATEGORY;

/// The follow_up drafts of one category in a window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CategoryFlow {
    /// Registered in the window (their `task_created`).
    pub registered: i64,
    /// Left `draft` for the first time in the window to anything but
    /// `canceled`, as `draft_flow` counts `adopted`.
    pub adopted: i64,
    /// Left `draft` for the first time in the window by a cancel that
    /// named no task it duplicates.
    pub canceled: i64,
    /// Left `draft` for the first time in the window by a cancel with
    /// `duplicate_of`.
    pub duplicate: i64,
    /// `adopted` ÷ (`adopted` + `canceled` + `duplicate`), to two decimals;
    /// null when none left.
    pub adoption_rate: Option<f64>,
    /// `duplicate` ÷ the same.
    pub duplicate_rate: Option<f64>,
    /// What the runtime's planners decided of the drafts in the window,
    /// by `draft_planner_settled`'s `outcome` (a
    /// [`crate::domain::DraftOutcome`]): a draft's last one in it.
    pub planner_outcomes: BTreeMap<String, i64>,
    /// `draft_planner_exhausted` in the window: drafts no more planners
    /// were opened for.
    pub exhausted: i64,
    /// `planner_question` asks opened about the drafts in the window.
    pub planner_questions: i64,
    /// The answers to those asks in the window, by option (`free` for one
    /// that chose none); not the asks the runtime closed itself
    /// (`runtime_closed`).
    pub answers: BTreeMap<String, i64>,
    /// Runs of the drafts landed in the window (`run_integrated`).
    pub landed: i64,
    /// From `task_created` to leaving `draft` for the first time, of the
    /// drafts that left it in the window, in seconds.
    pub draft_secs: Summary,
    /// The seconds `draft_secs` sums, for `kpi`'s spread.
    #[serde(skip)]
    pub draft_secs_values: Vec<i64>,
    /// Still `draft` at the window's end.
    pub backlog: i64,
    /// How long the oldest of `backlog` had waited at the window's end,
    /// in seconds.
    pub oldest_backlog_secs: Option<i64>,
}

/// One follow_up draft as `run_events` shows it.
#[derive(Default)]
struct Draft {
    category: String,
    created: Option<(EventId, i64)>,
    /// Its first `task_status_changed` away from `draft`: the event, its
    /// time, and where it went (`Some(duplicate)` for a cancel).
    left: Option<(EventId, i64, Option<bool>)>,
    draft_at_end: bool,
    /// Its last `draft_planner_settled` in the window.
    outcome: Option<String>,
}

/// `numerator` ÷ `denominator` to two decimals; `None` for a zero
/// denominator.
fn ratio(numerator: i64, denominator: i64) -> Option<f64> {
    (denominator > 0).then(|| (numerator as f64 * 100.0 / denominator as f64).round() / 100.0)
}

/// Count the follow_up drafts (the tasks a `follow_up_registered` names)
/// with `after < id <= upto` whose task `counts` accepts, by category; the
/// backlog is the drafts still `draft` at `upto`, aged to `end` (unix
/// milliseconds).
pub fn follow_up_categories(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    end: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> BTreeMap<String, CategoryFlow> {
    let mut drafts: HashMap<TaskId, Draft> = HashMap::new();
    for event in events.iter().filter(|e| e.kind == "follow_up_registered") {
        if let Some(task_id) = event.payload.get("task_id").and_then(Value::as_i64) {
            let category = event
                .payload
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or(UNLABELED_CATEGORY);
            drafts.entry(TaskId::new(task_id)).or_default().category = category.to_owned();
        }
    }
    let mut flows: BTreeMap<String, CategoryFlow> = BTreeMap::new();
    let window = |id: EventId| id > after && id <= upto;
    for event in events {
        let Some(task_id) = event.task_id.filter(|&id| counts(Some(id))) else {
            continue;
        };
        let Some(draft) = drafts.get_mut(&task_id) else {
            continue;
        };
        let at = timestamp_millis(&event.created_at);
        match event.kind.as_str() {
            "task_created" => {
                if let Some(at) = at {
                    draft.created = Some((event.id, at));
                }
                draft.draft_at_end = event.id <= upto;
            }
            "task_status_changed" => {
                let to = event.payload["to"].as_str();
                if event.payload["from"] == "draft"
                    && draft.left.is_none()
                    && let Some(at) = at
                {
                    let canceled = (to == Some("canceled"))
                        .then(|| event.payload.get("duplicate_of").is_some_and(Value::is_i64));
                    draft.left = Some((event.id, at, canceled));
                }
                if event.id <= upto {
                    draft.draft_at_end = to == Some("draft");
                }
            }
            "draft_planner_settled" if window(event.id) => {
                if let Some(outcome) = event.payload["outcome"].as_str() {
                    draft.outcome = Some(outcome.to_owned());
                }
            }
            "draft_planner_exhausted" if window(event.id) => {
                flows.entry(draft.category.clone()).or_default().exhausted += 1;
            }
            "ask_opened" if window(event.id) && event.payload["kind"] == "planner_question" => {
                flows
                    .entry(draft.category.clone())
                    .or_default()
                    .planner_questions += 1;
            }
            // An ask the runtime closed itself was not answered.
            "ask_answered"
                if window(event.id)
                    && event.payload["kind"] == "planner_question"
                    && event.payload["runtime_closed"] != true =>
            {
                let option = event.payload["option"].as_str().unwrap_or("free");
                *flows
                    .entry(draft.category.clone())
                    .or_default()
                    .answers
                    .entry(option.to_owned())
                    .or_default() += 1;
            }
            "run_integrated" if window(event.id) => {
                flows.entry(draft.category.clone()).or_default().landed += 1;
            }
            _ => {}
        }
    }
    let mut draft_secs: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut ids: Vec<TaskId> = drafts.keys().copied().collect();
    ids.sort();
    for task_id in ids.into_iter().filter(|&id| counts(Some(id))) {
        let draft = &drafts[&task_id];
        let Some((created, created_at)) = draft.created else {
            continue;
        };
        let flow = flows.entry(draft.category.clone()).or_default();
        if window(created) {
            flow.registered += 1;
        }
        if let Some((id, at, canceled)) = draft.left
            && window(id)
        {
            match canceled {
                None => flow.adopted += 1,
                Some(false) => flow.canceled += 1,
                Some(true) => flow.duplicate += 1,
            }
            draft_secs
                .entry(draft.category.clone())
                .or_default()
                .push((at - created_at).max(0) / 1000);
        }
        if let Some(outcome) = &draft.outcome {
            *flow.planner_outcomes.entry(outcome.clone()).or_default() += 1;
        }
        if draft.draft_at_end {
            let age = (end - created_at).max(0) / 1000;
            flow.backlog += 1;
            if flow.oldest_backlog_secs.is_none_or(|oldest| age > oldest) {
                flow.oldest_backlog_secs = Some(age);
            }
        }
    }
    for (category, flow) in &mut flows {
        let left = flow.adopted + flow.canceled + flow.duplicate;
        flow.adoption_rate = ratio(flow.adopted, left);
        flow.duplicate_rate = ratio(flow.duplicate, left);
        if let Some(secs) = draft_secs.remove(category) {
            flow.draft_secs = summary(secs.iter().copied().map(Some));
            flow.draft_secs_values = secs;
        }
    }
    // A category nothing happened to in the window is left out.
    flows.retain(|_, flow| *flow != CategoryFlow::default());
    flows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, task_id: i64, kind: &str, payload: Value, secs: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task_id)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: format!("1970-01-01T00:{:02}:{:02}.000Z", secs / 60, secs % 60),
            actor: None,
        }
    }

    fn changed(id: i64, task_id: i64, to: &str, extra: Value, secs: i64) -> RunEvent {
        let mut payload = json!({"from": "draft", "to": to});
        if let Some(extra) = extra.as_object() {
            payload.as_object_mut().unwrap().extend(extra.clone());
        }
        event(id, task_id, "task_status_changed", payload, secs)
    }

    /// Five follow_ups of three categories (one registered before
    /// categories were kept): adopted and landed, canceled, canceled as a
    /// duplicate, asked about and exhausted, and still waiting; a person's
    /// task and what happened after the window are left out.
    #[test]
    fn counts_the_follow_up_drafts_by_category() {
        let registered = |id, task, category: Option<&str>| {
            let mut payload = json!({"task_id": task, "index": 0});
            if let Some(category) = category {
                payload["category"] = json!(category);
            }
            event(id, 1, "follow_up_registered", payload, id)
        };
        let events = [
            event(1, 10, "task_created", json!({}), 10),
            registered(2, 10, Some("defect")),
            event(3, 11, "task_created", json!({}), 20),
            registered(4, 11, Some("defect")),
            event(5, 12, "task_created", json!({}), 30),
            registered(6, 12, Some("flaky_test")),
            event(7, 13, "task_created", json!({}), 40),
            registered(8, 13, None),
            event(9, 14, "task_created", json!({}), 50),
            registered(10, 14, Some("flaky_test")),
            event(11, 20, "task_created", json!({}), 55),
            // Task 10 is adopted and lands.
            event(
                12,
                10,
                "draft_planner_settled",
                json!({"outcome": "undecided"}),
                60,
            ),
            changed(13, 10, "submitted", json!({}), 70),
            event(
                14,
                10,
                "draft_planner_settled",
                json!({"outcome": "submitted"}),
                71,
            ),
            event(15, 10, "run_integrated", json!({}), 90),
            // Task 11 is canceled; task 12 is a duplicate of task 3.
            changed(16, 11, "canceled", json!({"duplicate_of": null}), 100),
            event(
                17,
                11,
                "draft_planner_settled",
                json!({"outcome": "canceled"}),
                100,
            ),
            changed(18, 12, "canceled", json!({"duplicate_of": 3}), 110),
            event(
                19,
                12,
                "draft_planner_settled",
                json!({"outcome": "duplicate"}),
                110,
            ),
            // Task 13 is asked about, kept, and exhausted.
            event(
                20,
                13,
                "ask_opened",
                json!({"kind": "planner_question"}),
                120,
            ),
            event(
                21,
                13,
                "ask_answered",
                json!({"kind": "planner_question", "option": "keep_draft"}),
                130,
            ),
            event(
                22,
                13,
                "draft_planner_exhausted",
                json!({"planners": 3}),
                140,
            ),
            // A person's task, not a follow_up.
            changed(23, 20, "submitted", json!({}), 150),
            // After the window: task 14 is adopted.
            changed(24, 14, "submitted", json!({}), 200),
        ];
        let flows =
            follow_up_categories(&events, EventId::new(0), EventId::new(23), 180_000, |_| {
                true
            });
        assert_eq!(
            flows.keys().collect::<Vec<_>>(),
            ["defect", "flaky_test", "unlabeled"]
        );
        let defect = &flows["defect"];
        assert_eq!(
            (
                defect.registered,
                defect.adopted,
                defect.canceled,
                defect.duplicate
            ),
            (2, 1, 1, 0)
        );
        assert_eq!(
            (defect.adoption_rate, defect.duplicate_rate),
            (Some(0.5), Some(0.0))
        );
        assert_eq!(
            defect.planner_outcomes,
            BTreeMap::from([("submitted".to_owned(), 1), ("canceled".to_owned(), 1)])
        );
        assert_eq!(defect.landed, 1);
        assert_eq!(
            defect.draft_secs,
            Summary {
                count: 2,
                total: 60 + 80,
                median: Some(70),
            }
        );
        assert_eq!(defect.backlog, 0);
        let flaky = &flows["flaky_test"];
        assert_eq!((flaky.registered, flaky.duplicate), (2, 1));
        assert_eq!(flaky.duplicate_rate, Some(1.0));
        assert_eq!(
            (flaky.backlog, flaky.oldest_backlog_secs),
            (1, Some(180 - 50))
        );
        let unlabeled = &flows[UNLABELED_CATEGORY];
        assert_eq!(
            (
                unlabeled.planner_questions,
                unlabeled.exhausted,
                unlabeled.backlog
            ),
            (1, 1, 1)
        );
        assert_eq!(
            unlabeled.answers,
            BTreeMap::from([("keep_draft".to_owned(), 1)])
        );
        assert_eq!(unlabeled.adoption_rate, None);
        let json = serde_json::to_value(&flows).unwrap();
        assert_eq!(json["defect"]["planner_outcomes"]["submitted"], 1);

        // A later window: task 14 is adopted, nothing else happened.
        let later =
            follow_up_categories(&events, EventId::new(23), EventId::new(24), 210_000, |_| {
                true
            });
        assert_eq!(later["flaky_test"].adopted, 1);
        assert_eq!(later["flaky_test"].registered, 0);
        assert_eq!(later["unlabeled"].backlog, 1);
        assert!(!later.contains_key("defect"));

        // Only the tasks `counts` accepts.
        let one = follow_up_categories(
            &events,
            EventId::new(0),
            EventId::new(23),
            180_000,
            |task| task == Some(TaskId::new(13)),
        );
        assert_eq!(one.keys().collect::<Vec<_>>(), ["unlabeled"]);
    }

    /// A window that ends between a draft's `task_created` and its
    /// `follow_up_registered` still gives the draft its category, and an
    /// answer the runtime closed the ask with is not counted.
    #[test]
    fn a_draft_registered_at_the_window_edge_and_a_runtime_close() {
        let events = [
            event(1, 10, "task_created", json!({}), 10),
            event(
                2,
                1,
                "follow_up_registered",
                json!({"task_id": 10, "category": "decision"}),
                10,
            ),
            event(3, 10, "ask_opened", json!({"kind": "planner_question"}), 20),
            event(
                4,
                10,
                "ask_answered",
                json!({"kind": "planner_question", "option": "cancel", "runtime_closed": true}),
                30,
            ),
        ];
        let flows =
            follow_up_categories(&events, EventId::new(0), EventId::new(1), 15_000, |_| true);
        assert_eq!(flows["decision"].registered, 1);
        assert_eq!(flows["decision"].backlog, 1);
        let flows =
            follow_up_categories(&events, EventId::new(1), EventId::new(4), 40_000, |_| true);
        let decision = &flows["decision"];
        assert_eq!((decision.registered, decision.planner_questions), (0, 1));
        assert!(decision.answers.is_empty());
    }
}
