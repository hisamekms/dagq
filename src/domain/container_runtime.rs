//! Where the processes that run untrusted code run (goal 38): the worker
//! (with its resume) and the verification of `integrate` run on the host
//! or in a container ([`RuntimeConfig`], ADR-t2113-3 decision 1), and the
//! read-only headless jobs, the planners and the inbox never in a container
//! (decisions 2 and 3). A target set to the container waits, never on the
//! host, while the container runtime cannot be used (ADR-t2113-1 decision
//! 4): this is what the supervisor judges and records of it.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{DomainError, EventKind, Provider};

string_enum!(Runtime {
    Host => "host",
    Container => "container",
});

impl Default for Runtime {
    /// The host: a `dagq.toml` that names no runtime runs as before.
    fn default() -> Self {
        Self::Host
    }
}

string_enum!(RuntimeTarget {
    Worker => "worker",
    Verification => "verification",
});

impl RuntimeTarget {
    pub const ALL: [Self; 2] = [Self::Worker, Self::Verification];

    /// What the target is, for a message.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Worker => "the worker and its resume",
            Self::Verification => "the verification of integrate",
        }
    }
}

/// The key of `[roles.<role>]` that names where the role runs.
pub const RUNTIME_KEY: &str = "runtime";

/// Where each target runs: `runtime = "host"` or `runtime = "container"`
/// in `[roles.worker]` (the worker and its resume) and in
/// `[roles.verification]` (the verification commands `integrate` runs) of
/// `dagq.toml`; a table or key left out is `"host"`. `runtime` is the only
/// key of these two tables. The other `[roles.<role>]` (the headless jobs
/// and the planners) take `runtime = "host"` only: a container there is
/// refused when the file is read ([`refuse_container`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub worker: Runtime,
    pub verification: Runtime,
}

impl RuntimeConfig {
    pub const fn of(&self, target: RuntimeTarget) -> Runtime {
        match target {
            RuntimeTarget::Worker => self.worker,
            RuntimeTarget::Verification => self.verification,
        }
    }

    pub fn set(&mut self, target: RuntimeTarget, runtime: Runtime) {
        match target {
            RuntimeTarget::Worker => self.worker = runtime,
            RuntimeTarget::Verification => self.verification = runtime,
        }
    }

    /// The targets set to the container.
    pub fn containers(&self) -> Vec<RuntimeTarget> {
        RuntimeTarget::ALL
            .into_iter()
            .filter(|target| self.of(*target) == Runtime::Container)
            .collect()
    }
}

/// Why a container is refused to `role`, a role other than the targets.
pub fn refuse_container(role: &str) -> String {
    format!(
        "runtime \"container\" is only for [roles.worker] and [roles.verification]; \
         {role} does not run untrusted code and stays on the host"
    )
}

/// Whether the container runtime can run a target now; `Unavailable` says
/// why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available,
    Unavailable(String),
}

/// Why a candidate's claim is deferred when the worker's runtime is the
/// container and it cannot be used (the `reason` of `claim_deferred`).
pub const CONTAINER_UNAVAILABLE: &str = "container_unavailable";
/// Why a candidate's claim is deferred when the worker's runtime is the
/// container and the task would run on Codex, which the container does not
/// reach (ADR-t2114-1 decision 2).
pub const CONTAINER_PROVIDER: &str = "container_provider_unsupported";

/// Whether `provider` can run a worker in the container: Claude only, as
/// the queue's broker reaches no OpenAI (ADR-t2114-1 decision 2).
pub const fn runs_in_container(provider: Provider) -> bool {
    matches!(provider, Provider::Claude)
}

/// Why a worker that would run on `provider` is not claimed or resumed,
/// given the worker's `runtime` and why the container cannot be used
/// (`unavailable`); `None` when it may run. The host runs as before.
pub fn worker_deferral(
    runtime: Runtime,
    unavailable: Option<&str>,
    provider: Provider,
) -> Option<&'static str> {
    match runtime {
        Runtime::Host => None,
        Runtime::Container if unavailable.is_some() => Some(CONTAINER_UNAVAILABLE),
        Runtime::Container if !runs_in_container(provider) => Some(CONTAINER_PROVIDER),
        Runtime::Container => None,
    }
}

