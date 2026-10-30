//! Where the AI actors run and how far that holds them (goal 55,
//! ADR-t728-1 decision 6): each actor's [`ExecutorBackend`] and the
//! [`EnforcementLevel`] it gives, as `status` and `doctor` show them, and
//! the Claude Code `permissions.deny` rules made from the role's policy.
//!
//! An actor runs on the host: a process of this user, and the runtime's
//! checks are advisory, not a sandbox. The worker (with its resume) and the
//! verification of `integrate` may be set to the container instead (goal
//! 38, [`RuntimeConfig`] read from `dagq.toml`): `status` and `doctor` show
//! each target's runtime ([`runtime_executions`]). While the container
//! cannot be used, what is set to it waits and never runs on the host
//! instead (ADR-t2113-1 decision 4; the supervisor and `integrate` ask
//! their `ContainerRuntime` port); an actor the host's executor is asked
//! to start on the container is refused
//! ([`ExecutorBackend::ensure_implemented`]).
//!
//! The worker's row also names each provider its supervisors run it on,
//! once a Codex worker can run (ADR-t813-3 decision 7): the Claude worker
//! is advisory like the rest, the Codex worker `confined`, held by the
//! OS's sandbox for writes and signals but not isolated.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::domain::{
    ActorRole, Provider, TrustLevel,
    actor::{ACTOR_ID_ENV, ROLE_ENV, RUN_ID_ENV, TASK_ID_ENV},
    authorization::{Capability, grants},
    container_runtime::{Runtime, RuntimeConfig, RuntimeTarget},
    worker::{ProviderCheck, WorkerMode},
};

/// Where an executor runs its actors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutorBackend {
    /// Processes of this user on this host.
    #[default]
    Host,
    /// A container of the dagq machine per turn of a run (goal 38): only a
    /// worker's turns run there
    /// ([`PodmanActorExecutor`](super::run_container::PodmanActorExecutor)),
    /// and nothing set to it is started on the host instead.
    Container,
}

impl From<Runtime> for ExecutorBackend {
    fn from(runtime: Runtime) -> Self {
        match runtime {
            Runtime::Host => Self::Host,
            Runtime::Container => Self::Container,
        }
    }
}

impl ExecutorBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Container => "container",
        }
    }

    /// The backend a configuration names; an unknown name is refused.
    pub fn parse(name: &str) -> Result<Self> {
        match name.parse::<Runtime>() {
            Ok(runtime) => Ok(runtime.into()),
            Err(_) => bail!("unknown executor backend {name:?} (host or container)"),
        }
    }

    /// How far the backend holds an actor to its spec.
    pub const fn enforcement(self) -> EnforcementLevel {
        match self {
            Self::Host => EnforcementLevel::Advisory,
            Self::Container => EnforcementLevel::Sandbox,
        }
    }

    /// Refuse a backend the host's executor cannot start an actor on (fail
    /// closed): the actor is not started, on it or on the host. Only a
    /// worker's turns run in the container, started there by the run's
    /// session wrapper.
    pub fn ensure_implemented(self) -> Result<()> {
        match self {
            Self::Host => Ok(()),
            Self::Container => bail!(
                "the host's executor does not start an actor set to the container \
                 (only a worker's turns run there, started by its session wrapper); \
                 dagq refuses to start it rather than run it on the host"
            ),
        }
    }
}

/// How far an executor holds its actors to their spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EnforcementLevel {
    /// Recorded and checked by the runtime's own code, not isolated: the
    /// process can do whatever this user can (ADR-t728-1 decision 6).
    Advisory,
    /// The OS stops the process's writes outside the places it may write
    /// and its signals to other processes (the Codex worker's
    /// workspace-write sandbox, ADR-t813-3 decision 7), but it runs as this
    /// user, reads broadly and reaches the network: not isolated.
    Confined,
    /// Held by an isolation the process cannot leave (a later backend).
    Sandbox,
}

