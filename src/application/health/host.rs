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
/// `HEARTBEAT_TIMEOUT_SECS`. `mode` is how it was started (`supervise --mode`: `launchd`, or
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

/// Two or more supervisors serving the queue at once, as `doctor` and
/// `status` report it (`duplicate_supervisors`). A task is not run twice:
/// the queue makes each claim alone (ADR-0054 decisions 3 and 5). But
/// `parallel` bounds each supervisor's own runs, so the queue runs up to
/// the sum of their slots, and the host carries that load. It is a
/// `warning`: nothing stops until a person stops the extra one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DuplicateSupervisors {
    pub severity: &'static str,
    /// The live supervisors ([`duplicate_supervisors`] counts them), a
    /// handoff's pair as its successor alone.
    pub supervisors: Vec<DuplicateSupervisor>,
    /// The queue's LaunchAgents launchd has loaded, under any label.
    pub agents: Vec<LoadedAgent>,
    /// What a person does: `down --wait`, which unloads the queue's agents
    /// of every label and stops every registered supervisor, then `up`
    /// (`up` alone reuses a supervisor of its own build); with two agents
    /// loaded, the `launchctl bootout` of each label and the removal of its
    /// plist in case one stays after `down`, each label and path quoted for
    /// the shell, since a plist's `Label` may hold any character.
    pub fix: Vec<String>,
}

/// One supervisor of [`DuplicateSupervisors`]: its registration's token,
/// pid, `mode` (null for one started by hand) and build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DuplicateSupervisor {
    pub token: LeaseToken,
    pub pid: u32,
    pub mode: Option<SupervisorMode>,
    pub binary_version: Option<String>,
    pub started_at: i64,
}

/// One loaded agent of [`DuplicateSupervisors`]: its label, plist and the
/// pid launchd runs under it (null when unknown).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoadedAgent {
    pub label: String,
    pub plist: std::path::PathBuf,
    pub pid: Option<u32>,
}

/// Whether the queue has two supervisors at once, the same judgement for
/// `doctor` and `status`: (a) two or more live registrations (the pid
/// alive and the heartbeat fresh; a silent one is reported as stale
/// already) that are not one handoff's pair, or (b) two or more of the
/// queue's LaunchAgents loaded (`agents`, from
/// [`queue_agents`](crate::application::lifecycle::queue_agents): its own
/// label and any other whose `--db` is its database), each of which
/// launchd keeps a supervisor alive for. A handoff's pair is the row of
/// the supervisor that took it and the row its exec made again under the
/// same pid ([`registered_again`](crate::application::lifecycle::registered_again),
/// the successor `up` waits for): one process for a moment, not two, so
/// it is counted once and does not warn. `None` when neither holds.
pub fn duplicate_supervisors(
    registrations: &[SupervisorRegistration],
    agents: &[crate::application::lifecycle::QueueAgent],
    now: i64,
    control: &dyn ProcessControl,
) -> Option<DuplicateSupervisors> {
    use crate::application::lifecycle::registered_again;
    let live: Vec<&SupervisorRegistration> = registrations
        .iter()
        .filter(|r| !heartbeat_stale(control.alive(r.pid), now - r.heartbeat_at))
        .collect();
    // A row another live row of its pid took over from is that one.
    let supervisors: Vec<DuplicateSupervisor> = live
        .iter()
        .filter(|r| {
            !live.iter().any(|other| {
                other.token != r.token
                    && registered_again(other, r.pid, r.started_at)
                    && (other.started_at, other.token.as_str()) > (r.started_at, r.token.as_str())
            })
        })
        .map(|r| DuplicateSupervisor {
            token: r.token.clone(),
            pid: r.pid,
            mode: r.mode,
            binary_version: r.binary_version.clone(),
            started_at: r.started_at,
        })
        .collect();
    let agents: Vec<LoadedAgent> = agents
        .iter()
        .filter(|agent| agent.state.loaded)
        .map(|agent| LoadedAgent {
            label: agent.label.clone(),
            plist: agent.plist.clone(),
            pid: agent.state.pid,
        })
        .collect();
    if supervisors.len() < 2 && agents.len() < 2 {
        return None;
    }
    let mut fix = vec![
        "`dagq down --wait` stops every supervisor of the queue and unloads its agents of \
         every label, then `dagq up` starts one"
            .to_owned(),
    ];
    if agents.len() >= 2 {
        fix.extend(agents.iter().map(|agent| {
            let quote = crate::application::naming::shell_quote;
            format!(
                "if {} stays loaded after `down`: `launchctl bootout \"gui/$(id -u)/\"{}` and \
                 `rm {}`",
                agent.label,
                quote(&agent.label),
                quote(&agent.plist.to_string_lossy())
            )
        }));
    }
    Some(DuplicateSupervisors {
        severity: "warning",
        supervisors,
        agents,
        fix,
    })
}