/// Recorded on the queue when a target set to the container cannot be
/// used: an attention for the inbox.
pub const CONTAINER_RUNTIME_UNAVAILABLE: &str = EventKind::ContainerRuntimeUnavailable.as_str();
/// Recorded when every target set to the container can be used again, or
/// none is; the attention ends with it.
pub const CONTAINER_RUNTIME_AVAILABLE: &str = EventKind::ContainerRuntimeAvailable.as_str();
/// The two kinds, for reading the latest of them.
pub const CONTAINER_RUNTIME_KINDS: [&str; 2] =
    [CONTAINER_RUNTIME_UNAVAILABLE, CONTAINER_RUNTIME_AVAILABLE];

/// What one look at the targets set to the container found: each that
/// cannot be used, with why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerCheck {
    pub unavailable: Vec<(RuntimeTarget, String)>,
}

impl ContainerCheck {
    /// Ask `available` about each target `config` sets to the container.
    pub fn of(config: &RuntimeConfig, available: impl Fn(RuntimeTarget) -> Availability) -> Self {
        Self {
            unavailable: config
                .containers()
                .into_iter()
                .filter_map(|target| match available(target) {
                    Availability::Available => None,
                    Availability::Unavailable(why) => Some((target, why)),
                })
                .collect(),
        }
    }

    /// Why `target` cannot be used, if it cannot.
    pub fn why(&self, target: RuntimeTarget) -> Option<&str> {
        self.unavailable
            .iter()
            .find(|(of, _)| *of == target)
            .map(|(_, why)| why.as_str())
    }

