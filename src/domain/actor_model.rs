//! The model and effort of the sessions other than the worker's (ADR-0079
//! decision 7 (b)(c)): the plan review, the review, the recovery (triage)
//! job, the observer, the runtime's planners and a person's planner. Each
//! role takes `[roles.<role>]` of `dagq.toml` when it has one, and is
//! started as before (the provider's default, no model or effort given)
//! when it has none. A planner the runtime opens again for a plan review's
//! `revise` is raised one effort step (up to `xhigh`). What a session was
//! started with, and why, is its [`ActorLaunch`], recorded with the event
//! that starts it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    DomainError,
    worker_model::{MEDIUM, OPUS},
};

string_enum!(ActorRole {
    PlanReview => "plan_review",
    Review => "review",
    Recovery => "recovery",
    Observer => "observer",
    RuntimePlanner => "runtime_planner",
    Planner => "planner",
});

impl ActorRole {
    pub const ALL: [Self; 6] = [
        Self::PlanReview,
        Self::Review,
        Self::Recovery,
        Self::Observer,
        Self::RuntimePlanner,
        Self::Planner,
    ];
}

/// The efforts a role may name, lowest first.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// The highest effort a revise raises a planner to.
pub const TOP_EFFORT: &str = "xhigh";
/// Why a planner opened for a plan review's `revise` was raised.
pub const REVISE_ESCALATION: &str = "plan_review_revise";
/// Why a revise delivered to a live planner did not raise its effort: the
/// runtime does not switch the model or effort inside a running session.
pub const LIVE_PLANNER_NOT_RAISED: &str =
    "the planner's session is live; its effort is not switched inside it";

/// The effort one step above `effort`: `low` → `medium` → `high` →
/// `xhigh`, which stays; `max` (above the top) and an effort not known
/// stay as they are.
pub fn raise(effort: &str) -> &str {
    match effort {
        "low" => MEDIUM,
        "medium" => "high",
        "high" => TOP_EFFORT,
        other => other,
    }
}

/// `[roles.<role>]` of `dagq.toml`: a model and an effort, either of which
/// may be left out (the default's then).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleModel {
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl RoleModel {
    pub const KEYS: [&'static str; 2] = ["model", "effort"];
}

/// Every `[roles.<role>]` of `dagq.toml`; none by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleModels {
    roles: Vec<(ActorRole, RoleModel)>,
}

impl RoleModels {
    /// The table of `role`, empty until a key is set.
    pub fn entry(&mut self, role: ActorRole) -> &mut RoleModel {
        let at = match self.roles.iter().position(|(known, _)| *known == role) {
            Some(at) => at,
            None => {
                self.roles.push((role, RoleModel::default()));
                self.roles.len() - 1
            }
        };
        &mut self.roles[at].1
    }

    pub fn get(&self, role: ActorRole) -> Option<&RoleModel> {
        self.roles
            .iter()
            .find(|(known, _)| *known == role)
            .map(|(_, model)| model)
    }

    /// What a session of `role` starts with: its table's model and effort
    /// (the default for the one it leaves out), or, without a table, the
    /// provider's default with nothing given.
    pub fn launch(&self, role: ActorRole) -> ActorLaunch {
        match self.get(role) {
            Some(table) => ActorLaunch {
                role,
                model: Some(table.model.clone().unwrap_or_else(|| OPUS.to_owned())),
                effort: Some(table.effort.clone().unwrap_or_else(|| MEDIUM.to_owned())),
                source: LaunchSource::Config,
                escalated_from: None,
                escalation_reason: None,
            },
            None => ActorLaunch::default_of(role),
        }
    }
}

string_enum!(LaunchSource {
    Default => "default",
    Config => "dagq.toml",
    ReviseEscalation => "revise_escalation",
});

/// The model and effort a session was started with, and where they came
/// from. `model` / `effort` are `None` when none was given (the provider's
/// default, which the transcript names once the session closes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorLaunch {
    pub role: ActorRole,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub source: LaunchSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalated_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_reason: Option<String>,
}

impl ActorLaunch {
    /// A session of `role` started as before: nothing given.
    pub fn default_of(role: ActorRole) -> Self {
        Self {
            role,
            model: None,
            effort: None,
            source: LaunchSource::Default,
            escalated_from: None,
            escalation_reason: None,
        }
    }

    /// The model and effort to give the provider, when there are.
    pub fn arguments(&self) -> Option<(&str, &str)> {
        Some((self.model.as_deref()?, self.effort.as_deref()?))
    }

