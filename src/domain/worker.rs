//! Which agent a task's worker runs on and how (ADR-t813-1 decision 7,
//! ADR-t813-2 decision 1): the provider (`claude` / `codex`) and the mode
//! (`interactive`: the agent's own session in the cmux terminal;
//! `headless`: one non-interactive call per turn). A task that names
//! neither runs Claude headless (ADR-t1340-1, amending ADR-t813-1
//! decision 7); Claude runs interactively only when the task names that
//! mode, and Codex runs headless only. A run carries the provider and mode
//! its task asked for.

use serde::{Deserialize, Serialize};

use super::{DomainError, Provider};

string_enum!(WorkerMode {
    Interactive => "interactive",
    Headless => "headless",
});

/// A worker's provider and mode, a pair the runtime can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Worker {
    pub provider: Provider,
    #[serde(rename = "worker_mode")]
    pub mode: WorkerMode,
}

impl Worker {
    /// Claude in its interactive session: what a task runs on when it
    /// names the interactive mode.
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

/// Why `worker` cannot be claimed by a supervisor that runs `supported`:
/// `None` when it can.
pub fn unavailable(worker: Worker, supported: &[Worker]) -> Option<&'static str> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_worker_is_unavailable_by_provider_or_by_mode() {
        let supported = [Worker::CLAUDE_INTERACTIVE];
        assert_eq!(unavailable(Worker::CLAUDE_INTERACTIVE, &supported), None);
        assert_eq!(
            unavailable(Worker::ALL[1], &supported),
            Some(MODE_UNAVAILABLE)
        );
        assert_eq!(
            unavailable(Worker::ALL[2], &supported),
            Some(PROVIDER_UNAVAILABLE)
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
