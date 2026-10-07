//! The provider, model and effort of the sessions other than the worker's
//! (ADR-0079 decision 7 (b)(c), ADR-t1063-1): the plan review, the review, the recovery (triage)
//! job, the goal review, the observer, the throughput review, the runtime's
//! planners and a person's planner. Each
//! role takes `[roles.<role>]` of `dagq.toml` when it has one, and is
//! started as before (the provider's default, no model or effort given)
//! when it has none. A planner the runtime opens again for a plan review's
//! `revise` is raised one effort step (up to `xhigh`). What a session was
//! started with, and why, is its [`ActorLaunch`], recorded with the event
//! that starts it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    DomainError, Provider,
    provider_switch::SwitchReason,
    worker_model::{MEDIUM, OPUS},
};

string_enum!(ModelRole {
    PlanReview => "plan_review",
    Review => "review",
    Recovery => "recovery",
    GoalReview => "goal_review",
    Observer => "observer",
    ThroughputReview => "throughput_review",
    RuntimePlanner => "runtime_planner",
    Planner => "planner",
});

impl ModelRole {
    pub const ALL: [Self; 8] = [
        Self::PlanReview,
        Self::Review,
        Self::Recovery,
        Self::GoalReview,
        Self::Observer,
        Self::ThroughputReview,
        Self::RuntimePlanner,
        Self::Planner,
    ];
}

/// The provider a session other than the worker's runs on when its role
/// names none (ADR-t813-2 decision 1, ADR-t1063-1 decision 1).
pub const ROLE_PROVIDER: Provider = Provider::Claude;

/// The roles Codex has an implementation for (ADR-t1063-1 decisions 1
/// and 7): goal, run and plan reviews, the throughput review, the
/// observer (ADR-t1222-1, task 1223) and the recovery job (task 1225).
/// Claude runs every role.
pub const CODEX_ROLES: [ModelRole; 6] = [
    ModelRole::GoalReview,
    ModelRole::Review,
    ModelRole::PlanReview,
    ModelRole::ThroughputReview,
    ModelRole::Observer,
    ModelRole::Recovery,
];

/// Whether `provider` can run a session of `role`.
pub fn runs_on(role: ModelRole, provider: Provider) -> bool {
    match provider {
        Provider::Claude => true,
        Provider::Codex => CODEX_ROLES.contains(&role),
    }
}