impl DuplicateSupervisors {
    /// `status`'s attention for the inbox: `supervisor_duplicate`, which a
    /// person settles as a stopped supervisor (`restart supervisor`), with
    /// the pids, the labels and the fix in `last_error`.
    pub fn attention(&self) -> Attention {
        let pids: Vec<String> = self.supervisors.iter().map(|s| s.pid.to_string()).collect();
        let labels: Vec<&str> = self.agents.iter().map(|a| a.label.as_str()).collect();
        Attention {
            run_id: None,
            task_id: None,
            pid: None,
            ask_id: None,
            reason_category: None,
            status: "duplicate".into(),
            kind: SUPERVISOR_DUPLICATE.into(),
            last_error: Some(format!(
                "{} supervisors live (pids {}), {} agents loaded ({}): {}",
                self.supervisors.len(),
                pids.join(", "),
                self.agents.len(),
                labels.join(", "),
                self.fix.join("; ")
            )),
            last_error_code: None,
            next: AttentionNext::RestartSupervisor,
        }
    }
}

/// The attention kind of [`DuplicateSupervisors::attention`].
pub const SUPERVISOR_DUPLICATE: &str = "supervisor_duplicate";

/// Add `duplicate_supervisors` ([`duplicate_supervisors`], null when there
/// is none) to `report`, and with `attention` its entry to the report's
/// `attention` (`status` for the inbox).
pub fn add_duplicate_supervisors(
    report: &mut Value,
    registrations: &[SupervisorRegistration],
    agents: &[crate::application::lifecycle::QueueAgent],
    now: i64,
    control: &dyn ProcessControl,
    attention: bool,
) -> Result<()> {
    let found = duplicate_supervisors(registrations, agents, now, control);
    if attention
        && let Some(found) = &found
        && let Some(entries) = report["attention"].as_array_mut()
    {
        entries.push(serde_json::to_value(found.attention())?);
    }
    report["duplicate_supervisors"] = serde_json::to_value(found)?;
    Ok(())
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

/// `status`'s and `doctor`'s `runtimes`, and their `actors` with the
/// worker where `read` puts it (goal 38): each target (the worker with its
/// resume, the verification of `integrate`) with the runtime `dagq.toml`
/// sets and how far that holds it, the host advisory and the container a
/// sandbox ([`runtime_executions`](crate::application::execution::runtime_executions)).
/// A file that could not be read shows `error` in place of the targets, and
/// `actors` as the host's.
pub fn add_runtimes(
    report: &mut Value,
    queue: &(impl SupervisorRegistry + ?Sized),
    control: &dyn ProcessControl,
    read: Result<crate::domain::container_runtime::RuntimeConfig>,
) -> Result<()> {
    use crate::application::execution::{ExecutionConfig, runtime_executions};
    let (config, runtimes) = match read {
        Ok(config) => (config, serde_json::to_value(runtime_executions(&config))?),
        Err(error) => (Default::default(), json!({"error": format!("{error:#}")})),
    };
    report["runtimes"] = runtimes;
    let checks = provider_checks(&queue.supervisors()?, control);
    report["actors"] =
        serde_json::to_value(ExecutionConfig::of_runtimes(&config).profiles(&checks)?)?;
    Ok(())
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
