//! Where the AI actors run and how far that holds them (goal 55,
//! ADR-t728-1 decision 6): each actor's [`ExecutorBackend`] and the
//! [`EnforcementLevel`] it gives, as `status` and `doctor` show them, and
//! the Claude Code `permissions.deny` rules made from the role's policy.
//!
//! `host` is the only backend: the actor is a process of this user, and
//! the runtime's checks are advisory, not a sandbox. `podman` is only a
//! reserved name: an actor configured for it is refused (fail closed) and
//! never started on the host instead. The configuration is the shape of a
//! later `[security] backend` and `[actors.<role>] backend`; nothing reads
//! it from a file yet, so every actor runs on the host.
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
    worker::{ProviderCheck, WorkerMode},
};

/// Where an executor runs its actors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutorBackend {
    /// Processes of this user on this host.
    #[default]
    Host,
    /// Containers (goal 38). Reserved: selecting it is refused until it
    /// exists, never run on the host instead.
    Podman,
}

impl ExecutorBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Podman => "podman",
        }
    }

    /// The backend a configuration names; an unknown name is refused.
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "host" => Ok(Self::Host),
            "podman" => Ok(Self::Podman),
            other => bail!("unknown executor backend {other:?} (host or podman)"),
        }
    }

    /// How far the backend holds an actor to its spec.
    pub const fn enforcement(self) -> EnforcementLevel {
        match self {
            Self::Host => EnforcementLevel::Advisory,
            Self::Podman => EnforcementLevel::Sandbox,
        }
    }

    /// Refuse a backend that is only a reserved name (fail closed): the
    /// actor is not started, on it or on the host.
    pub fn ensure_implemented(self) -> Result<()> {
        match self {
            Self::Host => Ok(()),
            Self::Podman => bail!(
                "the podman executor backend is reserved and not implemented; \
                 dagq refuses to start the actor rather than run it on the host"
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

/// The backend of each actor: `backend` for all (a later `[security]
/// backend`) and the overrides of some roles (a later `[actors.<role>]
/// backend`). The default runs every actor on the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionConfig {
    pub backend: ExecutorBackend,
    pub actors: Vec<(ActorRole, ExecutorBackend)>,
}

impl ExecutionConfig {
    /// The backend `role` runs on: its override, else the default.
    pub fn backend_of(&self, role: ActorRole) -> ExecutorBackend {
        self.actors
            .iter()
            .find(|(of, _)| *of == role)
            .map_or(self.backend, |(_, backend)| *backend)
    }

    /// Each AI actor with its backend and enforcement, as `status` and
    /// `doctor` show them. A backend that is not implemented is refused.
    /// `checks` are the providers the registered supervisors resolved:
    /// when one of them can run a Codex worker, the worker's row lists
    /// each usable provider with its own enforcement (`providers`);
    /// otherwise the row is as before, with no `providers`.
    pub fn profiles(&self, checks: &[ProviderCheck]) -> Result<Vec<ActorExecution>> {
        let workers = worker_providers(checks);
        ai_actor_roles()
            .map(|role| {
                let backend = self.backend_of(role);
                backend.ensure_implemented()?;
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
    ("broker start", &[C::ServiceLifecycle]),
    ("broker stop", &[C::ServiceLifecycle]),
    ("service start", &[C::ServiceLifecycle]),
    ("service stop", &[C::ServiceLifecycle]),
    ("service serve", &[C::ServiceLifecycle]),
    ("plan", &[C::PlannerOpen]),
    ("supervise", &[C::Supervise]),
    ("throughput-review", &[C::ObserveRun]),
    ("integrate", &[C::IntegrationRequest]),
    ("recover", &[C::RunRecover]),
    ("run close-workspaces", &[C::WorkspaceCleanup]),
    ("review", &[C::PrepareReview]),
    ("session", &[C::SessionRun]),
    ("planner-session", &[C::SessionRun]),
    ("session-event", &[C::SessionRecord]),
    ("run screen", &[C::ScreenRead]),
    ("run send", &[C::ScreenSend]),
    ("planner screen", &[C::ScreenRead]),
    ("planner send", &[C::ScreenSend]),
    ("add", &[C::TaskWrite]),
    ("draft", &[C::TaskWrite]),
    ("edit", &[C::TaskWrite]),
    ("set-goal", &[C::TaskWrite]),
    ("set-paths", &[C::TaskWrite]),
    ("set-priority", &[C::TaskWrite]),
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
    ("request add", &[C::RequestRecord]),
    ("request decline", &[C::RequestDecline]),
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
/// variables that name the actor. A guardrail against a mistake, not
/// enforcement: the same command by a path, through another shell or a
/// script is not matched, and the CLI's own check stays the one that
/// refuses (ADR-t728-1 decision 6).
pub fn permission_deny(role: ActorRole) -> Vec<String> {
    let granted = grants(role);
    let mut rules: Vec<String> = DAGQ_COMMANDS
        .iter()
        .filter(|(_, needs)| !needs.iter().any(|need| granted.contains(need)))
        .map(|(command, _)| format!("Bash(dagq {command}:*)"))
        .collect();
    for name in IDENTITY_ENV {
        rules.extend([
            format!("Bash({name}=*)"),
            format!("Bash(export {name}*)"),
            format!("Bash(env {name}=*)"),
            format!("Bash(env -u {name}*)"),
            format!("Bash(unset {name}*)"),
        ]);
    }
    rules
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
            EnforcementLevel::of_worker(ExecutorBackend::Podman, Provider::Codex),
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
    fn podman_is_refused_and_never_falls_back_to_the_host() {
        assert_eq!(
            ExecutorBackend::parse("podman").unwrap(),
            ExecutorBackend::Podman
        );
        assert_eq!(
            ExecutorBackend::parse("host").unwrap(),
            ExecutorBackend::Host
        );
        assert!(ExecutorBackend::parse("docker").is_err());
        assert_eq!(
            ExecutorBackend::Podman.enforcement(),
            EnforcementLevel::Sandbox
        );
        let error = ExecutorBackend::Podman
            .ensure_implemented()
            .unwrap_err()
            .to_string();
        assert!(error.contains("not implemented"), "{error}");
        // For every actor, or for one: the whole view is refused rather
        // than shown as the host.
        let all = ExecutionConfig {
            backend: ExecutorBackend::Podman,
            actors: Vec::new(),
        };
        assert!(all.profiles(&[]).is_err());
        let one = ExecutionConfig {
            backend: ExecutorBackend::Host,
            actors: vec![(ActorRole::Worker, ExecutorBackend::Podman)],
        };
        assert_eq!(one.backend_of(ActorRole::Worker), ExecutorBackend::Podman);
        assert_eq!(one.backend_of(ActorRole::Planner), ExecutorBackend::Host);
        assert!(one.profiles(&[]).is_err());
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
        // What the worker is granted stays open: its ask, its note, its
        // session.
        for open in [
            "Bash(dagq ask:*)",
            "Bash(dagq note:*)",
            "Bash(dagq session:*)",
            "Bash(dagq session-event:*)",
        ] {
            assert!(!worker.contains(&open.to_owned()), "{open}");
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