/// The provider of a launch recorded before launches named one.
fn claude() -> Provider {
    Provider::Claude
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

/// `[roles.<role>]` of `dagq.toml`: a provider, a model and an effort, any
/// of which may be left out (the default's then). The model is a name of
/// the role's provider's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleModel {
    pub provider: Option<Provider>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// The key of `[roles.runtime_planner]` that named its planners' route
/// (ADR-t1394-2 decision 1). The runtime's planners run headless only
/// (ADR-t1433-2 decision 3): the key is accepted, whatever its value, and
/// ignored, so a `dagq.toml` that still has it keeps loading.
pub const ROUTE_KEY: &str = "route";

impl RoleModel {
    pub const KEYS: [&'static str; 4] = ["provider", "model", "effort", ROUTE_KEY];

    /// Check the table of `role` as a whole (ADR-t1063-1 decision 1): a
    /// provider with no implementation for the role is refused, and so is
    /// a Claude model given to Codex (a model is the provider's own name).
    pub fn check(&self, role: ModelRole) -> Result<(), String> {
        let Some(provider) = self.provider else {
            return Ok(());
        };
        if !runs_on(role, provider) {
            let roles: Vec<&str> = CODEX_ROLES.iter().map(|role| role.as_str()).collect();
            return Err(format!(
                "provider {} cannot run the {} role; Codex runs only {}",
                provider.as_str(),
                role.as_str(),
                roles.join(", ")
            ));
        }
        if provider == Provider::Codex
            && let Some(model) = self.model.as_deref().filter(|m| m.starts_with("claude"))
        {
            return Err(format!(
                "model {model} is Claude's; with provider codex name a model of Codex's, or none for Codex's default"
            ));
        }
        Ok(())
    }
}

/// Every `[roles.<role>]` of `dagq.toml`; none by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleModels {
    roles: Vec<(ModelRole, RoleModel)>,
    /// The value of `[roles.runtime_planner] route` as `dagq.toml` writes
    /// it, accepted and ignored (ADR-t1433-2 decision 3): only to warn that
    /// it chooses nothing.
    ignored_planner_route: Option<String>,
}

impl RoleModels {
    /// Note that `[roles.runtime_planner] route` says `value`, which is
    /// ignored ([`ROUTE_KEY`]).
    pub fn ignore_planner_route(&mut self, value: String) {
        self.ignored_planner_route = Some(value);
    }

    /// `[roles.runtime_planner] route` as `dagq.toml` writes it, `None`
    /// without the key: it chooses nothing, the runtime's planners running
    /// headless only (ADR-t1433-2 decision 3).
    pub fn ignored_planner_route(&self) -> Option<&str> {
        self.ignored_planner_route.as_deref()
    }

    /// The table of `role`, empty until a key is set.
    pub fn entry(&mut self, role: ModelRole) -> &mut RoleModel {
        let at = match self.roles.iter().position(|(known, _)| *known == role) {
            Some(at) => at,
            None => {
                self.roles.push((role, RoleModel::default()));
                self.roles.len() - 1
            }
        };
        &mut self.roles[at].1
    }

    pub fn get(&self, role: ModelRole) -> Option<&RoleModel> {
        self.roles
            .iter()
            .find(|(known, _)| *known == role)
            .map(|(_, model)| model)
    }

    /// Check every table ([`RoleModel::check`]), naming the first that
    /// fails.
    pub fn check(&self) -> Result<(), String> {
        for (role, table) in &self.roles {
            table
                .check(*role)
                .map_err(|error| format!("[roles.{}]: {error}", role.as_str()))?;
        }
        Ok(())
    }

    /// The provider a session of `role` runs on and where it comes from:
    /// its table's `provider` (`dagq.toml`), else Claude (`default`).
    pub fn provider(&self, role: ModelRole) -> (Provider, LaunchSource) {
        match self.get(role).and_then(|table| table.provider) {
            Some(provider) => (provider, LaunchSource::Config),
            None => (ROLE_PROVIDER, LaunchSource::Default),
        }
    }

    /// Whether a job of `role` names its provider (ADR-t1063-1 decision
    /// 4): only such a role has a failure of its provider classified as
    /// that provider being unusable and held, and may move to the other
    /// provider (when `[provider_fallback] jobs` lets it, ADR-t1857-1); one
    /// that names none runs and waits as before, on Claude only.
    pub fn switchable(&self, role: ModelRole) -> bool {
        self.get(role).is_some_and(|table| table.provider.is_some())
    }

    /// What a session of `role` starts with: its table's provider, model
    /// and effort, or, without a table, Claude's default with nothing
    /// given. On Claude a table gives the default (Opus 5.5, `medium`) for
    /// the model or effort it leaves out; on Codex a model left out is
    /// Codex's own default, and the effort `medium`.
    pub fn launch(&self, role: ModelRole) -> ActorLaunch {
        // A table that names only the ignored route gives the session
        // nothing.
        let table = self.get(role).filter(|table| {
            table.provider.is_some() || table.model.is_some() || table.effort.is_some()
        });
        match table {
            Some(table) => {
                let provider = table.provider.unwrap_or(ROLE_PROVIDER);
                let model = match provider {
                    Provider::Claude => {
                        Some(table.model.clone().unwrap_or_else(|| OPUS.to_owned()))
                    }
                    Provider::Codex => table.model.clone(),
                };
                ActorLaunch {
                    role,
                    provider,
                    model,
                    effort: Some(table.effort.clone().unwrap_or_else(|| MEDIUM.to_owned())),
                    source: LaunchSource::Config,
                    escalated_from: None,
                    escalation_reason: None,
                    switched_from: None,
                    switch_reason: None,
                }
            }
            None => ActorLaunch::default_of(role),
        }
    }
}

/// Where a headless job of a role starts (ADR-t1063-1 decisions 4 and 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobRoute {
    /// Start it with this launch (on its own provider, or moved to the
    /// other one).
    Start(ActorLaunch),
    /// Start nothing for now: its provider cannot be used for `reason`, and
    /// no other provider that runs the role can be either.
    Wait {
        provider: Provider,
        reason: SwitchReason,
    },
}

/// How a due headless job starts once its route is decided (ADR-t1063-1
/// decisions 1, 4 and 5, ADR-t1204-1): the throughput review's, the
/// observer's and the recovery job's ([`job_start_route`]). A value both
/// the observation and the execution contexts read, so it lives here with
/// [`JobRoute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStartRoute {
    /// On this launch; `true` when its `[roles.<role>]` names its provider.
    Start(ActorLaunch, bool),
    /// Under `--no-claude`, no provider can run it: the job records why,
    /// starting no agent.
    Unavailable(ActorLaunch, String),
}