impl EnforcementLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Advisory => "advisory",
            Self::Confined => "confined",
            Self::Sandbox => "sandbox",
        }
    }

    /// Whether it isolates the actor; [`Self::Confined`] does not.
    pub const fn is_sandbox(self) -> bool {
        matches!(self, Self::Sandbox)
    }

    /// How far a worker of `provider` on `backend` is held: on the host,
    /// Claude is advisory and Codex confined by its OS sandbox; a backend
    /// that isolates holds both.
    pub const fn of_worker(backend: ExecutorBackend, provider: Provider) -> Self {
        match (backend, provider) {
            (ExecutorBackend::Host, Provider::Codex) => Self::Confined,
            (backend, _) => backend.enforcement(),
        }
    }
}

/// The backend of each actor: `backend` for all and the overrides of
/// some roles. The default runs every actor on the host;
/// [`Self::of_runtimes`] puts the worker where `dagq.toml` sets it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionConfig {
    pub backend: ExecutorBackend,
    pub actors: Vec<(ActorRole, ExecutorBackend)>,
}

impl ExecutionConfig {
    /// The worker where `runtimes` sets it, every other actor on the host.
    pub fn of_runtimes(runtimes: &RuntimeConfig) -> Self {
        Self {
            backend: ExecutorBackend::Host,
            actors: vec![(ActorRole::Worker, runtimes.worker.into())],
        }
    }

    /// The backend `role` runs on: its override, else the default.
    pub fn backend_of(&self, role: ActorRole) -> ExecutorBackend {
        self.actors
            .iter()
            .find(|(of, _)| *of == role)
            .map_or(self.backend, |(_, backend)| *backend)
    }

    /// Each AI actor with its backend and enforcement, as `status` and
    /// `doctor` show them: a backend that is not built is shown as set,
    /// as nothing is started on it.
    /// `checks` are the providers the registered supervisors resolved:
    /// when one of them can run a Codex worker, the worker's row lists
    /// each usable provider with its own enforcement (`providers`);
    /// otherwise the row is as before, with no `providers`.
    pub fn profiles(&self, checks: &[ProviderCheck]) -> Result<Vec<ActorExecution>> {
        let workers = worker_providers(checks);
        ai_actor_roles()
            .map(|role| {
                let backend = self.backend_of(role);
                let enforcement = backend.enforcement();
                let providers = if role == ActorRole::Worker {
                    workers
                        .iter()
                        .map(|(provider, modes)| {
                            let enforcement = EnforcementLevel::of_worker(backend, *provider);
                            ProviderExecution {
                                provider: *provider,
                                modes: modes.clone(),
                                enforcement,
                                sandboxed: enforcement.is_sandbox(),
                            }
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                Ok(ActorExecution {
                    role,
                    backend,
                    enforcement,
                    sandboxed: enforcement.is_sandbox(),
                    providers,
                })
            })
            .collect()
    }
}

/// The providers a worker can run on across `checks`, each with the modes
/// any supervisor runs it in, in [`Provider`] order; empty unless one of
/// them can run a Codex worker, as the worker is then only Claude.
fn worker_providers(checks: &[ProviderCheck]) -> Vec<(Provider, Vec<WorkerMode>)> {
    let usable = |provider: Provider| {
        checks
            .iter()
            .any(|check| check.provider == provider && check.usable())
    };
    if !usable(Provider::Codex) {
        return Vec::new();
    }
    [Provider::Claude, Provider::Codex]
        .into_iter()
        .filter(|provider| usable(*provider))
        .map(|provider| {
            let modes = [WorkerMode::Interactive, WorkerMode::Headless]
                .into_iter()
                .filter(|mode| {
                    checks.iter().any(|check| {
                        check.provider == provider && check.found && check.modes.contains(mode)
                    })
                })
                .collect();
            (provider, modes)
        })
        .collect()
}

/// The roles the runtime starts as AI actors.
pub fn ai_actor_roles() -> impl Iterator<Item = ActorRole> {
    ActorRole::ALL
        .into_iter()
        .filter(|role| role.trust() == TrustLevel::UntrustedAgent)
}

/// How one actor runs. `enforcement` is the weakest hold on it; a worker
/// that some supervisor can run on Codex also has `providers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActorExecution {
    pub role: ActorRole,
    pub backend: ExecutorBackend,
    pub enforcement: EnforcementLevel,
    pub sandboxed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderExecution>,
}

/// How the worker runs on one provider (ADR-t813-3 decision 7): the modes
/// the supervisors run it in and how far that holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderExecution {
    pub provider: Provider,
    pub modes: Vec<WorkerMode>,
    pub enforcement: EnforcementLevel,
    pub sandboxed: bool,
}

/// The actors of the default configuration, as `status` and `doctor`
/// show them: every AI actor on the host, advisory; with `checks` (the
/// supervisors' providers) naming a usable Codex, the worker's Codex
/// provider confined.
pub fn actor_executions(checks: &[ProviderCheck]) -> Result<Vec<ActorExecution>> {
    ExecutionConfig::default().profiles(checks)
}

/// Where one target runs and how far that holds it, as `status` and
/// `doctor` show it: the host advisory, the container a sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeExecution {
    pub target: RuntimeTarget,
    pub runtime: Runtime,
    pub enforcement: EnforcementLevel,
    pub sandboxed: bool,
}

