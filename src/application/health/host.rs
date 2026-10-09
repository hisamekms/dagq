//! The supervisors' health and the host's part of `doctor`: the
//! registrations and lease holders, the providers they resolved, the roles'
//! models and a queue that refuses this binary (host operations).

use super::*;

/// A process that owns runs, as `status` and `doctor` report it: a resident
/// `supervise` through its registration (`registered`, with `parallel` and
/// `started_at`), or an `integrate` process through the lease it holds
/// (`registered: false`). `run_ids` are the leases carrying its token, and
/// they share its heartbeat. `stale` is a registration or lease that no
/// working process stands behind: a dead pid or a heartbeat older than
/// `HEARTBEAT_TIMEOUT_SECS`. `mode` is how `up` started it (`launchd`, or
/// `in_cmux` with the `workspace_id` it runs in, which only an earlier
/// binary started, ADR-t1433-4); a supervisor started by
/// hand and an `integrate` process have none. Nothing here is deleted
/// automatically.
#[derive(Debug, Clone, Serialize)]
pub struct SupervisorHealth {
    pub pid: u32,
    pub alive: bool,
    pub registered: bool,
    pub mode: Option<SupervisorMode>,
    pub workspace_id: Option<String>,
    /// The `dagq` version the registered process runs; `None` for a
    /// lease holder without a registration, or a registration older than
    /// the column (ADR-0014).
    pub binary_version: Option<String>,
    pub parallel: Option<u32>,
    /// Where `parallel` comes from: `flag`, `dagq.toml` or `default` (task
    /// 698); `None` for a lease holder or a registration of an older binary.
    pub parallel_source: Option<SettingSource>,
    /// The registration's `max_waiting` (ADR-0062 decision 7) and where it
    /// comes from, `None` as for `parallel_source`.
    pub max_waiting: Option<u32>,
    pub max_waiting_source: Option<SettingSource>,
    /// The registration's `runtime_planners` (ADR-0041 decision 12) and
    /// where it comes from (task 941), `None` as for `parallel_source`.
    pub runtime_planners: Option<u32>,
    pub runtime_planners_source: Option<SettingSource>,
    /// It updates its own binary on every landing that changes the runtime
    /// (ADR-0045 decision 17).
    pub auto_update: bool,
    /// Each worker provider's executable as the supervisor resolved it
    /// (`supervise --claude` / `--codex`), whether it was found and the
    /// worker modes it runs it in (ADR-t813-2); absent for a lease holder
    /// or a registration of an older binary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<crate::domain::worker::ProviderCheck>>,
    pub started_at: Option<i64>,
    pub heartbeat_at: i64,
    pub heartbeat_age_secs: i64,
    pub stale: bool,
    pub run_ids: Vec<RunId>,
}

impl SupervisorHealth {
    /// The supervisor in `doctor`'s default output, one line's worth.
    pub fn summary(&self) -> Value {
        json!({
            "pid": self.pid,
            "alive": self.alive,
            "registered": self.registered,
            "mode": self.mode,
            "workspace_id": self.workspace_id,
            "binary_version": self.binary_version,
            "parallel": self.parallel,
            "parallel_source": self.parallel_source,
            "max_waiting": self.max_waiting,
            "max_waiting_source": self.max_waiting_source,
            "runtime_planners": self.runtime_planners,
            "runtime_planners_source": self.runtime_planners_source,
            "auto_update": self.auto_update,
            "providers": self.providers,
            "heartbeat_age_secs": self.heartbeat_age_secs,
            "stale": self.stale,
            "run_ids": self.run_ids,
        })
    }
}

/// The providers every registered supervisor that is alive resolved, for
/// the worker's row of `actors`: a registration a dead supervisor left
/// behind does not show its Codex.
pub(super) fn provider_checks(
    registrations: &[SupervisorRegistration],
    control: &dyn ProcessControl,
) -> Vec<ProviderCheck> {
    registrations
        .iter()
        .filter(|registration| control.alive(registration.pid))
        .filter_map(|registration| registration.providers.as_ref())
        .flatten()
        .cloned()
        .collect()
}

