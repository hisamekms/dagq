//! Which agent a task's worker runs on and how (ADR-t813-1 decision 7,
//! ADR-t813-2 decision 1): the provider (`claude` / `codex`) and the mode
//! (`headless`: one non-interactive call per turn). A task that names
//! neither runs Claude headless (ADR-t1340-1). The interactive worker was
//! retired (ADR-t1433-2): `add` and `edit` refuse to name it
//! ([`refuse_interactive`]), and `interactive` stays only so that the tasks
//! and runs recorded with it before are read; claim and resume start them
//! headless. A run carries the provider and mode its task asked for.

use serde::{Deserialize, Serialize};

use super::{DomainError, Provider};

string_enum!(WorkerMode {
    Interactive => "interactive",
    Headless => "headless",
});

/// Why `add` and `edit` refuse `--interactive` (ADR-t1433-2), with what
/// takes the place of watching and stepping into the worker's session.
pub const INTERACTIVE_WORKER_RETIRED: &str = "--interactive is refused: the interactive worker was retired and every worker runs headless, one non-interactive call per turn; read a run's turns with `dagq run log RUN --follow`, and its questions come as asks you reply to with `dagq answer`";

/// Refuses a worker mode that `add` or `edit` gives when it is the retired
/// interactive one (ADR-t1433-2); `headless` and none are accepted. A task
/// or run already recorded as interactive is not checked here: it is read
/// as it was stored, and claim and resume start it headless.
pub fn refuse_interactive(mode: Option<WorkerMode>) -> Result<(), DomainError> {
    match mode {
        Some(WorkerMode::Interactive) => Err(DomainError::InteractiveWorkerRetired),
        Some(WorkerMode::Headless) | None => Ok(()),
    }
}

/// A worker's provider and mode, a pair the runtime can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Worker {
    pub provider: Provider,
    #[serde(rename = "worker_mode")]
    pub mode: WorkerMode,
}

impl Worker {
    /// Claude in its interactive session: what a task or run recorded
    /// before ADR-t1433-2 may still name; no command chooses it now.
    pub const CLAUDE_INTERACTIVE: Self = Self {
        provider: Provider::Claude,
        mode: WorkerMode::Interactive,
    };
    /// Claude, one non-interactive call per turn.
    pub const CLAUDE_HEADLESS: Self = Self {
        provider: Provider::Claude,
        mode: WorkerMode::Headless,
    };
    /// The worker of a task that names none: Claude, headless
    /// (ADR-t1340-1).
    pub const DEFAULT: Self = Self::CLAUDE_HEADLESS;
    /// Every worker a task may ask for.
    pub const ALL: [Self; 3] = [
        Self::CLAUDE_INTERACTIVE,
        Self::CLAUDE_HEADLESS,
        Self {
            provider: Provider::Codex,
            mode: WorkerMode::Headless,
        },
    ];

    /// `provider` in `mode`; refused for a mode the provider has none of
    /// (Codex interactive).
    pub fn new(provider: Provider, mode: WorkerMode) -> Result<Self, DomainError> {
        match (provider, mode) {
            (Provider::Codex, WorkerMode::Interactive) => {
                Err(DomainError::WorkerModeUnsupported { provider, mode })
            }
            _ => Ok(Self { provider, mode }),
        }
    }

    /// The mode of `provider` when none is given: headless for both, as
    /// Claude's default (ADR-t1340-1, amending ADR-t813-1 decision 7) and
    /// Codex's only mode.
    pub const fn default_mode(provider: Provider) -> WorkerMode {
        match provider {
            Provider::Claude | Provider::Codex => WorkerMode::Headless,
        }
    }

    /// The worker from what a task stores or a command gives: a missing
    /// provider is Claude, a missing mode the provider's default.
    pub fn resolve(
        provider: Option<Provider>,
        mode: Option<WorkerMode>,
    ) -> Result<Self, DomainError> {
        let provider = provider.unwrap_or(Provider::Claude);
        Self::new(provider, mode.unwrap_or(Self::default_mode(provider)))
    }

    /// This worker with the given fields replaced, as `edit` changes it: a
    /// new provider without a mode takes that provider's default mode.
    pub fn with(
        self,
        provider: Option<Provider>,
        mode: Option<WorkerMode>,
    ) -> Result<Self, DomainError> {
        match (provider, mode) {
            (None, None) => Ok(self),
            (Some(provider), mode) => {
                Self::new(provider, mode.unwrap_or(Self::default_mode(provider)))
            }
            (None, Some(mode)) => Self::new(self.provider, mode),
        }
    }
}

/// Why a candidate's claim is deferred for its worker (ADR-t813-2): this
/// supervisor has no adapters for its provider (`provider_unavailable`),
/// or has them but not for its mode (`mode_unavailable`). Recorded as the
/// `reason` of `claim_deferred`; the deferral ends when the supervisor can
/// run the worker or the task leaves the candidates.
pub const PROVIDER_UNAVAILABLE: &str = "provider_unavailable";
pub const MODE_UNAVAILABLE: &str = "mode_unavailable";
/// Why a candidate's claim is deferred when its provider cannot be used
/// and `[provider_fallback] workers` is off, so it does not start on the
/// other one (ADR-t1857-1): it waits for its provider's hold to end.
pub const FALLBACK_OFF: &str = "provider_fallback_off";

/// Why `worker` cannot be claimed by a supervisor that runs `supported`:
/// `None` when it can. A worker recorded on the interactive route runs
/// headless (ADR-t1433-2), so its provider's headless worker is the one
/// looked for.
pub fn unavailable(worker: Worker, supported: &[Worker]) -> Option<&'static str> {
    let worker = Worker {
        mode: WorkerMode::Headless,
        ..worker
    };
    if supported.contains(&worker) {
        None
    } else if supported.iter().any(|w| w.provider == worker.provider) {
        Some(MODE_UNAVAILABLE)
    } else {
        Some(PROVIDER_UNAVAILABLE)
    }
}