/// Each target of `config` ([`RuntimeTarget::ALL`]) with its runtime and
/// enforcement.
pub fn runtime_executions(config: &RuntimeConfig) -> Vec<RuntimeExecution> {
    RuntimeTarget::ALL
        .into_iter()
        .map(|target| {
            let runtime = config.of(target);
            let enforcement = ExecutorBackend::from(runtime).enforcement();
            RuntimeExecution {
                target,
                runtime,
                enforcement,
                sandboxed: enforcement.is_sandbox(),
            }
        })
        .collect()
}

use Capability as C;

/// The `dagq` subcommands a role may be denied (state changes, `watch`
/// and `report`), and every capability a form of each may need
/// (`docs/design/authorization.md`). A role that has none of them gets
/// the command denied in its Claude settings. A command with a
/// form that only reads (`observe --history`, `graph` without `--out`) is
/// not listed, as a rule on its name would deny that form too; the CLI
/// refuses its other form. A test in `main.rs` checks that every
/// subcommand is here or on its list of the commands left out.
pub const DAGQ_COMMANDS: &[(&str, &[Capability])] = &[
    ("watch", &[C::QueueWatch]),
    ("report", &[C::ExportFile]),
    ("init", &[C::QueueAdmin]),
    ("migrate", &[C::QueueAdmin]),
    ("rebind", &[C::QueueAdmin]),
    ("install", &[C::BinaryInstall]),
    ("auto-update", &[C::BinaryInstall]),
    ("release-update", &[C::BinaryInstall]),
    ("up", &[C::ServiceLifecycle]),
    ("down", &[C::ServiceLifecycle]),
    ("inbox", &[C::ServiceLifecycle]),
    ("service start", &[C::ServiceLifecycle]),
    ("service stop", &[C::ServiceLifecycle]),
    ("service serve", &[C::ServiceLifecycle]),
    ("queue-broker serve", &[C::ServiceLifecycle]),
    ("queue-broker start", &[C::ServiceLifecycle]),
    ("queue-broker stop", &[C::ServiceLifecycle]),
    ("plan", &[C::PlannerOpen]),
    ("supervise", &[C::Supervise]),
    ("throughput-review", &[C::ObserveRun]),
    ("setup-review", &[C::ReviewSetup, C::ReviewSetupApprove]),
    ("integrate", &[C::IntegrationRequest]),
    ("recover", &[C::RunRecover]),
    ("run close-workspaces", &[C::WorkspaceCleanup]),
    ("review", &[C::PrepareReview]),
    ("session", &[C::SessionRun]),
    ("planner-session", &[C::SessionRun]),
    ("session-event", &[C::SessionRecord]),
    ("run screen", &[C::ScreenRead]),
    ("run log", &[C::ScreenRead]),
    ("run send", &[C::ScreenSend]),
    ("planner screen", &[C::ScreenRead]),
    ("planner log", &[C::ScreenRead]),
    ("planner send", &[C::ScreenSend]),
    ("planner request", &[C::PlannerRequest]),
    ("add", &[C::TaskWrite]),
    ("draft", &[C::TaskWrite]),
    ("edit", &[C::TaskWrite]),
    ("judge-follow-up", &[C::FollowUpJudge]),
    ("set-goal", &[C::TaskWrite]),
    ("set-paths", &[C::TaskWrite]),
    ("set-priority", &[C::TaskWrite]),
    ("revisit", &[C::TaskWrite]),
    ("dependency", &[C::TaskWrite]),
    ("cancel", &[C::TaskCancel]),
    ("ready", &[C::TaskReady, C::TaskReadyBypassReview]),
    ("submit", &[C::ProposalSubmit]),
    ("proposal withdraw", &[C::ProposalWithdraw]),
    ("goal add", &[C::GoalWrite]),
    ("goal edit", &[C::GoalWrite]),
    ("goal ready", &[C::GoalReady]),
    ("goal close", &[C::GoalClose]),
    ("goal review", &[C::GoalReviewRequest]),
    ("note", &[C::NoteWrite]),
    ("mark", &[C::MarkWrite]),
    ("finding record", &[C::FindingRecord]),
    ("finding resolve", &[C::FindingResolve]),
    ("finding dismiss", &[C::FindingDismiss]),
    ("ci failures", &[C::CiRead]),
    ("request add", &[C::RequestRecord]),
    (
        "agent eval",
        &[C::EvalRequest, C::EvalRequestHeldOut, C::EvalRerun],
    ),
    ("agent results", &[C::EvalRead]),
    ("agent result", &[C::EvalRead]),
    ("agent cases", &[C::EvalCases]),
    ("agent decide", &[C::EvalCases]),
    ("agent sets", &[C::EvalRead]),
    ("agent set", &[C::EvalRead]),
    ("request decline", &[C::RequestDecline]),
    ("replan request", &[C::ReplanRequest]),
    ("replan resolve", &[C::ReplanResolve]),
    ("replan propose", &[C::ReplanPropose]),
    ("hold", &[C::TaskHold]),
    ("release", &[C::TaskHold]),
    ("ask", &[C::AskOpen, C::FindingAsk, C::AskClose]),
    ("ask close", &[C::AskClose]),
    ("answer", &[C::AskAnswer]),
];

