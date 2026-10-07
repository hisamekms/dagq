//! Observations of an existing sccache server. Reads never start a server.
use super::{RunLog, SccacheServer, Verifier};
use crate::domain::{event_kind::EventKind, sccache::*};
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

/// Add sccache diagnostics only for a bound, readable configuration naming
/// sccache. Without that configuration its restart attention is hidden too.
pub fn add_diagnostics(
    queue: &dyn RunLog,
    server: &dyn SccacheServer,
    verifier: Option<&dyn Verifier>,
    run_dir: &Path,
    stats: bool,
    output: &mut Value,
) -> Result<()> {
    if let Some(report) = configured_report(queue, server, verifier, run_dir, stats)? {
        output["sccache"] = report;
    } else if let Some(attention) = output.get_mut("attention").and_then(Value::as_array_mut) {
        attention.retain(|entry| entry["kind"] != RESTART_FAILED);
    }
    Ok(())
}

fn configured_report(
    queue: &dyn RunLog,
    server: &dyn SccacheServer,
    verifier: Option<&dyn Verifier>,
    run_dir: &Path,
    stats: bool,
) -> Result<Option<Value>> {
    let Some(verifier) = verifier else {
        return Ok(None);
    };
    let Ok(env) = verifier.run_env(run_dir) else {
        return Ok(None);
    };
    let Some(target) = SccacheTarget::of_pairs(&env) else {
        return Ok(None);
    };
    let program = verifier.run_env_programs(None).ok().and_then(|check| {
        check
            .programs
            .into_iter()
            .find(|program| program.variable == WRAPPER_VAR)
            .and_then(|program| program.resolved)
    });
    report(
        queue,
        server,
        &target,
        Path::new(program.as_deref().unwrap_or(&target.program)),
        &env,
        stats,
    )
    .map(Some)
}

/// The latest recorded start's supervisor token only when its PID and port
/// match and so does its start: `started_at` when the record has one (a
/// reused PID differs there), else the process's start against the record's
/// `at` ([`started_near`]). Otherwise `"unknown"`, including a record
/// without a PID. A manually started replacement does not establish ownership.
pub fn owner(queue: &dyn RunLog, process: &ServerProcess, port: u16) -> Result<Value> {
    let started = queue.latest_queue_event(&[SCCACHE_SERVER_STARTED])?;
    Ok(started
        .filter(|event| {
            let record = &event.payload;
            record["pid"] == process.pid
                && record["port"] == port
                && match record["started_at"].as_str() {
                    Some(_) => record["started_at"] == process.started_at,
                    None => record["at"]
                        .as_i64()
                        .is_some_and(|at| started_near(process, at)),
                }
        })
        .and_then(|event| {
            event.payload["supervisor"]
                .as_str()
                .map(|owner| json!(owner))
        })
        .unwrap_or(json!("unknown")))
}

/// Keep a failed restart pending across supervisor handoffs. A different
/// current identity must be observed afresh rather than stopped for an old fault.
pub fn pending_restart(
    queue: &dyn RunLog,
    server: &dyn SccacheServer,
    port: u16,
) -> Result<Option<Value>> {
    let Some(event) = queue
        .latest_queue_event(&[RESTART_FAILED, SCCACHE_SERVER_STARTED])?
        .filter(|event| event.kind == RESTART_FAILED && event.payload["port"] == port)
    else {
        return Ok(None);
    };
    if let Some(process) = server.process(port)?
        && (event.payload["pid"] != process.pid
            || event.payload["started_at"] != process.started_at)
    {
        return Ok(None);
    }
    Ok(Some(event.payload))
}