/// A provider's executable as a supervisor resolved it, for its
/// registration, `status` and `doctor`: where it resolved to (or the path
/// given when it did not), whether it was found, and the modes this binary
/// runs its workers in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCheck {
    pub provider: Provider,
    pub executable: String,
    pub found: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The worker modes this binary has adapters for.
    pub modes: Vec<WorkerMode>,
}

impl ProviderCheck {
    /// Whether a worker of this provider can start: its executable was
    /// found and the binary runs it in some mode.
    pub fn usable(&self) -> bool {
        self.found && !self.modes.is_empty()
    }

    /// Turn `provider` off among `checks` (`supervise --no-claude`): it keeps
    /// its executable but has no mode, with the error `provider_disabled`, so
    /// no worker of it starts.
    pub fn disable(checks: &mut [ProviderCheck], provider: Provider) {
        for check in checks.iter_mut().filter(|check| check.provider == provider) {
            check.modes.clear();
            check.error = Some("provider_disabled".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--no-claude` leaves Claude's executable but no mode, so it is not
    /// usable, and leaves Codex alone.
    #[test]
    fn a_disabled_provider_keeps_its_executable_and_runs_no_worker() {
        let check = |provider| ProviderCheck {
            provider,
            executable: "/bin/x".into(),
            found: true,
            error: None,
            modes: vec![WorkerMode::Headless],
        };
        let mut checks = vec![check(Provider::Claude), check(Provider::Codex)];
        ProviderCheck::disable(&mut checks, Provider::Claude);
        assert!(!checks[0].usable());
        assert_eq!(checks[0].executable, "/bin/x");
        assert_eq!(checks[0].error.as_deref(), Some("provider_disabled"));
        assert_eq!(checks[1], check(Provider::Codex));
    }

    #[test]
    fn a_task_without_a_worker_runs_claude_headless() {
        assert_eq!(
            Worker::resolve(None, None).unwrap(),
            Worker::CLAUDE_HEADLESS
        );
        assert_eq!(
            Worker::resolve(None, Some(WorkerMode::Interactive)).unwrap(),
            Worker::CLAUDE_INTERACTIVE
        );
        let codex = Worker::resolve(Some(Provider::Codex), None).unwrap();
        assert_eq!(codex.mode, WorkerMode::Headless);
        let headless = Worker::resolve(None, Some(WorkerMode::Headless)).unwrap();
        assert_eq!(headless.provider, Provider::Claude);
        assert!(matches!(
            Worker::resolve(Some(Provider::Codex), Some(WorkerMode::Interactive)),
            Err(DomainError::WorkerModeUnsupported { .. })
        ));
    }

    #[test]
    fn an_edit_replaces_the_fields_given() {
        let worker = Worker::CLAUDE_INTERACTIVE;
        assert_eq!(worker.with(None, None).unwrap(), worker);
        let codex = worker.with(Some(Provider::Codex), None).unwrap();
        assert_eq!(codex, Worker::ALL[2]);
        assert!(codex.with(None, Some(WorkerMode::Interactive)).is_err());
        assert_eq!(
            codex.with(Some(Provider::Claude), None).unwrap(),
            Worker::CLAUDE_HEADLESS
        );
        assert_eq!(
            worker.with(None, Some(WorkerMode::Headless)).unwrap(),
            Worker::ALL[1]
        );
    }

    /// ADR-t1433-2: `--interactive` is refused with the reason and what
    /// replaces it; `--headless` and no mode are accepted.
    #[test]
    fn the_interactive_mode_is_refused_with_its_replacement() {
        let refused = refuse_interactive(Some(WorkerMode::Interactive)).unwrap_err();
        assert!(matches!(refused, DomainError::InteractiveWorkerRetired));
        let reason = refused.to_string();
        for part in ["--interactive", "retired", "run log", "answer"] {
            assert!(reason.contains(part), "{part} in {reason}");
        }
        assert!(refuse_interactive(Some(WorkerMode::Headless)).is_ok());
        assert!(refuse_interactive(None).is_ok());
    }

    #[test]
    fn a_worker_is_unavailable_by_provider_or_by_mode() {
        let headless = [Worker::CLAUDE_HEADLESS];
        assert_eq!(unavailable(Worker::CLAUDE_HEADLESS, &headless), None);
        // A worker recorded on the interactive route runs headless.
        assert_eq!(unavailable(Worker::CLAUDE_INTERACTIVE, &headless), None);
        assert_eq!(
            unavailable(Worker::ALL[2], &headless),
            Some(PROVIDER_UNAVAILABLE)
        );
        let interactive = [Worker::CLAUDE_INTERACTIVE];
        assert_eq!(
            unavailable(Worker::CLAUDE_HEADLESS, &interactive),
            Some(MODE_UNAVAILABLE)
        );
        assert_eq!(
            unavailable(Worker::CLAUDE_INTERACTIVE, &interactive),
            Some(MODE_UNAVAILABLE)
        );
    }

    #[test]
    fn a_provider_is_usable_when_found_with_a_mode() {
        let mut check = ProviderCheck {
            provider: Provider::Codex,
            executable: "codex".into(),
            found: true,
            error: None,
            modes: Vec::new(),
        };
        assert!(!check.usable());
        check.modes.push(WorkerMode::Headless);
        assert!(check.usable());
        check.found = false;
        assert!(!check.usable());
    }
}