    /// This launch one effort step higher for `reason` (ADR-0079 decision 7
    /// (c)): the effort it gives, or `medium` when it gives none, raised
    /// ([`raise`]); the model it gives, or Opus 5.5, unchanged.
    pub fn escalated(self, reason: &str) -> Self {
        let from = self.effort.unwrap_or_else(|| MEDIUM.to_owned());
        Self {
            role: self.role,
            model: Some(self.model.unwrap_or_else(|| OPUS.to_owned())),
            effort: Some(raise(&from).to_owned()),
            source: LaunchSource::ReviseEscalation,
            escalated_from: Some(from),
            escalation_reason: Some(reason.to_owned()),
        }
    }

    /// The launch as it is recorded (`launch` of the event that starts the
    /// session and of its `session_opened`).
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("a launch serializes")
    }

    /// The launch recorded in `payload` (its `launch`), or the default of
    /// `role` when there is none or it cannot be read (one recorded before
    /// launches were).
    pub fn recorded(payload: &Value, role: ActorRole) -> Self {
        payload
            .get("launch")
            .and_then(|launch| serde_json::from_value(launch.clone()).ok())
            .unwrap_or_else(|| Self::default_of(role))
    }
}

/// Check a role's `effort` value: one of [`EFFORTS`].
pub fn check_effort(effort: &str) -> Result<(), DomainError> {
    if EFFORTS.contains(&effort) {
        Ok(())
    } else {
        Err(DomainError::UnknownValue {
            kind: "effort",
            value: effort.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_role_without_a_table_is_started_as_before() {
        let models = RoleModels::default();
        for role in ActorRole::ALL {
            let launch = models.launch(role);
            assert_eq!(launch, ActorLaunch::default_of(role));
            assert_eq!(launch.arguments(), None);
            assert_eq!(launch.to_value()["source"], "default");
        }
    }

    #[test]
    fn a_table_gives_its_model_and_effort_and_the_default_for_the_rest() {
        let mut models = RoleModels::default();
        models.entry(ActorRole::PlanReview).effort = Some("high".into());
        models.entry(ActorRole::Observer).model = Some("claude-sonnet-5".into());
        let plan = models.launch(ActorRole::PlanReview);
        assert_eq!(plan.arguments(), Some((OPUS, "high")));
        assert_eq!(plan.source, LaunchSource::Config);
        assert_eq!(
            models.launch(ActorRole::Observer).arguments(),
            Some(("claude-sonnet-5", MEDIUM))
        );
        assert_eq!(models.launch(ActorRole::Review).arguments(), None);
        assert_eq!(plan.to_value()["source"], "dagq.toml");
        assert!(plan.to_value().get("escalated_from").is_none());
    }

    #[test]
    fn a_revise_raises_the_effort_one_step_up_to_xhigh() {
        let raised =
            ActorLaunch::default_of(ActorRole::RuntimePlanner).escalated(REVISE_ESCALATION);
        assert_eq!(raised.arguments(), Some((OPUS, "high")));
        assert_eq!(raised.escalated_from.as_deref(), Some(MEDIUM));
        assert_eq!(raised.escalation_reason.as_deref(), Some(REVISE_ESCALATION));
        assert_eq!(raised.source, LaunchSource::ReviseEscalation);
        let mut models = RoleModels::default();
        models.entry(ActorRole::RuntimePlanner).effort = Some("high".into());
        models.entry(ActorRole::RuntimePlanner).model = Some("claude-sonnet-5".into());
        let raised = models
            .launch(ActorRole::RuntimePlanner)
            .escalated(REVISE_ESCALATION);
        assert_eq!(raised.arguments(), Some(("claude-sonnet-5", TOP_EFFORT)));
        let again = raised.escalated(REVISE_ESCALATION);
        assert_eq!(again.effort.as_deref(), Some(TOP_EFFORT));
        assert_eq!(again.escalated_from.as_deref(), Some(TOP_EFFORT));
        assert_eq!(raise("low"), MEDIUM);
        assert_eq!(raise("max"), "max");
    }

    #[test]
    fn a_recorded_launch_reads_back_and_a_missing_one_is_the_default() {
        let launch = ActorLaunch::default_of(ActorRole::Review).escalated("why");
        let payload = json!({"launch": launch.to_value()});
        assert_eq!(ActorLaunch::recorded(&payload, ActorRole::Review), launch);
        assert_eq!(
            ActorLaunch::recorded(&json!({}), ActorRole::Recovery),
            ActorLaunch::default_of(ActorRole::Recovery)
        );
        assert_eq!(
            ActorLaunch::recorded(&json!({"launch": 3}), ActorRole::Recovery),
            ActorLaunch::default_of(ActorRole::Recovery)
        );
    }

    #[test]
    fn efforts_are_checked() {
        for effort in EFFORTS {
            assert!(check_effort(effort).is_ok());
        }
        assert!(check_effort("huge").is_err());
    }
}
