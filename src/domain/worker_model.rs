//! The model and effort a worker session is started with (ADR-0079
//! decisions 3 and 4): Opus 5.5 at medium effort, given explicitly, unless
//! the limited trial of `dagq.toml`'s `[worker.trial]` is on and the task
//! is one of its subjects (predicted `mechanical`, its
//! `expected_output_tokens` in the lower third of the latest predictions),
//! whose first claims alternate between the control (Opus 5.5 medium) and
//! the treatment (Sonnet 5 medium). The group stays with the task.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{
    DomainError, RunEvent, TaskId, event_kind,
    prediction::{PREDICTION_WINDOW, TaskNature, percentile, window},
};

/// The model of the control and of every session outside the trial.
pub const OPUS: &str = "claude-opus-5-5";
/// The model of the trial's treatment.
pub const SONNET: &str = "claude-sonnet-5";
/// The effort of both groups.
pub const MEDIUM: &str = "medium";
/// The highest percentile of the lower third (ADR-0079 decision 4); the
/// percentile is rounded to one decimal.
pub const LOWER_THIRD: f64 = 33.3;

string_enum!(TrialGroup {
    Control => "control",
    Treatment => "treatment",
});

impl TrialGroup {
    /// The other group.
    pub const fn other(self) -> Self {
        match self {
            Self::Control => Self::Treatment,
            Self::Treatment => Self::Control,
        }
    }
}

/// `[worker.trial]` of `dagq.toml`: off unless a person turns it on, and
/// how many of the latest predictions a task is ranked among.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerTrial {
    pub enabled: bool,
    pub window: usize,
}

impl Default for WorkerTrial {
    fn default() -> Self {
        Self {
            enabled: false,
            window: PREDICTION_WINDOW,
        }
    }
}

impl WorkerTrial {
    pub const KEYS: [&'static str; 2] = ["enabled", "window"];
}

/// The model and effort of a worker session, and its trial group (none
/// outside the trial).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSession {
    pub model: String,
    pub effort: String,
    pub group: Option<TrialGroup>,
}

impl Default for WorkerSession {
    fn default() -> Self {
        Self {
            model: OPUS.to_owned(),
            effort: MEDIUM.to_owned(),
            group: None,
        }
    }
}

impl WorkerSession {
    /// The session of a trial group.
    pub fn of_group(group: TrialGroup) -> Self {
        let model = match group {
            TrialGroup::Control => OPUS,
            TrialGroup::Treatment => SONNET,
        };
        Self {
            model: model.to_owned(),
            effort: MEDIUM.to_owned(),
            group: Some(group),
        }
    }

    /// `model`, `effort` and `group` for the payload of the event that
    /// opens the session (`run_claimed`, `resume_started`,
    /// `revise_requested`).
    pub fn fields(&self) -> Map<String, Value> {
        let Value::Object(fields) = json!({
            "model": self.model,
            "effort": self.effort,
            "group": self.group,
        }) else {
            unreachable!("an object")
        };
        fields
    }

    /// The session a run was claimed with: `model`, `effort` and `group` of
    /// its first `run_claimed` among `events` (the run's). A run claimed
    /// before they were recorded, or one without its claim, gets the
    /// default, which is what it ran with.
    pub fn of_run(events: &[RunEvent]) -> Self {
        let Some(claimed) = events
            .iter()
            .find(|event| event.kind == event_kind::RUN_CLAIMED)
        else {
            return Self::default();
        };
        let payload = &claimed.payload;
        match (payload["model"].as_str(), payload["effort"].as_str()) {
            (Some(model), Some(effort)) => Self {
                model: model.to_owned(),
                effort: effort.to_owned(),
                group: group_of(payload),
            },
            _ => Self::default(),
        }
    }
}

fn group_of(payload: &Value) -> Option<TrialGroup> {
    payload["group"].as_str()?.parse().ok()
}

/// Why a task is or is not in the trial, to read next to its group.
#[derive(Debug, Clone, PartialEq)]
pub struct TrialChoice {
    pub session: WorkerSession,
    /// Where the task's `expected_output_tokens` fell among the latest
    /// predictions, when it was ranked.
    pub percentile: Option<f64>,
}