    /// What the attention says: what waits, why, and what a person does.
    pub fn message(&self) -> Option<String> {
        if self.unavailable.is_empty() {
            return None;
        }
        let each = self
            .unavailable
            .iter()
            .map(|(target, why)| format!("{} ({}): {why}", target.as_str(), target.describe()))
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "dagq.toml sets runtime = \"container\" and the container runtime cannot be used: {each}; \
nothing of it runs on the host instead: its claims, resumes or verifications wait until the \
container can be used, or until a task sets the runtime back to \"host\""
        ))
    }

    /// The event to record when this look differs from the latest one on
    /// the queue (`last`, its kind of [`CONTAINER_RUNTIME_KINDS`] and its
    /// payload): unavailable after anything but the same targets with the
    /// same reasons unavailable, so a change of which target waits or why
    /// is told again; available after unavailable; else none. `targets`
    /// of the payload is each target that cannot be used with its `reason`.
    pub fn transition(&self, last: Option<(&str, &Value)>) -> Option<(EventKind, Value)> {
        let targets: Vec<Value> = self
            .unavailable
            .iter()
            .map(|(target, why)| json!({"target": target.as_str(), "reason": why}))
            .collect();
        let told = last
            .filter(|(kind, _)| *kind == CONTAINER_RUNTIME_UNAVAILABLE)
            .map(|(_, payload)| &payload["targets"]);
        match self.message() {
            Some(message) if told != Some(&Value::Array(targets.clone())) => Some((
                EventKind::ContainerRuntimeUnavailable,
                json!({"targets": targets, "message": message}),
            )),
            None if told.is_some() => Some((EventKind::ContainerRuntimeAvailable, json!({}))),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_runs_every_target_on_the_host() {
        let config = RuntimeConfig::default();
        for target in RuntimeTarget::ALL {
            assert_eq!(config.of(target), Runtime::Host);
        }
        assert!(config.containers().is_empty());
        let check = ContainerCheck::of(&config, |_| unreachable!("no container to ask"));
        assert_eq!(check, ContainerCheck::default());
        assert_eq!(check.message(), None);
        assert_eq!(check.transition(None), None);
        assert_eq!(
            worker_deferral(Runtime::Host, None, Provider::Codex),
            None,
            "the host runs Codex as before"
        );
    }

    #[test]
    fn a_container_that_cannot_be_used_holds_its_target_and_never_the_host() {
        let mut config = RuntimeConfig::default();
        config.set(RuntimeTarget::Worker, Runtime::Container);
        assert_eq!(config.containers(), [RuntimeTarget::Worker]);
        let check = ContainerCheck::of(&config, |target| {
            Availability::Unavailable(format!("no machine for {}", target.as_str()))
        });
        assert_eq!(
            check.why(RuntimeTarget::Worker),
            Some("no machine for worker")
        );
        assert_eq!(check.why(RuntimeTarget::Verification), None);
        let message = check.message().unwrap();
        assert!(message.contains("no machine for worker"), "{message}");
        assert!(message.contains("\"host\""), "{message}");
        let (kind, payload) = check.transition(None).unwrap();
        assert_eq!(kind, EventKind::ContainerRuntimeUnavailable);
        assert_eq!(payload["targets"][0]["target"], "worker");
        // Recorded once, ended once.
        let told = Some((CONTAINER_RUNTIME_UNAVAILABLE, &payload));
        assert_eq!(check.transition(told), None);
        let (kind, _) = ContainerCheck::default().transition(told).unwrap();
        assert_eq!(kind, EventKind::ContainerRuntimeAvailable);
        let ended = json!({});
        assert_eq!(
            ContainerCheck::default().transition(Some((CONTAINER_RUNTIME_AVAILABLE, &ended))),
            None
        );
        // Available: nothing to say.
        config.set(RuntimeTarget::Verification, Runtime::Container);
        let check = ContainerCheck::of(&config, |_| Availability::Available);
        assert_eq!(check.message(), None);
    }

    /// The worker back on the host and the verification on the container:
    /// another target waits, so the inbox is told again with it.
    #[test]
    fn a_change_of_the_waiting_target_or_its_reason_is_told_again() {
        let worker = RuntimeConfig {
            worker: Runtime::Container,
            verification: Runtime::Host,
        };
        let verification = RuntimeConfig {
            worker: Runtime::Host,
            verification: Runtime::Container,
        };
        let unusable = |why: &str| {
            let why = why.to_owned();
            move |_| Availability::Unavailable(why.clone())
        };
        let (_, told) = ContainerCheck::of(&worker, unusable("no machine"))
            .transition(None)
            .unwrap();
        let last = Some((CONTAINER_RUNTIME_UNAVAILABLE, &told));
        let (kind, payload) = ContainerCheck::of(&verification, unusable("no machine"))
            .transition(last)
            .unwrap();
        assert_eq!(kind, EventKind::ContainerRuntimeUnavailable);
        assert_eq!(
            payload["targets"],
            json!([{"target": "verification", "reason": "no machine"}])
        );
        assert!(
            payload["message"]
                .as_str()
                .unwrap()
                .contains("verification"),
            "{payload}"
        );
        // Another reason for the same target is told again too.
        let (kind, _) = ContainerCheck::of(&worker, unusable("no image"))
            .transition(last)
            .unwrap();
        assert_eq!(kind, EventKind::ContainerRuntimeUnavailable);
        assert_eq!(
            ContainerCheck::of(&worker, unusable("no machine")).transition(last),
            None
        );
    }

    #[test]
    fn a_container_worker_waits_for_the_container_and_never_runs_codex() {
        let why = Some("not built");
        assert_eq!(
            worker_deferral(Runtime::Container, why, Provider::Claude),
            Some(CONTAINER_UNAVAILABLE)
        );
        assert_eq!(
            worker_deferral(Runtime::Container, why, Provider::Codex),
            Some(CONTAINER_UNAVAILABLE)
        );
        assert_eq!(
            worker_deferral(Runtime::Container, None, Provider::Codex),
            Some(CONTAINER_PROVIDER)
        );
        assert_eq!(
            worker_deferral(Runtime::Container, None, Provider::Claude),
            None
        );
    }

    #[test]
    fn names_parse_and_a_container_elsewhere_is_refused_with_why() {
        assert_eq!("container".parse::<Runtime>().unwrap(), Runtime::Container);
        assert_eq!("host".parse::<Runtime>().unwrap(), Runtime::Host);
        assert!("podman".parse::<Runtime>().is_err());
        assert_eq!(
            "verification".parse::<RuntimeTarget>().unwrap(),
            RuntimeTarget::Verification
        );
        let refused = refuse_container("review");
        assert!(refused.contains("[roles.worker]"), "{refused}");
        assert!(refused.contains("review"), "{refused}");
    }
}