/// Where the job that `launch` starts goes, given why each provider cannot
/// be used now (`unusable`, `None` when it can): on its own provider when
/// it can be used; else, when the role names its provider (`switchable`),
/// on the other provider when that one runs the role and can be used, its
/// launch saying from which and why; else it waits. With
/// `[provider_fallback] jobs` off (`fallback` false, ADR-t1857-1) a
/// provider that cannot be used ([`SwitchReason::unusable`]) is waited
/// for instead; `--no-claude` ([`SwitchReason::Disabled`]) still moves.
pub fn job_route(
    launch: &ActorLaunch,
    switchable: bool,
    fallback: bool,
    unusable: impl Fn(Provider) -> Option<SwitchReason>,
) -> JobRoute {
    let Some(reason) = unusable(launch.provider) else {
        return JobRoute::Start(launch.clone());
    };
    let other = launch.provider.other();
    let moves = switchable && (fallback || !reason.unusable());
    if moves && runs_on(launch.role, other) && unusable(other).is_none() {
        return JobRoute::Start(launch.clone().switched(other, reason));
    }
    JobRoute::Wait {
        provider: launch.provider,
        reason,
    }
}

/// What [`job_start_route`] does under `--no-claude` with a job whose role
/// names no provider, which runs on Claude only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnnamedWithoutClaude {
    /// It does not start (the throughput review, the observer;
    /// ADR-t1204-1 decision 2).
    Wait,
    /// It starts no agent and goes to a person told why (the recovery job).
    Unavailable,
}

/// Where the due headless job that `launch` starts goes, or, as `Err`,
/// that it waits, with the reason [`job_wait_text`] gives when a provider
/// cannot be used. `switchable` says that its role names its provider,
/// `no_claude` is `--no-claude`, `claude_held` that the queue's hold ask
/// holds Claude, `fallback` is `[provider_fallback] jobs` for the job
/// (ADR-t1857-1), `unnamed` what a role that names no provider does under
/// `--no-claude`, and `unusable` why each provider cannot be used now. A
/// role that names no provider runs on Claude as before: it waits while
/// the hold ask holds Claude. One that names its provider goes by
/// [`job_route`]; when that waits under `--no-claude` it is
/// [`JobStartRoute::Unavailable`] with the reason, as a Codex job never
/// moves to Claude then.
pub fn job_start_route(
    launch: ActorLaunch,
    switchable: bool,
    no_claude: bool,
    claude_held: bool,
    fallback: bool,
    unnamed: UnnamedWithoutClaude,
    unusable: impl Fn(Provider) -> Option<SwitchReason>,
) -> Result<JobStartRoute, Option<String>> {
    const DISABLED: &str = "provider_disabled: Claude is disabled by --no-claude";
    if !switchable {
        if no_claude {
            return match unnamed {
                UnnamedWithoutClaude::Wait => Err(None),
                UnnamedWithoutClaude::Unavailable => Ok(JobStartRoute::Unavailable(
                    launch,
                    format!("{DISABLED}; handle this role manually"),
                )),
            };
        }
        if claude_held {
            return Err(None);
        }
        return Ok(JobStartRoute::Start(launch, false));
    }
    match job_route(&launch, true, fallback, &unusable) {
        JobRoute::Start(launch) => Ok(JobStartRoute::Start(launch, true)),
        JobRoute::Wait { .. } if no_claude => {
            let codex = unusable(Provider::Codex).map_or("unknown", SwitchReason::as_str);
            Ok(JobStartRoute::Unavailable(
                launch,
                format!("{DISABLED} and codex cannot be used ({codex}); handle this role manually"),
            ))
        }
        JobRoute::Wait { provider, reason } => Err(Some(job_wait_text(provider, reason, fallback))),
    }
}

/// Why a job [`job_route`] sent to [`JobRoute::Wait`] waits, as the log
/// says it: with `[provider_fallback] jobs` off (`fallback` false) and
/// `provider` unusable for `reason` ([`SwitchReason::unusable`]) it waits
/// for `provider` whether or not the other one could run it
/// (ADR-t1857-1); otherwise neither provider can run it.
pub fn job_wait_text(provider: Provider, reason: SwitchReason, fallback: bool) -> String {
    if !fallback && reason.unusable() {
        format!(
            "{} cannot be used ({}); [provider_fallback] jobs is false, so it waits for {}",
            provider.as_str(),
            reason.as_str(),
            provider.as_str()
        )
    } else {
        format!(
            "{} cannot be used ({}), nor can the other provider",
            provider.as_str(),
            reason.as_str()
        )
    }
}

string_enum!(LaunchSource {
    Default => "default",
    Config => "dagq.toml",
    ReviseEscalation => "revise_escalation",
});

