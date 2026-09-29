//! The model and effort a worker session is started with (ADR-0079
//! decisions 3 and 4): Opus 5.5 at medium effort, given explicitly, unless
//! the limited trial of `dagq.toml`'s `[worker.trial]` is on and the task
//! is one of its subjects (predicted `mechanical`, its
//! `expected_output_tokens` in the lower third of the latest predictions),
//! whose first claims alternate between the control (Opus 5.5 medium) and
//! the treatment (Sonnet 5 medium). The group stays with the task.
//!
//! A session opened after a failure the task caused (decision 5: a failed
//! verification, a review's `revise`, a concern a person sent back) is
//! raised one step of [`LADDER`], up to Opus 5.5 at `xhigh`; a conflict, a
//! kill or any other reason keeps the step. The raised step stays with the
//! task's later runs.
//!
//! A Codex run takes only the effort of its step: Codex runs its own
//! model, not the step's Claude model (task 892). Its session events name
//! no `model` (the step's is `ladder_model`, and `model_unknown` says why),
//! it is in no trial group, and each of its turns records the model Codex
//! says it used.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{
    DomainError, Provider, ReasonCode, RunEvent, TaskId, event_kind,
    prediction::{PREDICTION_WINDOW, TaskNature, percentile, window},
    reason::event_code,
    resume,
};

/// The model of the control and of every session outside the trial.
pub const OPUS: &str = "claude-opus-5-5";
/// The model of the trial's treatment.
pub const SONNET: &str = "claude-sonnet-5";
/// The effort of both groups.
pub const MEDIUM: &str = "medium";
/// The efforts above [`MEDIUM`] a task-caused failure raises Opus to.
pub const HIGH: &str = "high";
pub const XHIGH: &str = "xhigh";
/// The steps a worker session is raised by (ADR-0079 decision 5), lowest
/// first; the last is the top, which stays.
pub const LADDER: [(&str, &str); 4] = [
    (SONNET, MEDIUM),
    (OPUS, MEDIUM),
    (OPUS, HIGH),
    (OPUS, XHIGH),
];
/// Why a Codex run's session events name no model.
pub const CODEX_MODEL_UNKNOWN: &str = "codex runs its own model, not the claim's Claude model: each turn_finished records the model codex used";

/// The reason of a raise for a review's `revise`: the other reasons are
/// the codes that parked the run ([`raises`]).
pub const REVISE: &str = "revise";
/// The events that open a worker session and record its model and effort.
pub const SESSION_EVENTS: [&str; 3] = [
    event_kind::RUN_CLAIMED,
    event_kind::RESUME_STARTED,
    event_kind::REVISE_REQUESTED,
];

/// Whether a run parked with `code` resumes one step higher: a failed
/// verification (`integrate`'s or the landing recheck's) or a review's
/// concern a person sent back, the task-caused rework of ADR-0079's
/// decision 1. A conflict, a kill, `evidence_missing`, `scope_violation`
/// and the rest keep the step.
pub const fn raises(code: ReasonCode) -> bool {
    matches!(code, ReasonCode::VerificationFailed | ReasonCode::SentBack)
}

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

    /// [`Self::fields`] for a session on `provider`. Codex's names no
    /// `model`: the step's Claude model is `ladder_model`, which the next
    /// session is raised from, `model_unknown` says why, and it is in no
    /// trial group.
    pub fn fields_on(&self, provider: Provider) -> Map<String, Value> {
        let mut fields = self.fields();
        if provider == Provider::Codex {
            fields.insert("model".to_owned(), Value::Null);
            fields.insert("ladder_model".to_owned(), json!(self.model));
            fields.insert("group".to_owned(), Value::Null);
            fields.insert("model_unknown".to_owned(), json!(CODEX_MODEL_UNKNOWN));
        }
        fields
    }

    /// `model` and `effort` of the session on `provider`, as a raise names
    /// the session before it: Codex's `model` is null and the step's model
    /// is `ladder_model`.
    pub fn named_on(&self, provider: Provider) -> Value {
        match provider {
            Provider::Codex => {
                json!({"model": null, "ladder_model": self.model, "effort": self.effort})
            }
            Provider::Claude => json!({"model": self.model, "effort": self.effort}),
        }
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
        Self::recorded(&claimed.payload).unwrap_or_default()
    }
}

impl WorkerSession {
    /// `model/effort`, as `stats` names a session.
    pub fn label(&self) -> String {
        format!("{}/{}", self.model, self.effort)
    }

    /// Where the session is on [`LADDER`]; `None` off it.
    pub fn step(&self) -> Option<usize> {
        LADDER
            .iter()
            .position(|&(model, effort)| self.model == model && self.effort == effort)
    }