/// The variables that name the actor: an agent that rewrote them would
/// run the CLI as another role.
const IDENTITY_ENV: [&str; 4] = [ROLE_ENV, ACTOR_ID_ENV, RUN_ID_ENV, TASK_ID_ENV];

/// The Claude Code `permissions.deny` rules of `role`, made from its
/// policy ([`grants`]): each [`DAGQ_COMMANDS`] entry it has no capability
/// for (`Bash(dagq integrate:*)`), and setting, exporting or unsetting the
/// variables that name the actor. No role is refused a tool outside dagq:
/// dagq has no cmux session to guard (ADR-t2159-1 decision 5). A guardrail
/// against a mistake, not enforcement: the same command by a path, through
/// another shell or a script is not matched, and the CLI's own check stays
/// the one that refuses (ADR-t728-1 decision 6).
pub fn permission_deny(role: ActorRole) -> Vec<String> {
    deny_forms(role).iter().map(DenyForm::claude_rule).collect()
}

/// One shape of shell command a role's guardrail refuses, matched on the
/// head of the command only: Claude Code's `permissions.deny` rule
/// ([`permission_deny`]) and the Codex inbox's PreToolUse hook
/// ([`super::codex_inbox::guard`]) are both written from these, so the two
/// guardrails refuse the same heads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyForm {
    /// `dagq <command>` as a whole word sequence: the command alone or
    /// followed by whitespace (`Bash(dagq <command>:*)`).
    Command(String),
    /// Any command that starts with this text (`Bash(<text>*)`).
    Prefix(String),
}

impl DenyForm {
    /// The Claude Code rule of this form.
    pub fn claude_rule(&self) -> String {
        match self {
            Self::Command(command) => format!("Bash({command}:*)"),
            Self::Prefix(text) => format!("Bash({text}*)"),
        }
    }