/// Registered supervisors in registration order, then any other lease
/// holder (an `integrate` process) in lease order; leases join by token.
pub fn supervisors(
    registrations: &[SupervisorRegistration],
    leases: &[RunLease],
    now: i64,
    control: &dyn ProcessControl,
) -> Vec<SupervisorHealth> {
    let health = |pid: u32, heartbeat_at: i64, registration: Option<&SupervisorRegistration>| {
        let alive = control.alive(pid);
        let age = now - heartbeat_at;
        SupervisorHealth {
            pid,
            alive,
            registered: registration.is_some(),
            mode: registration.and_then(|r| r.mode),
            workspace_id: registration.and_then(|r| r.workspace_id.clone()),
            binary_version: registration.and_then(|r| r.binary_version.clone()),
            parallel: registration.map(|r| r.parallel),
            parallel_source: registration.and_then(|r| r.parallel_source),
            max_waiting: registration.and_then(|r| r.max_waiting),
            max_waiting_source: registration.and_then(|r| r.max_waiting_source),
            runtime_planners: registration.and_then(|r| r.runtime_planners),
            runtime_planners_source: registration.and_then(|r| r.runtime_planners_source),
            providers: registration.and_then(|r| r.providers.clone()),
            auto_update: registration.is_some_and(|r| r.auto_update),
            started_at: registration.map(|r| r.started_at),
            heartbeat_at,
            heartbeat_age_secs: age,
            stale: heartbeat_stale(alive, age),
            run_ids: Vec::new(),
        }
    };
    let mut entries: Vec<(&LeaseToken, SupervisorHealth)> = registrations
        .iter()
        .map(|r| (&r.token, health(r.pid, r.heartbeat_at, Some(r))))
        .collect();
    for lease in leases {
        let index = match entries.iter().position(|(token, _)| **token == lease.token) {
            Some(index) => index,
            None => {
                entries.push((&lease.token, health(lease.pid, lease.heartbeat_at, None)));
                entries.len() - 1
            }
        };
        let entry = &mut entries[index].1;
        entry.run_ids.push(lease.run_id.clone());
        if !entry.registered {
            // Every lease of one process carries the same heartbeat; the
            // freshest one stands for the process.
            let age = now - lease.heartbeat_at;
            if age < entry.heartbeat_age_secs {
                entry.heartbeat_at = lease.heartbeat_at;
                entry.heartbeat_age_secs = age;
                entry.stale = heartbeat_stale(entry.alive, age);
            }
        }
    }
    entries.into_iter().map(|(_, health)| health).collect()
}

/// The health of every registered supervisor, in registration order.
pub fn pulses(
    registrations: &[SupervisorRegistration],
    now: i64,
    control: &dyn ProcessControl,
) -> Vec<SupervisorPulse> {
    registrations
        .iter()
        .map(|r| SupervisorPulse::judge(r, control.alive(r.pid), now))
        .collect()
}

/// `doctor`'s report of a queue that refuses this binary: when it was
/// checked and why, beside the `schema` the caller adds.
pub fn refused(now: i64, error: &anyhow::Error) -> Value {
    json!({
        "checked_at": now,
        "error": format!("{error:#}"),
    })
}

/// `doctor`'s `roles`: the provider, model and effort each role other than
/// the worker's starts with, and where its provider comes from (`dagq.toml`
/// or `default`), from the `[roles.*]` `read` (ADR-t1063-1 decisions 1 and
/// 6), keyed by role. A file that could not be read adds `error`, and every
/// role starts as before meanwhile.
pub fn roles(read: Result<crate::domain::actor_model::RoleModels>) -> Value {
    use crate::domain::actor_model::{ModelRole, RoleModels};
    let (models, error) = match read {
        Ok(models) => (models, None),
        Err(error) => (RoleModels::default(), Some(format!("{error:#}"))),
    };
    let mut roles = serde_json::Map::new();
    for role in ModelRole::ALL {
        let (provider, source) = models.provider(role);
        let launch = models.launch(role);
        let mut entry = json!({
            "provider": provider,
            "source": source,
            "model": launch.model,
            "effort": launch.effort,
        });
        // The runtime's planners run headless only (ADR-t1433-2 decision
        // 3): there is no route to choose, and `route` of the table is
        // ignored.
        if role == ModelRole::RuntimePlanner {
            entry["route"] = json!(crate::domain::PlannerRoute::Headless);
        }
        roles.insert(role.as_str().to_owned(), entry);
    }
    if let Some(error) = error {
        roles.insert("error".to_owned(), error.into());
    }
    Value::Object(roles)
}