/// Record a foreign identity once, including across supervisor handoffs.
/// Return the reason for restarting a known unhealthy server.
/// The detected event carries [`ServerProcess`], `at` in Unix seconds and
/// `port`. An unhealthy event adds `reason` (`sandboxed` or `failure_bias`)
/// and nullable `stats`. A sample is discarded if its identity changed during
/// the stats query. A stats error alone never establishes an unhealthy server.
/// Record the same identity and unhealthy reason once until a supervisor start,
/// while returning the diagnosis on every observation so retries continue.
pub fn observe(
    queue: &dyn RunLog,
    server: &dyn SccacheServer,
    target: &SccacheTarget,
    program: &Path,
    env: &[(String, String)],
    watch: &mut FailureWatch,
    at: i64,
) -> Result<Option<Value>> {
    let Some(process) = server.process(target.port)? else {
        return Ok(None);
    };
    let foreign = owner(queue, &process, target.port)? == "unknown";
    let last = queue.latest_queue_event(&[DETECTED])?;
    let already = last.is_some_and(|event| {
        event.payload["pid"] == process.pid
            && event.payload["started_at"] == process.started_at
            && event.payload["port"] == target.port
    });
    let mut payload = serde_json::to_value(&process)?;
    payload["at"] = json!(at);
    payload["port"] = json!(target.port);
    if foreign && !already {
        queue.record_queue_event(EventKind::SccacheServerDetected, payload.clone())?;
    }
    let stats = match server.stats(program, env, target.port) {
        Ok(stats) => stats,
        Err(_) if process.sandboxed == Some(true) => None,
        Err(error) => return Err(error),
    };
    // Do not attribute a sample to a process replaced during the client call.
    let still_same = server.process(target.port)?.is_some_and(|current| {
        current.pid == process.pid && current.started_at == process.started_at
    });
    if !still_same {
        return Ok(None);
    }
    let broken = stats.is_some_and(|stats| watch.observe(&process, stats));
    if process.sandboxed == Some(true) || broken {
        payload["reason"] = json!(if process.sandboxed == Some(true) {
            "sandboxed"
        } else {
            "failure_bias"
        });
        payload["stats"] = json!(stats);
        let already = queue
            .latest_queue_event(&[UNHEALTHY, SCCACHE_SERVER_STARTED])?
            .is_some_and(|event| {
                event.kind == UNHEALTHY
                    && same_identity(&event.payload, &payload)
                    && event.payload["reason"] == payload["reason"]
            });
        if !already {
            queue.record_queue_event(EventKind::SccacheServerUnhealthy, payload.clone())?;
        }
        return Ok(Some(payload));
    }
    Ok(None)
}

/// doctor uses live process and stats queries; status uses the last detection
/// for health and omits the stats client. Both are read-only.
/// `server` is a nullable [`ServerProcess`]; `started_by` is [`owner`].
/// `last_detection` is the latest health event, which may concern a previous
/// identity. Health is `unhealthy` only for the current identity's failure,
/// otherwise `unknown_origin` or `running`. No listener means `absent`;
/// an unreadable process means `unknown` with `error`. Doctor additionally
/// supplies `stats` and failures / requests as `failure_ratio` (zero for no
/// requests), or `stats_error` while preserving the process identity.
pub fn report(
    queue: &dyn RunLog,
    server: &dyn SccacheServer,
    target: &SccacheTarget,
    program: &Path,
    env: &[(String, String)],
    stats: bool,
) -> Result<Value> {
    let last = queue.latest_queue_event(HEALTH_KINDS)?;
    let mut report = json!({"port": target.port, "server": null, "started_by": "unknown",
        "stats": null, "failure_ratio": null, "last_detection": last});
    match server.process(target.port) {
        Ok(Some(process)) => {
            report["started_by"] = owner(queue, &process, target.port)?;
            report["server"] = json!(process);
            report["health"] = json!(if last.as_ref().is_some_and(|event| event.payload["pid"]
                == process.pid
                && event.payload["started_at"] == process.started_at
                && event.payload["port"] == target.port
                && matches!(event.kind.as_str(), UNHEALTHY | RESTART_FAILED))
            {
                "unhealthy"
            } else if report["started_by"] == "unknown" {
                "unknown_origin"
            } else {
                "running"
            });
            if stats {
                match server.stats(program, env, target.port) {
                    Ok(Some(stats)) => {
                        report["stats"] = json!(stats);
                        report["failure_ratio"] = json!(stats.failure_ratio());
                    }
                    Ok(None) => {}
                    Err(error) => report["stats_error"] = json!(format!("{error:#}")),
                }
            }
        }
        Ok(None) => report["health"] = json!("absent"),
        Err(error) => {
            report["health"] = json!("unknown");
            report["error"] = json!(format!("{error:#}"));
        }
    }
    Ok(report)
}