    /// The session one step up [`LADDER`], in the same group; `None` at the
    /// top, or for a session off the ladder.
    pub fn raised(&self) -> Option<Self> {
        let &(model, effort) = LADDER.get(self.step()? + 1)?;
        Some(Self {
            model: model.to_owned(),
            effort: effort.to_owned(),
            group: self.group,
        })
    }

    /// The session opened last: `model`, `effort` and `group` of the latest
    /// of [`SESSION_EVENTS`] among `events` (a run's, or a task's, oldest
    /// first) that records them; the default without one.
    pub fn current(events: &[RunEvent]) -> Self {
        Self::latest(events).unwrap_or_default()
    }

    /// [`Self::current`], `None` without a recorded session.
    fn latest(events: &[RunEvent]) -> Option<Self> {
        openings(events)
            .into_iter()
            .rev()
            .find_map(|index| Self::recorded(&events[index].payload))
    }

    /// The session a payload records: a Codex run's step is its
    /// `ladder_model`.
    fn recorded(payload: &Value) -> Option<Self> {
        Some(Self {
            model: payload["model"]
                .as_str()
                .or_else(|| payload["ladder_model"].as_str())?
                .to_owned(),
            effort: payload["effort"].as_str()?.to_owned(),
            group: group_of(payload),
        })
    }

    /// The session a new run of the task starts with, `self` being the one
    /// the claim chose: once a session of the task was raised, the latest
    /// session among `task_events` (the task's) when it is higher, in
    /// `self`'s group (ADR-0079 decision 5: a raise stays with the task's
    /// later runs, retries included). Whether it was inherited.
    pub fn inheriting(self, task_events: &[RunEvent]) -> (Self, bool) {
        // Only a raise stays: a task claimed at the default before it
        // joined the trial keeps its group's session.
        let raised = openings(task_events)
            .into_iter()
            .any(|index| task_events[index].payload["escalated_from"].is_object());
        let Some(last) = Self::latest(task_events).filter(|_| raised) else {
            return (self, false);
        };
        match (last.step(), self.step()) {
            (Some(last_step), Some(step)) if last_step > step => (
                Self {
                    group: self.group,
                    ..last
                },
                true,
            ),
            _ => (self, false),
        }
    }

    /// [`Self::fields_on`] with the raise that led to the session, if any:
    /// `escalated_from` (the model and effort before, [`Self::named_on`])
    /// and `escalation_reason`.
    pub fn fields_raised(
        &self,
        provider: Provider,
        raise: Option<&Escalation>,
    ) -> Map<String, Value> {
        let mut fields = self.fields_on(provider);
        if let Some(raise) = raise {
            fields.insert("escalated_from".to_owned(), raise.from.named_on(provider));
            fields.insert("escalation_reason".to_owned(), json!(raise.reason));
        }
        fields
    }
}

/// A raise of a worker session: the session before it and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escalation {
    pub from: WorkerSession,
    pub reason: String,
}

/// The session a resume of the run opens, from the run's `events` (oldest
/// first) before its `resume_started`: the session opened last, one step
/// higher when the latest event that parked the run came after it and
/// carries a code that [`raises`]; with the raise, when there was one. A
/// run parked again without a new session in between (a resume that ended
/// without one) is not raised twice for one failure.
pub fn for_resume(events: &[RunEvent]) -> (WorkerSession, Option<Escalation>) {
    let current = WorkerSession::current(events);
    let opened = openings(events).last().copied();
    let parked = events.iter().rposition(resume::parks);
    let code = match (parked, opened) {
        (Some(parked), Some(opened)) if parked > opened => event_code(&events[parked]),
        (Some(parked), None) => event_code(&events[parked]),
        _ => None,
    };
    match code.filter(|&code| raises(code)).and_then(|code| {
        let raised = current.raised()?;
        Some((raised, code))
    }) {
        Some((raised, code)) => (
            raised,
            Some(Escalation {
                from: current,
                reason: code.as_str().to_owned(),
            }),
        ),
        None => (current, None),
    }
}