/// The provider, model and effort a session was started with, and where
/// the model and effort came from. `model` / `effort` are `None` when none
/// was given (the provider's default, which the transcript names once the
/// session closes). `provider` has the values of a run's
/// `requested_provider` / `actual_provider`; a launch recorded before it
/// was reads as `claude`, the only provider those sessions ran on. A job
/// moved off the provider its role names (ADR-t1063-1 decision 4) says
/// which (`switched_from`) and why (`switch_reason`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorLaunch {
    pub role: ModelRole,
    #[serde(default = "claude")]
    pub provider: Provider,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub source: LaunchSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalated_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switched_from: Option<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_reason: Option<SwitchReason>,
}

impl ActorLaunch {
    /// A session of `role` started as before: nothing given.
    pub fn default_of(role: ModelRole) -> Self {
        Self {
            role,
            provider: ROLE_PROVIDER,
            model: None,
            effort: None,
            source: LaunchSource::Default,
            escalated_from: None,
            escalation_reason: None,
            switched_from: None,
            switch_reason: None,
        }
    }

    /// This launch moved to `to` because its provider cannot be used for
    /// `reason`: started there with that provider's default, since a model
    /// (and its effort) is the provider's own name.
    pub fn switched(self, to: Provider, reason: SwitchReason) -> Self {
        Self {
            provider: to,
            switched_from: Some(self.provider),
            switch_reason: Some(reason),
            ..Self::default_of(self.role)
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
            provider: self.provider,
            model: Some(self.model.unwrap_or_else(|| OPUS.to_owned())),
            effort: Some(raise(&from).to_owned()),
            source: LaunchSource::ReviseEscalation,
            escalated_from: Some(from),
            escalation_reason: Some(reason.to_owned()),
            switched_from: self.switched_from,
            switch_reason: self.switch_reason,
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
    pub fn recorded(payload: &Value, role: ModelRole) -> Self {
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

/// Whether a job on its timer (the observer, the throughput review) that
/// ran on `provider` records `provider_unusable` when it could not use it:
/// a Codex one of a role that names its provider (`switchable`,
/// ADR-t1063-1 decision 4), and, with `[provider_fallback] jobs` off
/// (`fallback` false, ADR-t1857-1), a Claude one of such a role too, so
/// that the supervisor holds Claude and starts it again there once the
/// hold ends.
pub const fn records_unusable(provider: Provider, switchable: bool, fallback: bool) -> bool {
    switchable
        && match provider {
            Provider::Codex => true,
            Provider::Claude => !fallback,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A Codex job on its timer of a role that names its provider records
    /// that Codex cannot be used, on or off; a Claude one only with the
    /// fallback off; a role that names none never (ADR-t1857-1).
    #[test]
    fn a_timer_job_records_its_unusable_provider_as_its_role_and_fallback_say() {
        assert!(records_unusable(Provider::Claude, true, false));
        assert!(!records_unusable(Provider::Claude, true, true));
        assert!(!records_unusable(Provider::Claude, false, false));
        assert!(records_unusable(Provider::Codex, true, true));
        assert!(records_unusable(Provider::Codex, true, false));
        assert!(!records_unusable(Provider::Codex, false, false));
    }

    #[test]
    fn a_role_without_a_table_is_started_as_before() {
        let models = RoleModels::default();
        for role in ModelRole::ALL {
            let launch = models.launch(role);
            assert_eq!(launch, ActorLaunch::default_of(role));
            assert_eq!(launch.arguments(), None);
            assert_eq!(launch.to_value()["source"], "default");
        }
    }

    #[test]
    fn a_table_gives_its_model_and_effort_and_the_default_for_the_rest() {
        let mut models = RoleModels::default();
        models.entry(ModelRole::PlanReview).effort = Some("high".into());
        models.entry(ModelRole::Observer).model = Some("claude-sonnet-5".into());
        let plan = models.launch(ModelRole::PlanReview);
        assert_eq!(plan.arguments(), Some((OPUS, "high")));
        assert_eq!(plan.source, LaunchSource::Config);
        assert_eq!(
            models.launch(ModelRole::Observer).arguments(),
            Some(("claude-sonnet-5", MEDIUM))
        );
        assert_eq!(models.launch(ModelRole::Review).arguments(), None);
        assert_eq!(plan.to_value()["source"], "dagq.toml");
        assert!(plan.to_value().get("escalated_from").is_none());
    }

    #[test]
    fn a_revise_raises_the_effort_one_step_up_to_xhigh() {
        let raised =
            ActorLaunch::default_of(ModelRole::RuntimePlanner).escalated(REVISE_ESCALATION);
        assert_eq!(raised.arguments(), Some((OPUS, "high")));
        assert_eq!(raised.escalated_from.as_deref(), Some(MEDIUM));
        assert_eq!(raised.escalation_reason.as_deref(), Some(REVISE_ESCALATION));
        assert_eq!(raised.source, LaunchSource::ReviseEscalation);
        let mut models = RoleModels::default();
        models.entry(ModelRole::RuntimePlanner).effort = Some("high".into());
        models.entry(ModelRole::RuntimePlanner).model = Some("claude-sonnet-5".into());
        let raised = models
            .launch(ModelRole::RuntimePlanner)
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
        let launch = ActorLaunch::default_of(ModelRole::Review).escalated("why");
        let payload = json!({"launch": launch.to_value()});
        assert_eq!(ActorLaunch::recorded(&payload, ModelRole::Review), launch);
        assert_eq!(
            ActorLaunch::recorded(&json!({}), ModelRole::Recovery),
            ActorLaunch::default_of(ModelRole::Recovery)
        );
        assert_eq!(
            ActorLaunch::recorded(&json!({"launch": 3}), ModelRole::Recovery),
            ActorLaunch::default_of(ModelRole::Recovery)
        );
    }

    #[test]
    fn every_launch_records_its_provider_and_an_older_one_reads_as_claude() {
        let mut models = RoleModels::default();
        models.entry(ModelRole::GoalReview).effort = Some("high".into());
        for role in ModelRole::ALL {
            let launch = models.launch(role);
            assert_eq!(launch.provider, Provider::Claude);
            assert_eq!(launch.to_value()["provider"], "claude");
            assert_eq!(launch.clone().escalated("why").provider, Provider::Claude);
        }
        assert_eq!(
            models.launch(ModelRole::GoalReview).arguments(),
            Some((OPUS, "high"))
        );
        assert_eq!(ModelRole::GoalReview.as_str(), "goal_review");
        let older = json!({"launch": {"role": "review", "model": null, "effort": null, "source": "default"}});
        assert_eq!(
            ActorLaunch::recorded(&older, ModelRole::Review),
            ActorLaunch::default_of(ModelRole::Review)
        );
        let codex = json!({"launch": {"role": "goal_review", "provider": "codex", "model": null, "effort": null, "source": "default"}});
        assert_eq!(
            ActorLaunch::recorded(&codex, ModelRole::GoalReview).provider,
            Provider::Codex
        );
    }

    /// `[roles.<role>]`'s `provider` (ADR-t1063-1 decision 1, ADR-t1207-1):
    /// Codex runs the goal, run, plan and throughput reviews and the
    /// observer, with a model of its own or its
    /// default, and the effort `medium` unless given; a role that names no
    /// provider runs on Claude and does not move.
    #[test]
    fn a_role_table_names_its_provider() {
        let mut models = RoleModels::default();
        assert_eq!(
            models.provider(ModelRole::GoalReview),
            (Provider::Claude, LaunchSource::Default)
        );
        assert!(!models.switchable(ModelRole::GoalReview));
        models.entry(ModelRole::GoalReview).provider = Some(Provider::Codex);
        assert_eq!(
            models.provider(ModelRole::GoalReview),
            (Provider::Codex, LaunchSource::Config)
        );
        assert!(models.switchable(ModelRole::GoalReview));
        let launch = models.launch(ModelRole::GoalReview);
        assert_eq!(launch.provider, Provider::Codex);
        assert_eq!(launch.model, None);
        assert_eq!(launch.effort.as_deref(), Some(MEDIUM));
        assert_eq!(launch.to_value()["provider"], "codex");
        models.entry(ModelRole::GoalReview).model = Some("gpt-6-astra".into());
        assert_eq!(
            models.launch(ModelRole::GoalReview).model.as_deref(),
            Some("gpt-6-astra")
        );
        assert!(models.check().is_ok());
        // A Claude model given to Codex, and Codex for a role it cannot
        // run, are refused.
        models.entry(ModelRole::GoalReview).model = Some("claude-sonnet-5".into());
        let error = models.check().unwrap_err();
        assert!(error.contains("[roles.goal_review]"), "{error}");
        assert!(error.contains("Claude's"), "{error}");
        let mut review = RoleModels::default();
        review.entry(ModelRole::Review).provider = Some(Provider::Codex);
        review.entry(ModelRole::PlanReview).provider = Some(Provider::Codex);
        review.entry(ModelRole::ThroughputReview).provider = Some(Provider::Codex);
        review.entry(ModelRole::Observer).provider = Some(Provider::Codex);
        review.entry(ModelRole::Recovery).provider = Some(Provider::Codex);
        assert!(review.check().is_ok());
        review.entry(ModelRole::RuntimePlanner).provider = Some(Provider::Codex);
        let error = review.check().unwrap_err();
        assert!(
            error.contains("cannot run the runtime_planner role"),
            "{error}"
        );
        for role in ModelRole::ALL {
            assert!(runs_on(role, Provider::Claude));
            assert_eq!(
                runs_on(role, Provider::Codex),
                matches!(
                    role,
                    ModelRole::GoalReview
                        | ModelRole::Review
                        | ModelRole::PlanReview
                        | ModelRole::ThroughputReview
                        | ModelRole::Observer
                        | ModelRole::Recovery
                )
            );
        }
        // Claude named explicitly is valid for every role.
        let mut claude = RoleModels::default();
        claude.entry(ModelRole::Observer).provider = Some(Provider::Claude);
        assert!(claude.check().is_ok());
        assert_eq!(
            claude.launch(ModelRole::Observer).arguments(),
            Some((OPUS, MEDIUM))
        );
    }

    /// A job starts on its provider when it can be used, moves to the
    /// other one that runs its role when its table names the provider,
    /// and waits otherwise (ADR-t1063-1 decisions 4 and 5).
    #[test]
    fn a_job_moves_to_the_other_provider_or_waits() {
        let mut models = RoleModels::default();
        models.entry(ModelRole::GoalReview).provider = Some(Provider::Codex);
        models.entry(ModelRole::GoalReview).model = Some("gpt-6-astra".into());
        let codex = models.launch(ModelRole::GoalReview);
        let usable = |_: Provider| None;
        assert_eq!(
            job_route(&codex, true, true, usable),
            JobRoute::Start(codex.clone())
        );
        let no_codex = |provider: Provider| {
            (provider == Provider::Codex).then_some(SwitchReason::ExecutableMissing)
        };
        let JobRoute::Start(moved) = job_route(&codex, true, true, no_codex) else {
            panic!("moved to Claude");
        };
        assert_eq!(moved.provider, Provider::Claude);
        assert_eq!(moved.switched_from, Some(Provider::Codex));
        assert_eq!(moved.switch_reason, Some(SwitchReason::ExecutableMissing));
        // Codex's model is not given to Claude.
        assert_eq!(moved.arguments(), None);
        assert_eq!(moved.to_value()["switched_from"], "codex");
        assert_eq!(moved.to_value()["switch_reason"], "executable_missing");
        assert_eq!(
            ActorLaunch::recorded(&json!({"launch": moved.to_value()}), ModelRole::GoalReview),
            moved
        );
        let neither = |provider: Provider| {
            Some(match provider {
                Provider::Codex => SwitchReason::UsageLimit,
                Provider::Claude => SwitchReason::Authentication,
            })
        };
        assert_eq!(
            job_route(&codex, true, true, neither),
            JobRoute::Wait {
                provider: Provider::Codex,
                reason: SwitchReason::UsageLimit
            }
        );
        // A role that names no provider does not move.
        let claude = ActorLaunch::default_of(ModelRole::GoalReview);
        let no_claude =
            |provider: Provider| (provider == Provider::Claude).then_some(SwitchReason::UsageLimit);
        assert_eq!(
            job_route(&claude, false, true, no_claude),
            JobRoute::Wait {
                provider: Provider::Claude,
                reason: SwitchReason::UsageLimit
            }
        );
        let JobRoute::Start(moved) = job_route(&claude, true, true, no_claude) else {
            panic!("moved to Codex");
        };
        assert_eq!(moved.provider, Provider::Codex);
        // A run review whose `[roles.review]` names Claude moves to Codex
        // when Claude cannot be used, as the supervisor's review route does
        // (ADR-t1207-1), and so do a plan review, a throughput review, the
        // observer and a recovery job (task 1225); a planner, which Codex
        // does not run, waits.
        let review = ActorLaunch::default_of(ModelRole::Review);
        let JobRoute::Start(moved) = job_route(&review, true, true, no_claude) else {
            panic!("moved the review to Codex");
        };
        assert_eq!(moved.provider, Provider::Codex);
        let plan_review = ActorLaunch::default_of(ModelRole::PlanReview);
        let JobRoute::Start(moved) = job_route(&plan_review, true, true, no_claude) else {
            panic!("moved the plan review to Codex");
        };
        assert_eq!(moved.provider, Provider::Codex);
        let throughput = ActorLaunch::default_of(ModelRole::ThroughputReview);
        let JobRoute::Start(moved) = job_route(&throughput, true, true, no_claude) else {
            panic!("moved the throughput review to Codex");
        };
        assert_eq!(moved.provider, Provider::Codex);
        let observer = ActorLaunch::default_of(ModelRole::Observer);
        let JobRoute::Start(moved) = job_route(&observer, true, true, no_claude) else {
            panic!("moved the observer to Codex");
        };
        assert_eq!(moved.provider, Provider::Codex);
        let recovery = ActorLaunch::default_of(ModelRole::Recovery);
        let JobRoute::Start(moved) = job_route(&recovery, true, true, no_claude) else {
            panic!("moved the recovery job to Codex");
        };
        assert_eq!(moved.provider, Provider::Codex);
        let planner = ActorLaunch::default_of(ModelRole::RuntimePlanner);
        assert!(matches!(
            job_route(&planner, true, true, no_claude),
            JobRoute::Wait { .. }
        ));
    }

    /// A job that waits says why: with the fallback off, for its own
    /// provider (the other one may be usable); otherwise because neither
    /// provider can run it, as for `--no-claude` with the fallback off
    /// (ADR-t1857-1).
    #[test]
    fn a_waiting_job_says_whether_the_fallback_keeps_it_on_its_provider() {
        for provider in [Provider::Claude, Provider::Codex] {
            for reason in [
                SwitchReason::ExecutableMissing,
                SwitchReason::LaunchFailed,
                SwitchReason::Authentication,
                SwitchReason::UsageLimit,
            ] {
                let p = provider.as_str();
                let r = reason.as_str();
                assert_eq!(
                    job_wait_text(provider, reason, false),
                    format!(
                        "{p} cannot be used ({r}); [provider_fallback] jobs is false, so it waits for {p}"
                    )
                );
                assert_eq!(
                    job_wait_text(provider, reason, true),
                    format!("{p} cannot be used ({r}), nor can the other provider")
                );
            }
        }
        assert_eq!(
            job_wait_text(Provider::Claude, SwitchReason::Disabled, false),
            "claude cannot be used (provider_disabled), nor can the other provider"
        );
    }

    /// With `[provider_fallback] jobs` off a job whose role names its
    /// provider waits for that provider when it cannot be used, whichever
    /// provider and reason; `--no-claude` still moves a Claude role to
    /// Codex (ADR-t1857-1).
    #[test]
    fn with_the_fallback_off_a_job_waits_for_its_own_provider() {
        let mut models = RoleModels::default();
        models.entry(ModelRole::ThroughputReview).provider = Some(Provider::Codex);
        let codex = models.launch(ModelRole::ThroughputReview);
        let claude = ActorLaunch::default_of(ModelRole::GoalReview);
        for launch in [&codex, &claude] {
            for reason in [
                SwitchReason::ExecutableMissing,
                SwitchReason::LaunchFailed,
                SwitchReason::Authentication,
                SwitchReason::UsageLimit,
            ] {
                let own = |provider: Provider| (provider == launch.provider).then_some(reason);
                assert_eq!(
                    job_route(launch, true, false, own),
                    JobRoute::Wait {
                        provider: launch.provider,
                        reason
                    },
                    "{} {}",
                    launch.provider.as_str(),
                    reason.as_str()
                );
                // On, the same job moves.
                let JobRoute::Start(moved) = job_route(launch, true, true, own) else {
                    panic!("moved with the fallback on");
                };
                assert_eq!(moved.provider, launch.provider.other());
                // Usable, it starts on its own provider either way.
                assert_eq!(
                    job_route(launch, true, false, |_| None),
                    JobRoute::Start(launch.clone())
                );
            }
        }
        // `--no-claude` is no unusable provider: a Claude role moves.
        let disabled =
            |provider: Provider| (provider == Provider::Claude).then_some(SwitchReason::Disabled);
        let JobRoute::Start(moved) = job_route(&claude, true, false, disabled) else {
            panic!("moved under --no-claude with the fallback off");
        };
        assert_eq!(moved.provider, Provider::Codex);
        assert_eq!(moved.switch_reason, Some(SwitchReason::Disabled));
    }

    fn describe(route: Result<JobStartRoute, Option<String>>) -> String {
        match route {
            Err(None) => "wait".to_owned(),
            Err(Some(why)) => format!("wait: {why}"),
            Ok(JobStartRoute::Start(launch, switchable)) => format!(
                "start {} {switchable} {:?} {:?}",
                launch.provider.as_str(),
                launch.switched_from.map(Provider::as_str),
                launch.switch_reason.map(SwitchReason::as_str),
            ),
            Ok(JobStartRoute::Unavailable(launch, why)) => {
                format!("manual {} {why}", launch.provider.as_str())
            }
        }
    }

    /// A due job whose role names no provider runs on Claude and waits
    /// while the hold ask holds Claude; under `--no-claude` the throughput
    /// review and the observer wait and the recovery job goes to a person
    /// (ADR-t1204-1).
    #[test]
    fn a_job_whose_role_names_no_provider_runs_on_claude_or_waits_or_goes_to_a_person() {
        use UnnamedWithoutClaude::{Unavailable, Wait};
        let usable = |_: Provider| None;
        for role in [
            ModelRole::ThroughputReview,
            ModelRole::Observer,
            ModelRole::Recovery,
        ] {
            let launch = RoleModels::default().launch(role);
            for (unnamed, fallback) in [(Wait, true), (Wait, false), (Unavailable, true)] {
                let route = |no_claude, held| {
                    describe(job_start_route(
                        launch.clone(),
                        false,
                        no_claude,
                        held,
                        fallback,
                        unnamed,
                        usable,
                    ))
                };
                assert_eq!(route(false, false), "start claude false None None");
                assert_eq!(route(false, true), "wait");
                let disabled = if unnamed == Wait {
                    "wait"
                } else {
                    "manual claude provider_disabled: Claude is disabled by --no-claude; handle this role manually"
                };
                assert_eq!(route(true, false), disabled, "{}", role.as_str());
                assert_eq!(route(true, true), disabled, "{}", role.as_str());
            }
        }
    }

    /// A due job whose role names Codex starts there whatever holds
    /// Claude, moves to Claude when Codex cannot be used and the fallback
    /// lets it, waits when neither can run it, and under `--no-claude`
    /// never moves to Claude but records why, for the throughput review,
    /// the observer and the recovery job alike (ADR-t1063-1 decisions 4
    /// and 5, ADR-t1204-1, ADR-t1857-1).
    #[test]
    fn a_job_whose_role_names_codex_starts_where_it_can_waits_or_says_why() {
        use UnnamedWithoutClaude::{Unavailable, Wait};
        let usable = |_: Provider| None;
        let claude_only = |provider: Provider| {
            (provider == Provider::Codex).then_some(SwitchReason::ExecutableMissing)
        };
        let held = |provider: Provider| {
            Some(match provider {
                Provider::Claude => SwitchReason::UsageLimit,
                Provider::Codex => SwitchReason::Authentication,
            })
        };
        let neither = |provider: Provider| {
            Some(match provider {
                Provider::Claude => SwitchReason::Disabled,
                Provider::Codex => SwitchReason::Authentication,
            })
        };
        for (role, unnamed) in [
            (ModelRole::ThroughputReview, Wait),
            (ModelRole::Observer, Wait),
            (ModelRole::Recovery, Unavailable),
        ] {
            let mut models = RoleModels::default();
            models.entry(role).provider = Some(Provider::Codex);
            assert!(models.switchable(role));
            let codex = models.launch(role);
            let route =
                |no_claude,
                 claude_held,
                 fallback,
                 unusable: &dyn Fn(Provider) -> Option<SwitchReason>| {
                    describe(job_start_route(
                        codex.clone(),
                        true,
                        no_claude,
                        claude_held,
                        fallback,
                        unnamed,
                        unusable,
                    ))
                };
            for fallback in [true, false] {
                // Claude's hold ask does not hold a Codex job.
                assert_eq!(
                    route(false, true, fallback, &usable),
                    "start codex true None None"
                );
                assert_eq!(
                    route(false, false, fallback, &held),
                    format!(
                        "wait: {}",
                        job_wait_text(Provider::Codex, SwitchReason::Authentication, fallback)
                    )
                );
                assert_eq!(
                    route(true, false, fallback, &usable),
                    "start codex true None None"
                );
                assert_eq!(
                    route(true, false, fallback, &neither),
                    "manual codex provider_disabled: Claude is disabled by --no-claude and codex cannot be used (authentication); handle this role manually"
                );
            }
            assert_eq!(
                route(false, false, true, &claude_only),
                "start claude true Some(\"codex\") Some(\"executable_missing\")"
            );
            assert_eq!(
                route(false, false, false, &claude_only),
                "wait: codex cannot be used (executable_missing); [provider_fallback] jobs is false, so it waits for codex"
            );
        }
    }

    #[test]
    fn efforts_are_checked() {
        for effort in EFFORTS {
            assert!(check_effort(effort).is_ok());
        }
        assert!(check_effort("huge").is_err());
    }
}