/// The session `task` is claimed with now. `events` are the queue's
/// `task_weight_predicted` and `run_claimed`, oldest first (others are
/// ignored).
///
/// With the trial off, or for a task outside it, the default. A task that
/// was assigned a group keeps it. Otherwise the task joins when its last
/// prediction is `mechanical` and its `expected_output_tokens` is at most
/// the [`LOWER_THIRD`] percentile of the last prediction of the
/// `trial.window` latest other tasks; with fewer predictions than that, or
/// without its own, it does not. A task that joins takes the group other
/// than the last task that joined; the first is the control.
pub fn choose(trial: &WorkerTrial, task: TaskId, events: &[RunEvent]) -> TrialChoice {
    let outside = |percentile| TrialChoice {
        session: WorkerSession::default(),
        percentile,
    };
    if !trial.enabled {
        return outside(None);
    }
    // The first group of each task, in the order they joined.
    let mut joined: Vec<(TaskId, TrialGroup)> = Vec::new();
    let mut history: Vec<(TaskId, u64)> = Vec::new();
    let mut own: Option<(TaskNature, u64)> = None;
    for event in events {
        let Some(task_id) = event.task_id else {
            continue;
        };
        match event.kind.as_str() {
            event_kind::RUN_CLAIMED => {
                if let Some(group) = group_of(&event.payload)
                    && joined.iter().all(|(other, _)| *other != task_id)
                {
                    joined.push((task_id, group));
                }
            }
            event_kind::TASK_WEIGHT_PREDICTED => {
                let prediction = &event.payload["prediction"];
                let Some(tokens) = prediction["expected_output_tokens"].as_u64() else {
                    continue;
                };
                history.push((task_id, tokens));
                if task_id == task {
                    own = prediction["nature"]
                        .as_str()
                        .and_then(|nature| nature.parse().ok())
                        .map(|nature| (nature, tokens));
                }
            }
            _ => {}
        }
    }
    if let Some(&(_, group)) = joined.iter().find(|(other, _)| *other == task) {
        return TrialChoice {
            session: WorkerSession::of_group(group),
            percentile: None,
        };
    }
    let Some((nature, tokens)) = own else {
        return outside(None);
    };
    let others = window(&history, task, trial.window);
    if others.len() < trial.window {
        return outside(None);
    }
    let rank = percentile(tokens, &others);
    if nature != TaskNature::Mechanical || rank.is_none_or(|rank| rank > LOWER_THIRD) {
        return outside(rank);
    }
    let group = joined
        .last()
        .map_or(TrialGroup::Control, |&(_, last)| last.other());
    TrialChoice {
        session: WorkerSession::of_group(group),
        percentile: rank,
    }
}