/// The indexes of `events` that opened a worker session, oldest first: the
/// [`SESSION_EVENTS`], but a `revise_requested` the supervisor withdrew
/// (a `revise_unsent` of the same run and attempt), whose request and raise
/// never reached the session.
fn openings(events: &[RunEvent]) -> Vec<usize> {
    let withdrawn: Vec<(Option<&str>, &Value)> = events
        .iter()
        .filter(|event| event.kind == event_kind::REVISE_UNSENT)
        .map(|event| {
            (
                event.run_id.as_ref().map(|id| id.as_str()),
                &event.payload["attempt"],
            )
        })
        .collect();
    events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            SESSION_EVENTS.contains(&event.kind.as_str())
                && !(event.kind == event_kind::REVISE_REQUESTED
                    && withdrawn.contains(&(
                        event.run_id.as_ref().map(|id| id.as_str()),
                        &event.payload["attempt"],
                    )))
        })
        .map(|(index, _)| index)
        .collect()
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

    fn opened(id: i64, kind: &str, model: &str, effort: &str) -> RunEvent {
        event(
            id,
            1,
            kind,
            json!({"model": model, "effort": effort, "group": null}),
        )
    }

    fn session(model: &str, effort: &str) -> WorkerSession {
        WorkerSession {
            model: model.to_owned(),
            effort: effort.to_owned(),
            group: None,
        }
    }

    #[test]
    fn the_ladder_raises_one_step_up_to_opus_xhigh() {
        let mut treatment = WorkerSession::of_group(TrialGroup::Treatment);
        let mut steps = vec![treatment.label()];
        while let Some(next) = treatment.raised() {
            assert_eq!(next.group, Some(TrialGroup::Treatment));
            steps.push(next.label());
            treatment = next;
        }
        assert_eq!(
            steps,
            [
                "claude-sonnet-5/medium",
                "claude-opus-5-5/medium",
                "claude-opus-5-5/high",
                "claude-opus-5-5/xhigh"
            ]
        );
        assert_eq!(WorkerSession::default().step(), Some(1));
        // Off the ladder: not raised.
        assert_eq!(session("other", "medium").raised(), None);
        assert_eq!(session(OPUS, "low").step(), None);
        assert!(raises(ReasonCode::VerificationFailed));
        assert!(raises(ReasonCode::SentBack));
        for code in [
            ReasonCode::RebaseConflict,
            ReasonCode::SessionKilled,
            ReasonCode::EvidenceMissing,
            ReasonCode::ScopeViolation,
            ReasonCode::MigrationNumberTaken,
            ReasonCode::TriageResume,
        ] {
            assert!(!raises(code), "{code}");
        }
    }

    #[test]
    fn the_current_session_is_the_one_opened_last() {
        assert_eq!(WorkerSession::current(&[]), WorkerSession::default());
        let events = [
            opened(1, "run_claimed", SONNET, MEDIUM),
            opened(2, "resume_started", OPUS, MEDIUM),
            opened(3, "revise_requested", OPUS, HIGH),
            // Not a session's event, and one without its values.
            opened(4, "agent_started", OPUS, XHIGH),
            event(5, 1, "resume_started", json!({"attempt": 2})),
        ];
        assert_eq!(WorkerSession::current(&events), session(OPUS, HIGH));
        let raise = Escalation {
            from: session(OPUS, MEDIUM),
            reason: REVISE.to_owned(),
        };
        assert_eq!(
            Value::Object(session(OPUS, HIGH).fields_raised(Provider::Claude, Some(&raise))),
            json!({"model": OPUS, "effort": HIGH, "group": null,
                   "escalated_from": {"model": OPUS, "effort": MEDIUM},
                   "escalation_reason": "revise"})
        );
        assert_eq!(
            session(OPUS, HIGH).fields_raised(Provider::Claude, None),
            session(OPUS, HIGH).fields()
        );
    }

    /// Task 892: a Codex run's session names no Claude model and no group;
    /// its step is kept as `ladder_model`, which the next session is read
    /// and raised from.
    #[test]
    fn a_codex_session_names_no_claude_model_and_keeps_its_step() {
        let treatment = WorkerSession::of_group(TrialGroup::Treatment);
        let fields = Value::Object(treatment.fields_on(Provider::Codex));
        assert_eq!(
            fields,
            json!({"model": null, "ladder_model": SONNET, "effort": MEDIUM, "group": null,
                   "model_unknown": CODEX_MODEL_UNKNOWN})
        );
        assert_eq!(
            Value::Object(treatment.fields_on(Provider::Claude)),
            Value::Object(treatment.fields())
        );
        let raise = Escalation {
            from: session(OPUS, MEDIUM),
            reason: REVISE.to_owned(),
        };
        let raised =
            Value::Object(session(OPUS, HIGH).fields_raised(Provider::Codex, Some(&raise)));
        assert_eq!(raised["model"], Value::Null);
        assert_eq!(
            raised["escalated_from"],
            json!({"model": null, "ladder_model": OPUS, "effort": MEDIUM})
        );
        // Read back: the step, out of any group.
        let mut claim = event(1, 1, "run_claimed", fields);
        claim.run_id = Some(RunId::new(RUN).unwrap());
        assert_eq!(
            WorkerSession::of_run(std::slice::from_ref(&claim)),
            session(SONNET, MEDIUM)
        );
        let revised = event(2, 1, "revise_requested", raised);
        let events = [claim, revised];
        assert_eq!(WorkerSession::current(&events), session(OPUS, HIGH));
        assert_eq!(session(OPUS, HIGH).raised(), Some(session(OPUS, XHIGH)));
        // A raise stays with the task's later runs, Codex's too.
        assert_eq!(
            WorkerSession::default().inheriting(&events),
            (session(OPUS, HIGH), true)
        );
    }

    #[test]
    fn a_resume_is_raised_once_for_a_failure_the_task_caused() {
        let parked = |id, code: &str| {
            event(
                id,
                1,
                "integration_deferred",
                json!({"code": code, "status": "needs_session"}),
            )
        };
        let claim = opened(1, "run_claimed", OPUS, MEDIUM);
        // A failed verification after the claim raises it.
        let (next, raise) = for_resume(&[claim.clone(), parked(2, "verification_failed")]);
        assert_eq!(next, session(OPUS, HIGH));
        let raise = raise.unwrap();
        assert_eq!(
            (raise.from, raise.reason.as_str()),
            (session(OPUS, MEDIUM), "verification_failed")
        );
        // A conflict does not.
        assert_eq!(
            for_resume(&[claim.clone(), parked(2, "rebase_conflict")]),
            (session(OPUS, MEDIUM), None)
        );
        // A park before the session opened last is not raised again.
        let events = [
            claim.clone(),
            parked(2, "verification_failed"),
            opened(3, "resume_started", OPUS, HIGH),
        ];
        assert_eq!(for_resume(&events), (session(OPUS, HIGH), None));
        // The landing recheck's failed verification raises too; at the top
        // the step stays.
        let recheck = event(
            4,
            1,
            "landing_recheck_failed",
            json!({"action": "resumed", "code": "verification_failed"}),
        );
        let top = [opened(1, "run_claimed", OPUS, XHIGH), recheck.clone()];
        assert_eq!(for_resume(&top), (session(OPUS, XHIGH), None));
        // Without any session recorded, the park is still read.
        assert_eq!(for_resume(&[recheck]).0, session(OPUS, HIGH));
        assert_eq!(for_resume(&[]), (WorkerSession::default(), None));
    }

    /// A revise the supervisor withdrew never reached the session: its
    /// raise does not count, and a person's `send_back` after it raises
    /// from the session that ran.
    #[test]
    fn a_withdrawn_revise_opens_no_session() {
        let mut requested = opened(2, "revise_requested", OPUS, HIGH);
        requested.payload["attempt"] = json!(1);
        requested.payload["escalated_from"] = json!({"model": OPUS, "effort": MEDIUM});
        let unsent = event(3, 1, "revise_unsent", json!({"attempt": 1}));
        let sent_back = event(
            4,
            1,
            "landing_decided",
            json!({"code": "sent_back", "status": "needs_session"}),
        );
        let events = [
            opened(1, "run_claimed", OPUS, MEDIUM),
            requested.clone(),
            unsent.clone(),
            sent_back,
        ];
        assert_eq!(WorkerSession::current(&events[..3]), session(OPUS, MEDIUM));
        let (next, raise) = for_resume(&events);
        assert_eq!(next, session(OPUS, HIGH));
        assert_eq!(raise.unwrap().from, session(OPUS, MEDIUM));
        // Nor does it stay with the task.
        assert_eq!(
            session(OPUS, MEDIUM).inheriting(&[requested.clone(), unsent]),
            (session(OPUS, MEDIUM), false)
        );
        // A revise of another attempt that was sent stays.
        assert_eq!(WorkerSession::current(&[requested]), session(OPUS, HIGH));
    }

    #[test]
    fn a_raised_step_stays_with_the_task() {
        let raised = event(
            3,
            1,
            "resume_started",
            json!({"model": OPUS, "effort": XHIGH, "group": "treatment",
                   "escalated_from": {"model": OPUS, "effort": HIGH}}),
        );
        let treatment = WorkerSession::of_group(TrialGroup::Treatment);
        let (session_now, inherited) = treatment
            .clone()
            .inheriting(&[opened(1, "run_claimed", SONNET, MEDIUM), raised.clone()]);
        assert!(inherited);
        assert_eq!(session_now.label(), "claude-opus-5-5/xhigh");
        assert_eq!(session_now.group, Some(TrialGroup::Treatment));
        // Nothing raised: the claim's choice, even below the last session.
        let unraised = [opened(1, "run_claimed", OPUS, MEDIUM)];
        assert_eq!(
            treatment.clone().inheriting(&unraised),
            (treatment.clone(), false)
        );
        assert_eq!(
            treatment.clone().inheriting(&[]),
            (treatment.clone(), false)
        );
        // A choice as high as the raise keeps the choice.
        let high = session(OPUS, XHIGH);
        assert_eq!(high.clone().inheriting(&[raised]), (high, false));
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