/// Stop a bad server and start its replacement with the same supervisor
/// environment as a missing server. Record failure or the replacement identity.
/// The supervisor supplies its token, PID and time in `payload`. A successful
/// start records `reason: restart`, `idle_timeout: "0"` and `replaced` with the
/// old PID, start time and unhealthy reason. Missing process metadata remains
/// null; an unreadable PID is reported in `pid_error` without failing the start.
/// A failed stop or start records the old identity, program and `error` once
/// while those values remain the same, including across supervisor handoffs;
/// health attention clears only after a recorded supervisor start, not merely
/// detection of a foreign replacement.
pub fn restart(
    queue: &dyn RunLog,
    server: &dyn SccacheServer,
    program: &Path,
    env: &[(String, String)],
    mut payload: Value,
) -> Result<()> {
    let port = payload["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| anyhow::anyhow!("restart has no valid sccache port"))?;
    let mut env = env.to_vec();
    env.retain(|(key, _)| key != IDLE_TIMEOUT_VAR);
    env.push((IDLE_TIMEOUT_VAR.into(), IDLE_TIMEOUT.into()));
    let result = server
        .stop(program, &env, port)
        .and_then(|()| server.start(program, &env, port));
    match result {
        Ok(pid) => {
            payload["replaced"] = json!({"pid": payload["pid"], "started_at": payload["started_at"], "reason": payload["reason"]});
            if let Some(object) = payload.as_object_mut() {
                object.remove("error");
                object.remove("pid_error");
                object.remove("stats");
            }
            payload["reason"] = json!("restart");
            payload["idle_timeout"] = json!(IDLE_TIMEOUT);
            payload["program"] = json!(program);
            payload["started_at"] = Value::Null;
            payload["parent_pid"] = Value::Null;
            payload["command"] = Value::Null;
            payload["sandboxed"] = Value::Null;
            match pid {
                Ok(pid) => payload["pid"] = json!(pid),
                Err(error) => {
                    payload["pid"] = Value::Null;
                    payload["pid_error"] = json!(error);
                }
            }
            if let Ok(Some(process)) = server.process(port)
                && payload["pid"] == process.pid
            {
                payload["started_at"] = json!(process.started_at);
                payload["parent_pid"] = json!(process.parent_pid);
                payload["command"] = json!(process.command);
                payload["sandboxed"] = json!(process.sandboxed);
            }
            queue.record_queue_event(EventKind::SccacheServerStarted, payload)?;
            Ok(())
        }
        Err(error) => {
            payload["error"] = json!(format!("{error:#}"));
            record_restart_failure(queue, program, payload)?;
            Err(error)
        }
    }
}

fn same_identity(left: &Value, right: &Value) -> bool {
    ["pid", "started_at", "port"]
        .iter()
        .all(|key| left[key] == right[key])
}

/// Share the failure notice between a failed replacement and its later retries
/// while no server listens. A changed identity, program or error notifies again;
/// time, counters and supervisor handoffs do not. A recorded supervisor start
/// resets the notice, so a later recurrence can wake the inbox again.
pub fn record_restart_failure(
    queue: &dyn RunLog,
    program: &Path,
    mut payload: Value,
) -> Result<()> {
    payload["program"] = json!(program);
    let already = queue
        .latest_queue_event(&[RESTART_FAILED, SCCACHE_SERVER_STARTED])?
        .is_some_and(|event| {
            event.kind == RESTART_FAILED
                && same_identity(&event.payload, &payload)
                && event.payload["program"] == payload["program"]
                && event.payload["error"] == payload["error"]
        });
    if !already {
        queue.record_queue_event(EventKind::SccacheServerRestartFailed, payload)?;
    }
    Ok(())
}

/// The look just before a process of `target` given `[run.env]` starts
/// (ADR-t2086-1), where nothing starts a missing server (a wrapper's turn,
/// a person's `integrate`, the e2e): a connect to its port, then the guard
/// made in `dir`. The supervisor looks with its own start first.
pub fn look(server: &dyn SccacheServer, target: &SccacheTarget, dir: &Path) -> GuardLook {
    let unconfirmed = |why: String| GuardLook::Unconfirmed {
        port: target.port,
        why,
    };
    match server.listening(target.port) {
        Ok(true) => guard_in(server, dir, target.port),
        Ok(false) => unconfirmed(format!("no sccache server listens on port {}", target.port)),
        Err(error) => unconfirmed(format!(
            "the sccache server could not be looked at: {error:#}"
        )),
    }
}

/// The guard made in `dir` for the server confirmed on `port`, or why the
/// process runs without `RUSTC_WRAPPER`.
pub fn guard_in(server: &dyn SccacheServer, dir: &Path, port: u16) -> GuardLook {
    let why = match server.guard(dir) {
        Ok(guard) => match guard.to_str() {
            Some(guard) => return GuardLook::Guard(guard.to_owned()),
            None => format!("the guard's path {} is not UTF-8", guard.display()),
        },
        Err(error) => format!("the guard could not be made: {error:#}"),
    };
    GuardLook::Unconfirmed { port, why }
}

/// The payload of `sccache_wrapper_removed`, one shape whoever writes it
/// (ADR-t2086-1): when (`at`, Unix seconds), who looked (`by`: `wrapper`,
/// `supervisor` or `integrate`), the server's `port`, why (`reason`), and
/// the process's `fields` (its `turn`, or its `job` and `attempt`).
pub fn removed_payload(at: i64, by: &str, port: u16, why: &str, fields: Value) -> Value {
    let mut payload = json!({
        "at": at,
        "by": by,
        "port": port,
        "reason": why,
    });
    if let (Some(payload), Some(fields)) = (payload.as_object_mut(), fields.as_object()) {
        payload.extend(fields.clone());
    }
    payload
}

/// Record `sccache_wrapper_removed` ([`removed_payload`]) on run `run`:
/// its process runs without `RUSTC_WRAPPER`. The one writer of the event
/// for the wrapper, the supervisor and integrate; a failure to record is
/// only logged, as the process runs either way.
pub fn record_wrapper_removed(
    queue: &dyn RunLog,
    run: &crate::domain::RunId,
    at: i64,
    by: &str,
    port: u16,
    why: &str,
    fields: Value,
) {
    tracing::warn!(run_id = %run, "run {run}: its {} runs without {WRAPPER_VAR}: {why}", process_name(&fields));
    let payload = removed_payload(at, by, port, why, fields);
    if let Err(error) = queue.record_runtime_event(run, EventKind::SccacheWrapperRemoved, payload) {
        tracing::warn!(run_id = %run, error = %format_args!("{error:#}"), "sccache_wrapper_removed could not be recorded: {error:#}");
    }
}

/// How the log names the process of `fields`: its job, or its turn.
fn process_name(fields: &Value) -> String {
    match (fields["job"].as_str(), fields["turn"].as_u64()) {
        (Some(job), _) => job.to_owned(),
        (None, Some(turn)) => format!("turn {turn}"),
        _ => "process".to_owned(),
    }
}

/// A server that is only looked at and guarded, for the tests of the
/// processes given `[run.env]`: it listens or not (or cannot be looked at),
/// and makes the guard `<dir>/dagq-rustc-wrapper` or fails to.
#[cfg(test)]
pub(crate) struct LookedAt {
    pub listening: Result<bool, &'static str>,
    pub guard: bool,
    pub looks: std::cell::Cell<usize>,
}

#[cfg(test)]
impl LookedAt {
    pub fn new(listening: Result<bool, &'static str>, guard: bool) -> Self {
        Self {
            listening,
            guard,
            looks: std::cell::Cell::new(0),
        }
    }
}

#[cfg(test)]
impl SccacheServer for LookedAt {
    fn listening(&self, _: u16) -> Result<bool> {
        self.looks.set(self.looks.get() + 1);
        self.listening.map_err(|why| anyhow::anyhow!(why))
    }
    fn process(&self, _: u16) -> Result<Option<ServerProcess>> {
        Ok(None)
    }
    fn stats(&self, _: &Path, _: &[(String, String)], _: u16) -> Result<Option<ServerStats>> {
        unreachable!("a look reads no stats")
    }
    fn stop(&self, _: &Path, _: &[(String, String)], _: u16) -> Result<()> {
        unreachable!("a look stops nothing")
    }
    fn start(&self, _: &Path, _: &[(String, String)], _: u16) -> Result<super::ServerPid> {
        unreachable!("a look starts nothing")
    }
    fn guard(&self, dir: &Path) -> Result<std::path::PathBuf> {
        anyhow::ensure!(self.guard, "no dagq binary to make the guard of");
        Ok(dir.join(GUARD_NAME))
    }
}

#[cfg(test)]
mod tests;