impl std::fmt::Display for TrialGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, RunId};

    const RUN: &str = "11111111-1111-4111-8111-111111111111";

    fn event(id: i64, task: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: "2027-01-15T08:00:00.000Z".to_owned(),
            actor: None,
        }
    }

    fn predicted(id: i64, task: i64, nature: &str, tokens: u64) -> RunEvent {
        event(
            id,
            task,
            "task_weight_predicted",
            json!({"prediction": {"nature": nature, "expected_output_tokens": tokens}}),
        )
    }

    fn claimed(id: i64, task: i64, group: Option<&str>) -> RunEvent {
        event(id, task, "run_claimed", json!({"group": group}))
    }

    /// Ten other tasks predicted 10k to 100k tokens.
    fn history() -> Vec<RunEvent> {
        (1..=10)
            .map(|n| predicted(n, 100 + n, "implementation", n as u64 * 10_000))
            .collect()
    }

    fn on() -> WorkerTrial {
        WorkerTrial {
            enabled: true,
            window: 10,
        }
    }

    #[test]
    fn the_default_session_is_opus_at_medium_outside_any_group() {
        let session = WorkerSession::default();
        assert_eq!(
            (
                session.model.as_str(),
                session.effort.as_str(),
                session.group
            ),
            (OPUS, MEDIUM, None)
        );
        assert_eq!(
            Value::Object(session.fields()),
            json!({"model": "claude-opus-5-5", "effort": "medium", "group": null})
        );
        let treatment = WorkerSession::of_group(TrialGroup::Treatment);
        assert_eq!(
            Value::Object(treatment.fields()),
            json!({"model": "claude-sonnet-5", "effort": "medium", "group": "treatment"})
        );
        assert_eq!(WorkerSession::of_group(TrialGroup::Control).model, OPUS);
        assert_eq!(WorkerTrial::default().window, 60);
        assert!(!WorkerTrial::default().enabled);
    }

    #[test]
    fn a_run_is_resumed_with_the_session_it_was_claimed_with() {
        let mut claim = claimed(1, 1, Some("treatment"));
        claim.run_id = Some(RunId::new(RUN).unwrap());
        claim.payload["model"] = json!(SONNET);
        claim.payload["effort"] = json!(MEDIUM);
        let later = event(2, 1, "run_claimed", json!({"model": "x", "effort": "y"}));
        assert_eq!(
            WorkerSession::of_run(&[event(0, 1, "agent_started", json!({})), claim, later]),
            WorkerSession::of_group(TrialGroup::Treatment)
        );
        // Claimed before the session was recorded, or no claim at all.
        assert_eq!(
            WorkerSession::of_run(&[claimed(1, 1, None)]),
            WorkerSession::default()
        );
        assert_eq!(WorkerSession::of_run(&[]), WorkerSession::default());
    }

    #[test]
    fn the_trial_off_starts_every_task_at_the_default() {
        let mut events = history();
        events.push(predicted(11, 1, "mechanical", 1));
        let choice = choose(&WorkerTrial::default(), TaskId::new(1), &events);
        assert_eq!(choice.session, WorkerSession::default());
        assert_eq!(choice.percentile, None);
    }

    #[test]
    fn mechanical_tasks_of_the_lower_third_alternate_between_the_groups() {
        let t = TaskId::new;
        let mut events = history();
        events.push(predicted(11, 1, "mechanical", 15_000));
        let first = choose(&on(), t(1), &events);
        assert_eq!(first.session, WorkerSession::of_group(TrialGroup::Control));
        assert_eq!(first.percentile, Some(10.0));
        events.push(claimed(12, 1, Some("control")));
        // The next subject is the treatment, the one after it the control.
        events.push(predicted(13, 2, "mechanical", 20_000));
        let second = choose(&on(), t(2), &events);
        assert_eq!(second.session.group, Some(TrialGroup::Treatment));
        assert_eq!(second.session.model, SONNET);
        assert_eq!(second.session.effort, MEDIUM);
        events.push(claimed(14, 2, Some("treatment")));
        // A task outside the trial between them changes nothing.
        events.push(claimed(15, 101, None));
        events.push(predicted(16, 3, "mechanical", 5_000));
        assert_eq!(
            choose(&on(), t(3), &events).session.group,
            Some(TrialGroup::Control)
        );
        // A retry of the treatment keeps its group, even when its prediction
        // has moved out of the lower third since.
        events.push(claimed(17, 2, Some("treatment")));
        events.push(predicted(18, 2, "design_judgment", 999_999));
        assert_eq!(
            choose(&on(), t(2), &events).session,
            WorkerSession::of_group(TrialGroup::Treatment)
        );
        // With the trial turned off again, the group is not applied.
        assert_eq!(
            choose(&WorkerTrial::default(), t(2), &events).session,
            WorkerSession::default()
        );
    }

    #[test]
    fn other_tasks_stay_at_the_default() {
        let t = TaskId::new;
        let mut events = history();
        // Not mechanical.
        events.push(predicted(11, 1, "implementation", 5_000));
        let choice = choose(&on(), t(1), &events);
        assert_eq!(choice.session, WorkerSession::default());
        assert_eq!(choice.percentile, Some(0.0));
        // Mechanical, above the lower third: 40k is above 3 of the 10.
        events.push(predicted(12, 2, "mechanical", 40_000));
        let choice = choose(&on(), t(2), &events);
        assert_eq!(choice.session, WorkerSession::default());
        assert_eq!(choice.percentile, Some(35.0));
        // Without a prediction.
        assert_eq!(
            choose(&on(), t(3), &events).session,
            WorkerSession::default()
        );
        // Fewer other predictions than the window.
        let few: Vec<RunEvent> = events[5..].to_vec();
        let mut short = few.clone();
        short.push(predicted(20, 4, "mechanical", 1));
        let choice = choose(&on(), t(4), &short);
        assert_eq!(choice.session, WorkerSession::default());
        assert_eq!(choice.percentile, None);
        // The task's own earlier predictions are not among the others: only
        // its last one counts, ranked against ten others.
        let mut own = history();
        own.push(predicted(11, 5, "mechanical", 900_000));
        own.push(predicted(12, 5, "mechanical", 1));
        assert_eq!(
            choose(&on(), t(5), &own).session.group,
            Some(TrialGroup::Control)
        );
        // An unreadable prediction or group is skipped.
        let mut odd = history();
        odd.push(event(
            11,
            6,
            "task_weight_predicted",
            json!({"prediction": {}}),
        ));
        odd.push(event(12, 6, "run_claimed", json!({"group": "other"})));
        assert_eq!(choose(&on(), t(6), &odd).session, WorkerSession::default());
    }

    #[test]
    fn groups_read_back() {
        assert_eq!(
            "treatment".parse::<TrialGroup>().unwrap(),
            TrialGroup::Treatment
        );
        assert_eq!(TrialGroup::Control.other(), TrialGroup::Treatment);
        assert_eq!(TrialGroup::Treatment.other(), TrialGroup::Control);
        assert_eq!(TrialGroup::Control.to_string(), "control");
        assert!("x".parse::<TrialGroup>().is_err());
    }
}