    /// Whether the shell command `command`, its leading whitespace left
    /// out, starts with this form. A command wrapped in another (after
    /// `&&`, `;`, a pipe, inside `sh -c`) is not matched, as Claude Code's
    /// rule does not match it either.
    pub fn matches_head(&self, command: &str) -> bool {
        let command = command.trim_start();
        match self {
            Self::Command(form) => command
                .strip_prefix(form.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace)),
            Self::Prefix(text) => command.starts_with(text.as_str()),
        }
    }
}

/// The forms of `role`'s guardrail, from its policy ([`grants`]): each
/// [`DAGQ_COMMANDS`] entry it has no capability for, and setting,
/// exporting or unsetting the variables that name the actor.
pub fn deny_forms(role: ActorRole) -> Vec<DenyForm> {
    let granted = grants(role);
    let mut forms: Vec<DenyForm> = DAGQ_COMMANDS
        .iter()
        .filter(|(_, needs)| !needs.iter().any(|need| granted.contains(need)))
        .map(|(command, _)| DenyForm::Command(format!("dagq {command}")))
        .collect();
    for name in IDENTITY_ENV {
        forms.extend(
            [
                format!("{name}="),
                format!("export {name}"),
                format!("env {name}="),
                format!("env -u {name}"),
                format!("unset {name}"),
            ]
            .map(DenyForm::Prefix),
        );
    }
    forms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ai_actor_runs_on_the_host_advisory_by_default() {
        let profiles = actor_executions(&[]).unwrap();
        let roles: Vec<_> = profiles.iter().map(|p| p.role).collect();
        assert_eq!(roles, ai_actor_roles().collect::<Vec<_>>());
        assert!(roles.contains(&ActorRole::Worker));
        assert!(!roles.contains(&ActorRole::Supervisor));
        assert!(!roles.contains(&ActorRole::User));
        for profile in &profiles {
            assert_eq!(profile.backend, ExecutorBackend::Host);
            assert_eq!(profile.enforcement, EnforcementLevel::Advisory);
            assert!(!profile.sandboxed);
        }
        assert_eq!(
            serde_json::to_value(&profiles[0]).unwrap(),
            serde_json::json!({
                "role": "inbox",
                "backend": "host",
                "enforcement": "advisory",
                "sandboxed": false,
            })
        );
    }

    fn check(provider: Provider, found: bool, modes: &[WorkerMode]) -> ProviderCheck {
        ProviderCheck {
            provider,
            executable: provider.as_str().to_owned(),
            found,
            error: None,
            modes: modes.to_vec(),
        }
    }

    #[test]
    fn a_codex_worker_is_confined_and_claude_stays_advisory() {
        use WorkerMode::{Headless, Interactive};
        let both = [
            check(Provider::Claude, true, &[Interactive, Headless]),
            check(Provider::Codex, true, &[Headless]),
        ];
        let profiles = actor_executions(&both).unwrap();
        for profile in &profiles {
            assert_eq!(profile.enforcement, EnforcementLevel::Advisory);
            assert!(!profile.sandboxed);
            assert_eq!(
                profile.providers.is_empty(),
                profile.role != ActorRole::Worker,
                "{:?}",
                profile.role
            );
        }
        let worker = profiles
            .iter()
            .find(|p| p.role == ActorRole::Worker)
            .unwrap();
        assert_eq!(
            serde_json::to_value(worker).unwrap(),
            serde_json::json!({
                "role": "worker",
                "backend": "host",
                "enforcement": "advisory",
                "sandboxed": false,
                "providers": [
                    {"provider": "claude", "modes": ["interactive", "headless"],
                     "enforcement": "advisory", "sandboxed": false},
                    {"provider": "codex", "modes": ["headless"],
                     "enforcement": "confined", "sandboxed": false},
                ],
            })
        );
        // Neither advisory nor the isolation, and not isolated.
        assert_ne!(EnforcementLevel::Confined, EnforcementLevel::Advisory);
        assert_ne!(EnforcementLevel::Confined, EnforcementLevel::Sandbox);
        assert!(!EnforcementLevel::Confined.is_sandbox());
        // An isolating backend holds both providers.
        assert_eq!(
            EnforcementLevel::of_worker(ExecutorBackend::Container, Provider::Codex),
            EnforcementLevel::Sandbox
        );
        assert_eq!(
            EnforcementLevel::of_worker(ExecutorBackend::Host, Provider::Claude),
            EnforcementLevel::Advisory
        );
        // A Codex that is missing, or has no mode, leaves the rows as they
        // were without any supervisor.
        let without = actor_executions(&[]).unwrap();
        for unusable in [
            [
                check(Provider::Claude, true, &[Interactive, Headless]),
                check(Provider::Codex, false, &[Headless]),
            ],
            [
                check(Provider::Claude, true, &[Interactive]),
                check(Provider::Codex, true, &[]),
            ],
        ] {
            assert_eq!(actor_executions(&unusable).unwrap(), without);
        }
        // Across supervisors: the modes are the union, and a Claude that
        // none found is not listed.
        let merged = actor_executions(&[
            check(Provider::Claude, false, &[Interactive]),
            check(Provider::Codex, true, &[Headless]),
        ])
        .unwrap();
        let worker = merged.iter().find(|p| p.role == ActorRole::Worker).unwrap();
        assert_eq!(worker.providers.len(), 1);
        assert_eq!(worker.providers[0].provider, Provider::Codex);
        let merged = actor_executions(&[
            check(Provider::Claude, true, &[Interactive]),
            check(Provider::Claude, true, &[Headless]),
            check(Provider::Codex, true, &[Headless]),
        ])
        .unwrap();
        let worker = merged.iter().find(|p| p.role == ActorRole::Worker).unwrap();
        assert_eq!(worker.providers[0].modes, [Interactive, Headless]);
    }

    #[test]
    fn the_container_is_never_started_and_never_falls_back_to_the_host() {
        assert_eq!(
            ExecutorBackend::parse("container").unwrap(),
            ExecutorBackend::Container
        );
        assert!(ExecutorBackend::parse("podman").is_err());
        assert_eq!(
            ExecutorBackend::parse("host").unwrap(),
            ExecutorBackend::Host
        );
        assert!(ExecutorBackend::parse("docker").is_err());
        assert_eq!(
            ExecutorBackend::Container.enforcement(),
            EnforcementLevel::Sandbox
        );
        let error = ExecutorBackend::Container
            .ensure_implemented()
            .unwrap_err()
            .to_string();
        assert!(error.contains("refuses to start it"), "{error}");
        // `status` and `doctor` show a backend set to the container as it
        // is set, never as the host.
        let one = ExecutionConfig {
            backend: ExecutorBackend::Host,
            actors: vec![(ActorRole::Worker, ExecutorBackend::Container)],
        };
        assert_eq!(
            one.backend_of(ActorRole::Worker),
            ExecutorBackend::Container
        );
        assert_eq!(one.backend_of(ActorRole::Planner), ExecutorBackend::Host);
        let worker = one
            .profiles(&[])
            .unwrap()
            .into_iter()
            .find(|p| p.role == ActorRole::Worker)
            .unwrap();
        assert_eq!(worker.backend, ExecutorBackend::Container);
        assert_eq!(worker.enforcement, EnforcementLevel::Sandbox);
        assert!(worker.sandboxed);
        // Every target runs where `dagq.toml` sets it; the default is the
        // host.
        let runtimes = RuntimeConfig {
            worker: Runtime::Container,
            verification: Runtime::Host,
        };
        assert_eq!(ExecutionConfig::of_runtimes(&runtimes), one);
        assert_eq!(
            serde_json::to_value(runtime_executions(&runtimes)).unwrap(),
            serde_json::json!([
                {"target": "worker", "runtime": "container",
                 "enforcement": "sandbox", "sandboxed": true},
                {"target": "verification", "runtime": "host",
                 "enforcement": "advisory", "sandboxed": false},
            ])
        );
        assert!(
            runtime_executions(&RuntimeConfig::default())
                .iter()
                .all(|row| row.runtime == Runtime::Host && !row.sandboxed)
        );
        assert_eq!(
            EnforcementLevel::Sandbox.as_str(),
            "sandbox",
            "the level a sandbox would report"
        );
    }

    #[test]
    fn watch_and_report_rules_preserve_each_roles_access() {
        for role in ActorRole::ALL {
            let deny = permission_deny(role);
            let exports = matches!(
                role,
                ActorRole::User | ActorRole::Inbox | ActorRole::Planner | ActorRole::Supervisor
            );
            let watches = exports || role == ActorRole::Observer;
            assert_eq!(
                deny.contains(&"Bash(dagq watch:*)".to_owned()),
                !watches,
                "{role:?}"
            );
            assert_eq!(
                deny.contains(&"Bash(dagq report:*)".to_owned()),
                !exports,
                "{role:?}"
            );
            assert!(!deny.contains(&"Bash(dagq graph:*)".to_owned()), "{role:?}");
        }
    }

    #[test]
    fn the_deny_rules_follow_the_policy() {
        let worker = permission_deny(ActorRole::Worker);
        for denied in [
            "Bash(dagq integrate:*)",
            "Bash(dagq answer:*)",
            "Bash(dagq ready:*)",
            "Bash(dagq cancel:*)",
            "Bash(dagq recover:*)",
            "Bash(dagq goal close:*)",
            "Bash(dagq ask close:*)",
            "Bash(dagq install:*)",
            "Bash(DAGQ_ROLE=*)",
            "Bash(export DAGQ_ROLE*)",
            "Bash(unset DAGQ_RUN_ID*)",
        ] {
            assert!(worker.contains(&denied.to_owned()), "{denied}: {worker:?}");
        }
        // What the worker is granted stays open: its ask, its note; its
        // session is its wrapper's (ADR-t3102-1 decision 4).
        for open in ["Bash(dagq ask:*)", "Bash(dagq note:*)"] {
            assert!(!worker.contains(&open.to_owned()), "{open}");
        }
        for denied in ["Bash(dagq session:*)", "Bash(dagq session-event:*)"] {
            assert!(worker.contains(&denied.to_owned()), "{denied}");
        }
        let planner = permission_deny(ActorRole::Planner);
        for denied in [
            "Bash(dagq integrate:*)",
            "Bash(dagq answer:*)",
            "Bash(dagq ready:*)",
            "Bash(dagq recover:*)",
            "Bash(dagq supervise:*)",
        ] {
            assert!(planner.contains(&denied.to_owned()), "{denied}");
        }
        for open in [
            "Bash(dagq add:*)",
            "Bash(dagq submit:*)",
            "Bash(dagq goal close:*)",
            "Bash(dagq up:*)",
            "Bash(dagq install:*)",
        ] {
            assert!(!planner.contains(&open.to_owned()), "{open}");
        }
        // The review denies every state change it is not granted, and the
        // identity variables for every role.
        let review = permission_deny(ActorRole::ReviewJob);
        assert!(review.contains(&"Bash(dagq ask:*)".to_owned()));
        for role in ActorRole::ALL {
            assert!(permission_deny(role).contains(&"Bash(DAGQ_ROLE=*)".to_owned()));
        }
        // No role is refused cmux (ADR-t2159-1 decision 5): every rule is
        // a dagq command's or an identity variable's.
        for role in ActorRole::ALL {
            let deny = permission_deny(role);
            assert!(
                deny.iter().all(|rule| rule.starts_with("Bash(dagq ")
                    || IDENTITY_ENV.iter().any(|name| rule.contains(name))),
                "{role:?}: {deny:?}"
            );
            assert!(!deny.iter().any(|rule| rule.contains("cmux")), "{role:?}");
        }
        // Every rule a role gets is one the policy refuses.
        for role in ActorRole::ALL {
            for (command, needs) in DAGQ_COMMANDS {
                let rule = format!("Bash(dagq {command}:*)");
                assert_eq!(
                    permission_deny(role).contains(&rule),
                    needs.iter().all(|need| !grants(role).contains(need)),
                    "{role:?} {command}"
                );
            }
        }
    }
}
