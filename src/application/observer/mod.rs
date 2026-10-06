//! The observer job (ADR-0044 decision 4): a headless agent run that reads
//! `stats`, the unsettled findings, the latest notes, the open asks and the
//! dependency graph, and may write only findings (ADR-0044 decision 18) and
//! `blocked` asks. The CLI refuses everything else under
//! `DAGQ_ROLE=observer`, notes and goals included. The supervisor starts it on
//! a timer (`--observe-interval`, `--observe-daily`); `observe` starts it
//! by hand. An observation that finds no event since the last one but its
//! own and no alert the last one did not see starts no agent and records a
//! skipped `observe_finished`; the agent
//! loads no MCP server; `observe --history` reads what each observation
//! read and wrote. The prompt carries the input within limits and says
//! how to read what it left out ([`input`], ADR-t1566-1).
mod input;

pub use input::{
    INPUT_PAGE, LANGUAGE_RESERVE, PROMPT_LIMIT, Reading, SectionSize, check_read, read_input,
};

use crate::domain::EventKind;
use crate::domain::actor_model::records_unusable;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Result;
use serde_json::{Value, json};

use crate::{
    application::{
        AgentProvider, AgentSignals, AskQuery, Generators, ObserverLog, Queue, RunLog,
        WorkspaceBackend, dependency_graph, lifecycle::OBSERVER_ROLE,
    },
    domain::{
        ActorContext, ActorRole, AskId, CONSECUTIVE_FAILURES, EventId, FindingQuery, NewHold,
        NoteQuery, Provider, RunEvent,
        actor_model::ActorLaunch,
        headless_job::{JobAccess, JobFailure},
        language::Language,
        provider_switch::SwitchReason,
        queue_hold::{HoldJob, Wall},
        stats::StatsQuery,
    },
};

/// Observations `observe --history` lists by default.
pub const HISTORY_LIMIT: usize = 20;
/// Notes the prompt carries.
pub const PROMPT_NOTES: usize = 20;
/// How long one observer run may take before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// What the observer may do: run the queue CLI only (ADR-t1063-1 decision
/// 2), which its role's policy lets write findings and `blocked` asks.
pub const ACCESS: JobAccess = JobAccess::QueueCli;

/// The supervisor starts the observations on its timer, so their modes
/// belong to its use case.
pub use crate::application::supervise::{DAILY_WINDOW_SECS, ObserveMode};

#[derive(Debug, Clone)]
pub struct ObserveOptions {
    pub mode: ObserveMode,
    /// cmux executable; bare names resolve on PATH. None disables workspace
    /// listing and inbox notifications, as does an executable not found.
    pub cmux: Option<PathBuf>,
    /// The cursor to read `stats` past; by default the hourly observation's
    /// saved cursor, or for the daily one the last event 24 hours ago.
    pub since: Option<EventId>,
    /// Build and return the prompt without starting the agent.
    pub dry_run: bool,
    pub timeout: Duration,
    /// The `dagq` binary the agent calls; its directory goes first on PATH.
    pub dagq: PathBuf,
    /// The user's `config.toml` the language comes from under the bound
    /// checkout's `dagq.toml` (ADR-t616-2); `None` reads none.
    pub user_config: Option<PathBuf>,
    /// The prompt's bytes ([`PROMPT_LIMIT`] but in tests).
    pub prompt_limit: usize,
    /// What the agent starts with: the provider the supervisor routed it
    /// to (ADR-t1063-1 decisions 1 and 4, task 1223), the provider of
    /// `provider`; `None` reads `[roles.observer]` of the bound checkout's
    /// `dagq.toml` ([`ObserverHost::launch`]).
    pub launch: Option<ActorLaunch>,
    /// Whether `[roles.observer]` names its provider, so that a Codex
    /// observation that stopped where Codex cannot be used (a login, the
    /// usage limit, a launch that failed, the executable gone) records
    /// `provider_unusable`, and the supervisor holds Codex and starts the
    /// observation again on the other provider (ADR-t1063-1 decision 4).
    pub switchable: bool,
    /// `[provider_fallback] jobs` (ADR-t1857-1): with it off (`false`) a
    /// Claude observation of a role that names its provider that stopped
    /// where Claude cannot be used records `provider_unusable` too, and
    /// the supervisor starts it again on Claude once Claude's hold ends.
    pub fallback: bool,
    /// Why no provider can run the observation (`--no-claude` with Codex
    /// not usable, ADR-t1204-1): an observation that is not skipped records
    /// its finish as an `error` with this reason instead of starting the
    /// agent.
    pub unavailable: Option<String>,
}

/// The reads of the queue the observer's input takes that the composition
/// root assembles (`stats`, the KPIs, the improvements and the bound
/// checkout), and the cmux the hold ask notifies the inbox through.
pub trait ObserverSources<Q: ?Sized> {
    fn stats(&self, queue: &Q, db: &Path, query: &StatsQuery) -> Result<Value>;
    fn kpi(&self, queue: &Q, db: &Path) -> Result<Value>;
    fn improvements(&self, queue: &Q) -> Result<Value>;
    /// The checkout the queue is bound to, if any.
    fn checkout(&self, queue: &Q) -> Result<Option<PathBuf>>;
    /// The configured cmux, when there is one and it was found.
    fn cmux(&self) -> Option<&dyn WorkspaceBackend>;
}

/// The host the observation runs on: its files under `<queue
/// dir>/observer`, the configuration its agent starts with and the agent's
/// process.
pub trait ObserverHost {
    /// The hourly observation's cursor, if one was saved.
    fn read_cursor(&self, db: &Path) -> Result<Option<EventId>>;
    fn write_cursor(&self, db: &Path, cursor: EventId) -> Result<()>;
    /// A new directory for the observation started at `started`.
    fn observation_dir(&self, db: &Path, started: i64) -> Result<PathBuf>;
    fn write(&self, path: &Path, contents: &str) -> Result<()>;
    /// What the agent printed in `dir`: its stdout and its stderr, each
    /// empty for a stream it did not leave.
    fn output(&self, dir: &Path) -> (String, String);
    /// What the observer's agent starts with (ADR-0079 decision 7).
    fn launch(&self, checkout: Option<&Path>) -> ActorLaunch;
    /// The language of the prompt (ADR-t616-2).
    fn language(&self, checkout: Option<&Path>, user_config: Option<&Path>) -> Option<Language>;
    /// Start the agent in `dir` and wait for it up to its timeout: the
    /// exit code, or `None` when a signal ended it.
    fn run(
        &self,
        provider: &dyn AgentProvider,
        db: &Path,
        dir: &Path,
        prompt: &str,
        agent: &HeadlessAgent<'_>,
    ) -> Result<Option<i32>>;
}

/// What [`observe`] reaches outside the queue through.
pub struct ObserverEnvironment<'a, Q: ?Sized> {
    pub sources: &'a dyn ObserverSources<Q>,
    pub host: &'a dyn ObserverHost,
    pub generators: &'a Generators,
}

/// Run one observation: gather the inputs, start the agent headless with
/// `DAGQ_ROLE=observer`, wait for it, then record `observe_finished` with
/// what it wrote and, for a succeeded hourly one, save the new cursor.
/// `signals` are Claude Code's, which read a Claude observation's failure
/// (a Codex one is read by `provider`'s `job_failure`); they must belong to
/// `provider` when it is Claude's. `db` is the queue's canonical path,
/// which names its directory.
pub fn observe<Q: Queue + ObserverLog>(
    queue: &mut Q,
    db: &Path,
    provider: &dyn AgentProvider,
    signals: Option<&dyn AgentSignals>,
    options: &ObserveOptions,
    environment: &ObserverEnvironment<'_, Q>,
) -> Result<Value> {
    let ObserverEnvironment {
        sources,
        host,
        generators,
    } = *environment;
    let started = generators.clock.now();
    let since = match (options.since, options.mode) {
        (Some(since), _) => Some(since),
        (None, ObserveMode::Hourly) => host.read_cursor(db)?,
        (None, ObserveMode::Daily) => Some(queue.event_id_before(started - DAILY_WINDOW_SECS)?),
    };
    let stats = sources.stats(
        queue,
        db,
        &StatsQuery {
            since: since.map(Into::into),
            ..StatsQuery::default()
        },
    )?;
    let alerts = alert_keys(&stats);
    // A cursor given by hand asks to read past it whatever happened since.
    if options.since.is_none()
        && !options.dry_run
        && let Some(payload) = skip(queue, options.mode, since, &alerts)?
    {
        return Ok(payload);
    }
    let cursor = EventId::new(stats["next_cursor"].as_i64().unwrap_or_default());
    // No provider can run it (`--no-claude`, Codex not usable): the
    // observation records why and starts no agent (ADR-t1204-1, task
    // 1223). Its window is read again by the next one.
    if !options.dry_run
        && let Some(why) = &options.unavailable
    {
        return unavailable(queue, options.mode, since, cursor, &alerts, why);
    }
    let notes = queue.notes(&NoteQuery {
        goal_id: None,
        task_id: None,
        since: None,
        limit: PROMPT_NOTES,
    })?;
    let asks = queue.asks(AskQuery {
        open: true,
        ..AskQuery::default()
    })?;
    let findings = queue.findings(&FindingQuery::default())?;
    let graph = dependency_graph(queue.graph_input()?, None);
    // A KPI that does not read leaves the rest of the observation to run.
    let kpi = sources
        .kpi(queue, db)
        .unwrap_or_else(|error| json!({"error": format!("{error:#}")}));
    let improvements = sources
        .improvements(queue)
        .unwrap_or_else(|error| json!({"error": format!("{error:#}")}));
    let input = observer_input(ObserverInput {
        stats,
        kpi,
        findings: serde_json::to_value(findings)?,
        improvements,
        notes: serde_json::to_value(notes.notes)?,
        open_asks: serde_json::to_value(asks)?,
        graph: json!({"candidates": graph.candidates, "critical": graph.critical}),
    });
    // The job's `dagq` goes to the queue service in client mode: the
    // prompt, an argument of the agent, names no queue path (goal 82's
    // stage (3)).
    let command = "dagq";
    let checkout = sources.checkout(queue)?;
    let language = host.language(checkout.as_deref(), options.user_config.as_deref());
    // The prompt names the observation's directory, whose `input.json`
    // `observe --input` reads what the prompt left out from.
    let dir = if options.dry_run {
        None
    } else {
        Some(host.observation_dir(db, started)?)
    };
    let observation = dir.as_deref().and_then(Path::file_name).map_or_else(
        || "DRY-RUN".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let fitted = observer_prompt(
        options.mode,
        command,
        since,
        &observation,
        &input,
        options.prompt_limit,
    )?;
    let prompt = crate::domain::language::with_instruction(fitted.text, language.as_ref());
    let prompt_record = json!({
        "prompt_bytes": prompt.len(),
        "prompt_limit": options.prompt_limit,
        "prompt_sections": fitted.sections,
    });
    let Some(dir) = dir else {
        let mut payload = json!({
            "dry_run": true,
            "mode": options.mode.as_str(),
            "since": since,
            "cursor": cursor,
            "prompt": prompt,
        });
        merge(&mut payload, &prompt_record);
        return Ok(payload);
    };
    host.write(&dir.join("prompt.md"), &prompt)?;
    host.write(
        &dir.join("input.json"),
        &serde_json::to_string_pretty(&input)?,
    )?;
    let ask_mark = queue.ask_high_water()?;
    let launch = options
        .launch
        .clone()
        .unwrap_or_else(|| host.launch(checkout.as_deref()));
    // The job's Claude session id (ADR-0048 decision 4); Codex names its
    // thread itself, which the finish records (ADR-t1063-1 decision 6).
    // The actor is one per observation either way.
    let instance = generators.ids.uuid();
    let session_id = (launch.provider == Provider::Claude).then(|| instance.clone());
    let mut started_payload = json!({"mode": options.mode.as_str(), "since": since, "dir": dir, "session_id": session_id, "launch": launch.to_value()});
    merge(&mut started_payload, &prompt_record);
    let event_mark = queue.record_queue_event(EventKind::ObserveStarted, started_payload)?;
    tracing::info!(
        mode = options.mode.as_str(),
        since = since.map(EventId::as_i64),
        "observer ({}) started",
        options.mode.as_str()
    );
    let clock = Instant::now();
    let started_ms = generators
        .clock
        .system_time()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok());
    // `failed`: the agent exited non-zero or by a signal; `error`: it could
    // not start or ran past the timeout.
    let mut start_failure = None;
    let (outcome, exit_code, error) = match host.run(
        provider,
        db,
        &dir,
        &prompt,
        &HeadlessAgent {
            actor: observer_actor(&instance),
            session_id: session_id.as_deref(),
            launch: &launch,
            dagq: &options.dagq,
            timeout: options.timeout,
            what: OBSERVER_JOB,
            access: ACCESS,
        },
    ) {
        Ok(Some(0)) => ("succeeded", Some(0), None),
        Ok(code) => ("failed", code, None),
        Err(error) => {
            // An agent that did not start at all (its executable gone)
            // left no output to read why: its start's error says.
            if not_started(&error, OBSERVER_JOB) {
                start_failure = Some(crate::application::job_start_failure(&error));
            }
            ("error", None, Some(format!("{error:#}")))
        }
    };
    let (stdout, stderr) = host.output(&dir);
    let (wall, unusable) = read_failure(
        outcome == "succeeded",
        launch.provider,
        (options.switchable, options.fallback),
        start_failure,
        || start_failure.unwrap_or_else(|| provider.job_failure(&stdout, &stderr)),
        // Both streams, with a line boundary so stderr diagnostics cannot
        // be joined onto a partial stdout line.
        || signals.map(|signals| signals.job_failure(&format!("{stdout}\n{stderr}"))),
    );
    let provider_unusable = unusable
        .map(|reason| json!({"provider": launch.provider.as_str(), "reason": reason.as_str()}));
    // A hold that could not be written is logged: the observation's
    // finish is recorded either way.
    let hold = wall.and_then(|wall| {
        hold_wall(queue, wall, checkout.as_deref(), sources.cmux())
            .inspect_err(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "the observer stopped at the {} wall, and its hold ask could not be written: {error:#}", wall.as_str());
            })
            .ok()
    });
    let written = queue.written_by(OBSERVER_ROLE, event_mark, ask_mark)?;
    let (recorded, updated, closed, asks, without_ask) = (
        written.recorded.len(),
        written.updated.len(),
        written.closed.len(),
        written.asks.len(),
        written.without_ask.len(),
    );
    let failures = consecutive_failures(queue, outcome)?;
    let saved = outcome == "succeeded" && options.mode == ObserveMode::Hourly;
    if saved {
        host.write_cursor(db, cursor)?;
    }
    let mut payload = json!({
        "mode": options.mode.as_str(),
        "outcome": outcome,
        CONSECUTIVE_FAILURES: failures,
        "exit_code": exit_code,
        "error": error,
        "since": since,
        "cursor": cursor,
        "cursor_saved": saved,
        "findings_recorded": recorded,
        "findings_updated": updated,
        "findings_closed": closed,
        "asks": asks,
        "findings_without_ask": without_ask,
        "recorded_finding_ids": written.recorded,
        "updated_finding_ids": written.updated,
        "closed_finding_ids": written.closed,
        "ask_ids": written.asks,
        "without_ask_finding_ids": written.without_ask,
        "duration_secs": clock.elapsed().as_secs(),
        "dir": dir,
        "wall": wall.map(Wall::as_str),
        "hold_ask_id": hold,
        "alerts": alerts,
    });
    // The span this observation's start opened closes by its directory;
    // Codex's thread and the model of its rollout (ADR-t1063-1 decision 6)
    // are the id and the model it and `stats` read.
    if let Some(session) = provider.job_session(&stdout, started_ms) {
        session.record(&mut payload);
    }
    if let Some(unusable) = provider_unusable {
        payload["provider_unusable"] = unusable;
    }
    queue.record_queue_event(EventKind::ObserveFinished, payload.clone())?;
    tracing::info!(
        mode = options.mode.as_str(),
        outcome,
        exit_code,
        error,
        findings_recorded = recorded,
        findings_updated = updated,
        findings_closed = closed,
        asks,
        findings_without_ask = without_ask,
        "observer ({}) finished: {outcome}",
        options.mode.as_str()
    );
    Ok(payload)
}

/// The observer as the errors of its agent name it.
const OBSERVER_JOB: &str = "the observer";

/// What the failure of a headless job on its timer (an observation, a
/// throughput review) that did not succeed leads to: the wall of the
/// queue's hold ask it joins, and the reason its provider cannot be used.
/// A Claude agent stopped at a login that ran out or the usage limit joins
/// the queue's hold ask (ADR-0047 decision 42, task 438): no observer
/// starts again until a person answers it; `claude` reads it with Claude's
/// signals (`None` without them). A Codex one does not hold Claude: when
/// its role names its provider (`switchable`), `codex` (its start's error,
/// or its provider's reading of its output) says whether Codex could not
/// be used, and the supervisor holds Codex and starts the job again on the
/// other provider (ADR-t1063-1 decision 4), or, with `[provider_fallback]
/// jobs` off (`fallback` false), on Codex once its hold ends. With the
/// fallback off a Claude one of a role that names its provider says so
/// too: its start's error (`start`) or its wall, and the supervisor holds
/// Claude and starts it again on Claude once Claude's hold ends
/// (ADR-t1857-1). Neither is read for a success.
pub(crate) fn read_failure(
    succeeded: bool,
    provider: Provider,
    (switchable, fallback): (bool, bool),
    start: Option<JobFailure>,
    codex: impl FnOnce() -> JobFailure,
    claude: impl FnOnce() -> Option<JobFailure>,
) -> (Option<Wall>, Option<SwitchReason>) {
    let records = records_unusable(provider, switchable, fallback);
    match (succeeded, provider) {
        (true, _) => (None, None),
        (false, Provider::Codex) => (
            None,
            records.then(codex).and_then(JobFailure::switch_reason),
        ),
        (false, Provider::Claude) => {
            let failure = start.or_else(claude);
            (
                failure.and_then(JobFailure::wall),
                failure
                    .filter(|_| records)
                    .and_then(JobFailure::switch_reason),
            )
        }
    }
}

/// Record the finish of an observation no provider can run (`why`), with
/// no agent started and no directory, and return its payload: an `error`,
/// so the next observation reads its window again.
fn unavailable(
    queue: &(impl RunLog + ObserverLog),
    mode: ObserveMode,
    since: Option<EventId>,
    cursor: EventId,
    alerts: &[Value],
    why: &str,
) -> Result<Value> {
    let error = format!("the observer could not start: {why}");
    let failures = consecutive_failures(queue, "error")?;
    let payload = json!({
        "mode": mode.as_str(),
        "outcome": "error",
        CONSECUTIVE_FAILURES: failures,
        "exit_code": null,
        "error": error,
        "unavailable": true,
        "since": since,
        "cursor": cursor,
        "cursor_saved": false,
        "findings_recorded": 0,
        "findings_updated": 0,
        "findings_closed": 0,
        "asks": 0,
        "findings_without_ask": 0,
        "recorded_finding_ids": [],
        "updated_finding_ids": [],
        "closed_finding_ids": [],
        "ask_ids": [],
        "without_ask_finding_ids": [],
        "duration_secs": 0,
        "dir": null,
        "alerts": alerts,
    });
    queue.record_queue_event(EventKind::ObserveFinished, payload.clone())?;
    tracing::warn!(
        mode = mode.as_str(),
        "observer ({}) not started: {why}",
        mode.as_str()
    );
    Ok(payload)
}

/// The [`CONSECUTIVE_FAILURES`] of an observation that ended `outcome`,
/// counted on from the queue's last `observe_finished` (task 1574).
fn consecutive_failures(queue: &dyn ObserverLog, outcome: &str) -> Result<i64> {
    let last = queue.observations(1)?;
    Ok(crate::domain::consecutive_failures(
        last.first().map(|(finished, _)| &finished.payload),
        outcome,
    ))
}

/// `extra`'s keys set on the object `payload`.
fn merge(payload: &mut Value, extra: &Value) {
    if let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
        payload.extend(extra.clone());
    }
}

/// Add the observer to the hold ask of `wall`, or open it (notifying the
/// inbox through `cmux` when there is one), and record `auth_required` or
/// `usage_limited` on the queue when it joined. Returns the ask's ID.
fn hold_wall(
    queue: &mut dyn Queue,
    wall: Wall,
    checkout: Option<&Path>,
    cmux: Option<&dyn WorkspaceBackend>,
) -> Result<AskId> {
    let hold = NewHold::wall(wall, None, Some(HoldJob::Observer));
    let outcome = match (checkout, cmux) {
        (Some(checkout), Some(cmux)) => {
            crate::application::ask::hold(queue, checkout, hold, cmux)?.0
        }
        _ => queue.hold(hold)?,
    };
    if outcome.joined {
        queue.record_queue_event(
            wall.event_kind(),
            json!({
                "job": HoldJob::Observer.kind(),
                "entry": HoldJob::Observer.entry(),
                "ask_id": outcome.ask.id,
            }),
        )?;
    }
    tracing::warn!(
        ask_id = outcome.ask.id.as_i64(),
        "the observer stopped at the {} wall: ask {} holds it",
        wall.as_str(),
        outcome.ask.id
    );
    Ok(outcome.ask.id)
}

/// What one observation reads, each part as JSON.
pub struct ObserverInput {
    pub stats: Value,
    /// [`ObserverSources::kpi`], or `{"error": ...}`.
    pub kpi: Value,
    pub findings: Value,
    /// [`ObserverSources::improvements`], or `{"error": ...}`.
    pub improvements: Value,
    pub notes: Value,
    pub open_asks: Value,
    pub graph: Value,
}

/// The input the prompt carries and `input.json` keeps: `stats`, the KPIs
/// (ADR-0051 decision 24), the unsettled findings with the improvements
/// running and their limit (decision 25), the notes, the open asks and
/// the graph. The lists the prompt may cut go in the order it keeps them
/// (task 1567), so what it left out is the rest from an offset of
/// `input.json`: the KPI breaches longest first (`streak`, then
/// `subject`), the findings weightiest first (`impact` high, normal, low,
/// then more `occurrences`, then the newest), the open asks newest first.
pub fn observer_input(input: ObserverInput) -> Value {
    let mut kpi = input.kpi;
    if let Some(breaches) = kpi.get_mut("breaches").and_then(Value::as_array_mut) {
        breaches.sort_by_key(|breach| {
            (
                std::cmp::Reverse(breach["streak"].as_i64().unwrap_or_default()),
                breach["subject"].as_str().unwrap_or_default().to_owned(),
            )
        });
    }
    let mut findings = input.findings;
    if let Some(findings) = findings.as_array_mut() {
        findings.sort_by_key(|finding| {
            let impact = match finding["impact"].as_str() {
                Some("high") => 0,
                Some("normal") => 1,
                Some("low") => 2,
                _ => 3,
            };
            (
                impact,
                std::cmp::Reverse(finding["occurrences"].as_i64().unwrap_or_default()),
                std::cmp::Reverse(finding["id"].as_i64().unwrap_or_default()),
            )
        });
    }
    let mut open_asks = input.open_asks;
    if let Some(asks) = open_asks.as_array_mut() {
        asks.sort_by_key(|ask| std::cmp::Reverse(ask["id"].as_i64().unwrap_or_default()));
    }
    json!({
        "stats": input.stats,
        "kpi": kpi,
        "findings": findings,
        "improvements": input.improvements,
        "notes": input.notes,
        "open_asks": open_asks,
        "graph": input.graph,
    })
}

/// What tells the alerts of `stats` apart (ADR-t649-1): the kind and the
/// target (task, run, file, ask, workspace) of each of `alerts` and
/// `running_alerts`, without the value that grows with time, sorted and
/// without repeats.
fn alert_keys(stats: &Value) -> Vec<Value> {
    const TARGETS: &[&str] = &["task_id", "run_id", "path", "ask_id", "workspace_id"];
    let mut keys = ["alerts", "running_alerts"]
        .iter()
        .filter_map(|list| stats[*list].as_array())
        .flatten()
        .map(|alert| {
            let mut key = serde_json::Map::new();
            key.insert("kind".into(), alert["kind"].clone());
            for target in TARGETS {
                if let Some(value) = alert.get(*target).filter(|value| !value.is_null()) {
                    key.insert((*target).into(), value.clone());
                }
            }
            Value::Object(key)
        })
        .collect::<Vec<_>>();
    keys.sort_by_key(Value::to_string);
    keys.dedup();
    keys
}

/// Whether `alerts` has one the last observation did not see; one that
/// kept no `alerts` (from before ADR-t649-1) saw none of them.
fn new_alerts(last: &Value, alerts: &[Value]) -> bool {
    match last["alerts"].as_array() {
        Some(seen) => alerts.iter().any(|alert| !seen.contains(alert)),
        None => !alerts.is_empty(),
    }
}

/// When the last observation of `mode` that ran its agent succeeded, no
/// event but the observer's own came after the events it read and none of
/// `alerts` is new since it (ADR-t649-1: an alert that grew past its
/// threshold with time alone records no event), record a skipped
/// `observe_finished` without starting anything, and return its payload.
fn skip(
    queue: &(impl Queue + ObserverLog),
    mode: ObserveMode,
    since: Option<EventId>,
    alerts: &[Value],
) -> Result<Option<Value>> {
    let Some((previous, last)) = queue.last_observation(mode.as_str())? else {
        return Ok(None);
    };
    // Counted past what its input read, not past its finish: events of
    // others while its agent ran are unread too.
    let read = last["cursor"].as_i64().map_or(previous, EventId::new);
    if last["outcome"] != "succeeded"
        || new_alerts(&last, alerts)
        || queue.events_besides(OBSERVER_ROLE, crate::domain::sessions::OBSERVER, read)? > 0
    {
        return Ok(None);
    }
    let payload = json!({
        "mode": mode.as_str(),
        "outcome": "skipped",
        CONSECUTIVE_FAILURES: 0,
        "reason": "no events but the observer's own and no new alert since the last observation",
        "previous_event_id": previous,
        "since": since,
        "cursor": since,
        "cursor_saved": false,
        "findings_recorded": 0,
        "findings_updated": 0,
        "findings_closed": 0,
        "asks": 0,
        "findings_without_ask": 0,
        "recorded_finding_ids": [],
        "updated_finding_ids": [],
        "closed_finding_ids": [],
        "ask_ids": [],
        "without_ask_finding_ids": [],
        "duration_secs": 0,
        "dir": null,
        "alerts": alerts,
    });
    queue.record_queue_event(EventKind::ObserveFinished, payload.clone())?;
    tracing::info!(
        mode = mode.as_str(),
        previous = previous.as_i64(),
        "observer ({}) skipped: nothing happened since event {previous}",
        mode.as_str()
    );
    Ok(Some(payload))
}

/// `observe --history`: the newest `limit` observations, newest first, each
/// with the events it read (after `since` through `cursor`), the findings
/// it recorded, updated and closed and the asks it wrote, how long it took
/// and whether it was skipped.
/// Observations recorded before the ids were kept give the counts only.
pub fn history(queue: &dyn ObserverLog, limit: usize) -> Result<Value> {
    let observations = queue
        .observations(limit)?
        .into_iter()
        .map(|(finished, started)| history_entry(&finished, started.as_ref()))
        .collect::<Vec<_>>();
    Ok(json!({"observations": observations}))
}

fn history_entry(finished: &RunEvent, started: Option<&RunEvent>) -> Value {
    let payload = &finished.payload;
    let field = |name: &str| payload.get(name).cloned().unwrap_or(Value::Null);
    let outcome = field("outcome");
    json!({
        "event_id": finished.id,
        "mode": field("mode"),
        "outcome": outcome,
        "skipped": outcome == "skipped",
        // Recorded since task 1574; null before.
        CONSECUTIVE_FAILURES: field(CONSECUTIVE_FAILURES),
        "started_at": started.map_or(&finished.created_at, |started| &started.created_at),
        "finished_at": finished.created_at,
        "duration_secs": field("duration_secs"),
        "input": {"since": field("since"), "through": field("cursor")},
        "cursor_saved": field("cursor_saved"),
        "findings": {
            "recorded": field("findings_recorded"),
            "updated": field("findings_updated"),
            "closed": field("findings_closed"),
            "recorded_ids": field("recorded_finding_ids"),
            "updated_ids": field("updated_finding_ids"),
            "closed_ids": field("closed_finding_ids"),
        },
        "asks": {"count": field("asks"), "ids": field("ask_ids")},
        "without_ask": {"count": field("findings_without_ask"), "ids": field("without_ask_finding_ids")},
        "exit_code": field("exit_code"),
        "error": field("error"),
        "dir": field("dir"),
        // What the prompt came to (task 1567); none for a skipped one or
        // one started before it was recorded.
        "prompt_bytes": started.map_or(Value::Null, |started| started.payload.get("prompt_bytes").cloned().unwrap_or(Value::Null)),
        "prompt_limit": started.map_or(Value::Null, |started| started.payload.get("prompt_limit").cloned().unwrap_or(Value::Null)),
        "prompt_sections": started.map_or(Value::Null, |started| started.payload.get("prompt_sections").cloned().unwrap_or(Value::Null)),
    })
}

/// The actor the observer's agent runs as (ADR-t728-1 decision 4):
/// `observer`, one actor per observation, by `instance`, the uuid made once
/// per observation. On Claude it is also the job's session id; a Codex
/// observation has no session id, since Codex names its thread itself
/// (task 1223).
fn observer_actor(instance: &str) -> ActorContext {
    ActorContext::instance(ActorRole::Observer, instance)
}

/// Whether `error` says the agent of the job `what` names did not start at
/// all: its context is [`HeadlessAgent::start_context`], which the host
/// that runs the agent gives the error of its start, so its output has
/// nothing to read why.
pub fn not_started(error: &anyhow::Error, what: &str) -> bool {
    error.to_string() == HeadlessAgent::start_context(what)
}

/// Who a headless job of the queue's (the observer, the throughput review)
/// runs as and how.
pub struct HeadlessAgent<'a> {
    pub actor: ActorContext,
    /// The session id the runtime gives the job (Claude Code's, ADR-0048
    /// decision 4); `None` for a provider that names its session itself
    /// (Codex's thread, which the job's end records).
    pub session_id: Option<&'a str>,
    pub launch: &'a ActorLaunch,
    /// The `dagq` binary the agent calls; its directory goes first on PATH.
    pub dagq: &'a Path,
    pub timeout: Duration,
    /// The job as the errors name it.
    pub what: &'a str,
    /// What the agent may do, as an intent its provider turns into its
    /// own mechanism.
    pub access: JobAccess,
}

impl HeadlessAgent<'_> {
    /// The context of the error of an agent that did not start, for the
    /// job `what` names: the host that runs the agent gives it, and
    /// [`not_started`] reads it.
    pub fn start_context(what: &str) -> String {
        format!("start {what}'s agent")
    }
}

/// The observer's instructions and its input within `limit` bytes
/// ([`input::fit`]): `observation` names the directory `observe --input`
/// reads the whole input of.
pub fn observer_prompt(
    mode: ObserveMode,
    dagq: &str,
    since: Option<EventId>,
    observation: &str,
    input: &Value,
    limit: usize,
) -> Result<input::Fitted> {
    let window = match (mode, since) {
        (ObserveMode::Daily, _) => {
            "This is the daily observation: the stats cover the runs that finished in the last 24 hours. \
             Look for trends rather than single incidents: failures, resumes or waits that recur across tasks and goals, \
             findings that keep coming back, and whether the problems of earlier findings went away."
                .to_owned()
        }
        (ObserveMode::Hourly, Some(since)) => format!(
            "This is the hourly observation: the stats cover the runs that finished after event {since}."
        ),
        (ObserveMode::Hourly, None) => {
            "This is the first hourly observation: the stats cover the latest finished runs.".to_owned()
        }
    };
    let head = format!(
        "You are the observer of the dagq queue, started headless by the supervisor.\n\
         Your job is to observe whether dagq is running well, not to fix it.\n\
         {window}\n\
         \n\
         Do:\n\
         - Record each problem you see as a finding: `{dagq} finding record --kind <slug> --task ID|--run ID|--goal ID|--queue [--subject '...'] --summary '...' [--detail '...'] [--impact high|normal|low] --evidence EVENT_ID ...` \
           (a lowercase kind such as stall, failure, wait, capacity, threshold, conflict_hotspot; the subject tells problems of one target apart, such as an alert's name, a threshold's name or a file's path; \
           the evidence is the ids of the run events that show it, never written into the text). \
           Recording the same kind, target and subject again updates the existing finding: only new evidence adds an occurrence, so record a finding below again only when there are events it does not hold yet or your reading of it changed.\n\
         - When a finding recurs or weighs enough that a planned change should remedy it (a refactoring of a file that keeps conflicting, a threshold to revisit), add `--propose '<why>'` to its record. A planner the runtime opens makes the proposal; you do not write goals or tasks.\n\
         - When the problem no longer occurs, resolve its finding with the evidence in the reason: `{dagq} finding resolve ID --reason '...'`.\n\
         - Read each problem to a recommendation: what should happen next, and whether a person must do it. \
           When your reading is that waiting clears it or that it is best left alone (leave it, wait), open no ask: record or update its finding only, with that reading in its `--detail`. \
           People read those findings with `{dagq} findings`, not in the inbox.\n\
         - Raise a finding to the inbox as a blocked ask only when your reading needs a person: a person's decision (`--because scope`: the acceptance, the scope, an ADR or a goal's decision; `--because discard`: whether to throw work away), \
           or something a person must do by hand that the runtime and the recovery job cannot (`--because recovery_failed`). \
           Authentication and cost are no blocked ask: the queue's hold ask has them. \
           Write the next moves a person can choose as options, and your reading as the recommended option with how sure you are: \
           `{dagq} ask --kind blocked --because <scope|discard|recovery_failed> --finding ID --question '...' --option '...' --recommend '<one of the options, or propose / dismiss>' --confidence <high|low> [--task ID | --run ID]` \
           (the queue refuses a blocked ask without `--recommend`). \
           One ask per finding stays open: do not ask again when an open ask below already covers it.\n\
         - Read more when needed: `{dagq} findings [ID] [--full]`, `{dagq} stats`, `{dagq} kpi`, `{dagq} marks`, `{dagq} notes`, `{dagq} show ID`, `{dagq} asks`, `{dagq} graph`, `{dagq} forecast`, `{dagq} goal show ID`. \
           `{dagq} observe --input {observation}` lists the sections of this observation's whole input with their bytes, and `--section PATH` reads one as the inputs below were taken (a list {INPUT_PAGE} items at a time from `--offset N`, `--limit N` for more or fewer).\n\
         - Read the record, not prose, for the evidence: `{dagq} events --full` gives each event with its run_id and whole payload, narrowed by `--run ID`, `--task ID`, `--goal ID`, `--kind KIND` (repeatable), `--since TIME` and `--until TIME` (UTC, YYYY-MM-DD or YYYY-MM-DDTHH:MM:SSZ); \
           without `--kind` it lists attention events only, so add `--all` for every kind; it gives the oldest 100 first, so page on with `--after <cursor>` or narrow with `--since`. \
           `{dagq} timeline RUN` gives a run's events oldest first with each gap and its reason (idle, waiting_ask, background, after_receipt, ...). \
           `{dagq} observe --history` gives what each earlier observation read and wrote.\n\
         \n\
         Reading the stalled-session thresholds:\n\
         - stats' `stall_thresholds` has one entry per `[stall]` threshold of a detection (`idle_without_receipt_secs`, `send_confirm_secs`, `background_alert_secs`, `idle_process_secs`; `screen_idle_secs` detects nothing and has none), each with \
           `threshold_secs` (the value now), `detections`, `by_detection` (nudge, recovery, ask, enter_retry, resend, left_to_phase, with their outcomes), `outcomes`, `detected_after_secs` / `resolved_after_secs` (count, median, max), \
           `preempted` (a person stepped in by input or recover before any detection), `by_threshold_secs` (the outcomes per value the detections were made with) and `running_alerts`.\n\
         - Many `answered_wait` outcomes (the answer to the ask was to wait) suggest the threshold is too early; many `preempted` suggest it is too late. \
           `resolved_by_nudge` / `resolved_by_enter` / `resolved_by_resend` show the detection works; `pending` has no outcome yet, so do not judge on it. \
           Compare the outcomes before and after a change of the value with `by_threshold_secs`.\n\
         - An `idle_without_receipt` entry of stats' `running_alerts` with `nudged: false` and `asked: false` past its `threshold` suggests the supervisor missed the stall.\n\
         - When a threshold needs revisiting, record a finding with `--kind threshold --subject <the setting's name>` (a missed detection too, on the run), and add `--propose` when it recurs. \
           You never change the threshold yourself.\n\
         \n\
         Reading the flaky tests:\n\
         - stats' `failed_tests.flaky_candidates` lists the tests that `integrate`'s verification saw fail in `failed_tests.flaky_runs` or more runs, \
           or saw pass when nextest ran them again (`flaky` of 1 or more: marked FLAKY from the first failure), \
           each with its `name`, its counts and `integrate_event_ids` (the `verification_command` events of `integrate` that named it, the newest first).\n\
         - Record each candidate as a finding of kind `flaky_test` on the queue with the test's name as the subject and its `integrate_event_ids` as the evidence: \
           `{dagq} finding record --kind flaky_test --queue --subject '<name>' --summary '...' --evidence <id> ...`. \
           A finding that already holds those events is not recorded again.\n\
         - A test that failed only in the worker's own sessions (`integrate_runs` below `flaky_runs`) is its work in progress, not a flaky test: no finding.\n\
         - When a `flaky_test` finding's test is no longer a candidate and the window's `failed_tests.tests` shows no `integrate` failure of it, you may resolve the finding.\n\
         - stats' `updates.e2e.tests` counts, per e2e test of the automatic update's e2e gate (the gate reruns failed e2e tests once by name, and a test under a mark of `.config/e2e-quarantine.toml` that fails its rerun too is only recorded), \
           `failed` (the gates it failed in), `passed_on_rerun` (flaky), `quarantined` (let through by its mark), `failed_gate` (it failed the gate), `failures_in_a_row` (the latest gates it failed the rerun of in a row; a mark stops holding at 3), \
           `event_ids` (those gates' `update_e2e_passed` / `update_failed` events, the newest first), `marked` and `mark_task` (the task its mark names).\n\
         - Record each e2e test with a `failed` of 2 or more (passed on the rerun or let through by its mark included) as a finding of kind `flaky_test` on the queue with the test's name as the subject and its `event_ids` as the evidence, \
           and add `--propose` when it needs a task to make it stable (it fails again and again, `failed_gate` or `failures_in_a_row` is above 0, or its mark stands): \
           the runtime's planner checks the existing tasks with `search` / `related` (a `mark_task` is one) before it adds one. A finding that already holds those events is not recorded again. \
           The counts cover the stats' window only, so a test that fails twice a day may show it only in the 24 hours read once a day.\n\
         \n\
         Reading the KPIs (`kpi` in the inputs, `{dagq} kpi` for more):\n\
         - `kpi.targets` is each target of `[kpi.targets]` judged on the days and on the weeks: `state` is `ok`, `missed` (off target fewer periods in a row than `kpi.config.breach_periods` days or `breach_weeks` weeks), \
           `breach` (off target that many judged periods in a row, `streak` of them since `breach_since`) or `not_judged` (too few samples or no value); `values` are the periods' values, the latest in progress not judged (`partial`).\n\
         - Each entry of `kpi.breaches` is a target in `breach`: record it as a finding of its `finding_kind` (`kpi`, or `forecast` for the forecast's `forecast.*` KPIs) on the queue with its `subject` (`<kpi>/<stratum>`, without the `forecast.` prefix for a forecast KPI), \
           its `evidence_event_id` (the `kpi_breach_started` event; a breach without one is not recorded by the supervisor yet, so leave it for the next observation) as the evidence, \
           and in the summary and detail the `value`, the target (`min` / `max` of `stat`), the `streak` since `breach_since`, and the `marks` (the changes that took effect since it started), copied as they are: \
           `{dagq} finding record --kind <finding_kind> --queue --subject '<subject>' --summary '...' --detail '...' --evidence <evidence_event_id>`. \
           A breach of the days and of the weeks of one KPI and stratum is one finding, as is a breach that goes on: record it again only with a new event.\n\
         - `missed` and `not_judged` are no findings. When an open `kpi` or `forecast` finding's target is `ok` again, resolve it with the period it came back in: `{dagq} finding resolve ID --reason '...'`.\n\
         - `kpi.trend` lists the periods with their runs, their marks and the KPIs judged worse than the period before (`worsened`); read a change next to its marks, and never compute a number the inputs do not give.\n\
         - `kpi.forecast` lists, per day and per week with a scored snapshot, how the completion forecasts came out against the finishes (`{dagq} forecast` shows the forecast itself): \
           `details` counts the samples, the rows left out and the samples with change marks between the snapshot and the finish; `kpis` holds each `forecast.*` KPI per stratum \
           (`p50_error` and `p50_error_ratio` positive when the finish came later than the p50, `p50_abs_error`, `p90_hit_rate`, `late_rate`, `early_rate`; `marks=0` is the forecast method's own error, the other samples include the plan's changes). \
           A bias that goes on (the p50 always short or long, the p90 hit rate low) is a `forecast` finding only through a breach of a `forecast.*` target in `kpi.breaches`; never judge a bias yourself from these numbers.\n\
         - Add `--propose '<why>'` to a `kpi` or `forecast` finding when its impact and the periods it went on call for a remedy. Never raise a breach as an ask. \
           `improvements` shows the improvement proposals `running` against their `limit` (`[kpi] max_improvement_proposals`); while it is `reached`, a marked finding waits (`waiting`) and no planner is opened for it until one ends.\n\
         \n\
         Do not:\n\
         - Write notes, goals or tasks, resolve individual stalls, answer asks, dismiss findings, or change the state of runs, tasks or goals (ready, cancel, integrate, recover, goal ready/close); the queue refuses those from your environment.\n\
         - Edit files or run anything but the queue commands above.\n\
         \n\
         When you are done, print one line saying how many findings you recorded or updated, how many of them you left as findings without an ask, and how many asks you wrote.\n\
         \n\
         Inputs of observation {observation}: the KPI breaches, the open asks, stats' alerts and running alerts (the required sections, cut only by the whole prompt's limit), \
         then stats, the KPIs, the open and proposed findings, the improvements running, the latest {PROMPT_NOTES} notes and the graph's critical chain and candidates, \
         within {limit} bytes in all and each within its own limits. \
         Each section is JSON, an item or a key a line, and says what it left out and how to read it; never judge on what a section left out without reading it.\n\
         \n",
    );
    Ok(input::fit(
        &head,
        input,
        &Reading {
            observation,
            dagq,
            since: since.map(EventId::as_i64),
        },
        limit,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed observation (task 1223): a Codex one of a role that names
    /// its provider says why Codex cannot be used and joins no hold; one
    /// whose role names none, or whose failure is no reason, says nothing;
    /// a Claude one joins the hold of its wall; a success reads nothing.
    /// The fallback on or off makes no difference to Codex (ADR-t1857-1).
    #[test]
    fn a_failure_holds_claude_or_says_codex_cannot_be_used() {
        let unread = || -> JobFailure { panic!("read Codex's failure") };
        let unread_claude = || -> Option<JobFailure> { panic!("read Claude's failure") };
        for fallback in [true, false] {
            for provider in [Provider::Claude, Provider::Codex] {
                assert_eq!(
                    read_failure(
                        true,
                        provider,
                        (true, fallback),
                        None,
                        unread,
                        unread_claude
                    ),
                    (None, None)
                );
            }
            for (failure, reason) in [
                (JobFailure::UsageLimit, Some(SwitchReason::UsageLimit)),
                (
                    JobFailure::Authentication,
                    Some(SwitchReason::Authentication),
                ),
                (JobFailure::LaunchFailed, Some(SwitchReason::LaunchFailed)),
                (
                    JobFailure::ExecutableMissing,
                    Some(SwitchReason::ExecutableMissing),
                ),
                (JobFailure::Other, None),
            ] {
                assert_eq!(
                    read_failure(
                        false,
                        Provider::Codex,
                        (true, fallback),
                        None,
                        || failure,
                        unread_claude
                    ),
                    (None, reason),
                    "{failure:?}"
                );
            }
            assert_eq!(
                read_failure(
                    false,
                    Provider::Codex,
                    (false, fallback),
                    None,
                    unread,
                    unread_claude
                ),
                (None, None)
            );
            // A role that names no provider: Claude's wall only.
            assert_eq!(
                read_failure(
                    false,
                    Provider::Claude,
                    (false, fallback),
                    None,
                    unread,
                    || { Some(JobFailure::UsageLimit) }
                ),
                (Some(Wall::UsageLimit), None)
            );
            assert_eq!(
                read_failure(
                    false,
                    Provider::Claude,
                    (false, fallback),
                    None,
                    unread,
                    || { Some(JobFailure::LaunchFailed) }
                ),
                (None, None)
            );
            assert_eq!(
                read_failure(
                    false,
                    Provider::Claude,
                    (true, fallback),
                    None,
                    unread,
                    || None
                ),
                (None, None)
            );
        }
        // On, a Claude one of a role that names its provider says nothing
        // of Claude: it joins the hold of its wall as before.
        assert_eq!(
            read_failure(false, Provider::Claude, (true, true), None, unread, || {
                Some(JobFailure::UsageLimit)
            }),
            (Some(Wall::UsageLimit), None)
        );
        assert_eq!(
            read_failure(
                false,
                Provider::Claude,
                (true, true),
                Some(JobFailure::LaunchFailed),
                unread,
                || None
            ),
            (None, None)
        );
    }

    /// With `[provider_fallback] jobs` off a Claude observation of a role
    /// that names its provider says why Claude could not be used: its wall
    /// (which also joins the hold ask) or its start's error; Codex is never
    /// read (ADR-t1857-1).
    #[test]
    fn with_the_fallback_off_a_claude_failure_says_claude_cannot_be_used() {
        let unread = || -> JobFailure { panic!("read Codex's failure") };
        for (wall, reason) in [
            (Wall::UsageLimit, SwitchReason::UsageLimit),
            (Wall::Authentication, SwitchReason::Authentication),
        ] {
            assert_eq!(
                read_failure(false, Provider::Claude, (true, false), None, unread, || {
                    Some(JobFailure::of_wall(wall))
                }),
                (Some(wall), Some(reason))
            );
        }
        for (start, reason) in [
            (JobFailure::LaunchFailed, SwitchReason::LaunchFailed),
            (
                JobFailure::ExecutableMissing,
                SwitchReason::ExecutableMissing,
            ),
        ] {
            assert_eq!(
                read_failure(
                    false,
                    Provider::Claude,
                    (true, false),
                    Some(start),
                    unread,
                    || panic!("read Claude's output after a start that failed")
                ),
                (None, Some(reason))
            );
        }
        assert_eq!(
            read_failure(false, Provider::Claude, (true, false), None, unread, || {
                Some(JobFailure::Other)
            }),
            (None, None)
        );
    }

    /// The error of an agent that did not start is told apart by the
    /// context its host gives it, not by what caused it.
    #[test]
    fn an_agent_that_did_not_start_is_told_by_its_start_context() {
        let failed = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound))
            .context(HeadlessAgent::start_context(OBSERVER_JOB));
        assert!(not_started(&failed, OBSERVER_JOB));
        assert!(!not_started(&failed, "the throughput review"));
        assert!(!not_started(
            &anyhow::anyhow!("the observer did not finish within 5s"),
            OBSERVER_JOB
        ));
    }

    #[test]
    fn the_agent_runs_as_the_observer_with_its_actor_id() {
        assert_eq!(
            observer_actor("s1").env(),
            [
                ("DAGQ_ROLE".to_owned(), "observer".to_owned()),
                ("DAGQ_ACTOR_ID".to_owned(), "observer:s1".to_owned()),
            ]
        );
    }

    #[test]
    fn the_input_carries_the_kpis_and_the_improvements_next_to_the_rest() {
        let input = observer_input(ObserverInput {
            stats: json!({"runs": 3}),
            kpi: json!({"breaches": [{"subject": "lead_time/all", "evidence_event_id": 7}]}),
            findings: json!([]),
            improvements: json!({"running": 2, "limit": 2, "reached": true, "waiting": [{"finding_id": 4}]}),
            notes: json!([]),
            open_asks: json!([]),
            graph: json!({"candidates": [], "critical": []}),
        });
        let keys: Vec<&str> = input
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for key in [
            "stats",
            "kpi",
            "findings",
            "improvements",
            "notes",
            "open_asks",
            "graph",
        ] {
            assert!(keys.contains(&key), "{key} is missing: {input}");
        }
        assert_eq!(input["kpi"]["breaches"][0]["evidence_event_id"], 7);
        assert_eq!(input["improvements"]["reached"], true);
        let prompt = observer_prompt(
            ObserveMode::Daily,
            "dagq",
            None,
            "1791005872",
            &input,
            PROMPT_LIMIT,
        )
        .unwrap()
        .text;
        assert!(prompt.contains("\"lead_time/all\""), "{prompt}");
    }

    #[test]
    fn prompt_explains_how_to_read_the_kpis() {
        let prompt = observer_prompt(
            ObserveMode::Hourly,
            "dagq",
            None,
            "1791005872",
            &json!({"kpi": {}}),
            PROMPT_LIMIT,
        )
        .unwrap()
        .text;
        for text in [
            "`kpi.targets`",
            "`breach` (off target that many judged periods in a row",
            "its `finding_kind` (`kpi`, or `forecast` for the forecast's `forecast.*` KPIs) on the queue with its `subject` (`<kpi>/<stratum>`",
            "`evidence_event_id`",
            "`dagq finding record --kind <finding_kind> --queue --subject '<subject>'",
            "`kpi.forecast` lists",
            "`marks=0` is the forecast method's own error",
            "is a `forecast` finding only through a breach of a `forecast.*` target",
            "`missed` and `not_judged` are no findings",
            "`kpi.trend`",
            "never compute a number the inputs do not give",
            "Never raise a breach as an ask.",
            "`improvements` shows the improvement proposals `running` against their `limit`",
            "`dagq kpi`, `dagq marks`",
        ] {
            assert!(prompt.contains(text), "the prompt lacks {text:?}");
        }
    }

    #[test]
    fn prompt_explains_how_to_read_the_stall_thresholds() {
        let prompt = observer_prompt(
            ObserveMode::Hourly,
            "dagq",
            None,
            "1791005872",
            &json!({"stats": {}}),
            PROMPT_LIMIT,
        )
        .unwrap()
        .text;
        for text in [
            "`stall_thresholds`",
            "`idle_without_receipt_secs`, `send_confirm_secs`, `background_alert_secs`, `idle_process_secs`",
            "`detections`, `by_detection`",
            "`detected_after_secs` / `resolved_after_secs`",
            "`by_threshold_secs`",
            "Many `answered_wait` outcomes (the answer to the ask was to wait) suggest the threshold is too early",
            "many `preempted` suggest it is too late",
            "`resolved_by_nudge` / `resolved_by_enter`",
            "`pending` has no outcome yet",
            "`idle_without_receipt` entry of stats' `running_alerts` with `nudged: false` and `asked: false`",
            "`--kind threshold --subject <the setting's name>`",
            "add `--propose` when it recurs",
            "You never change the threshold yourself.",
        ] {
            assert!(prompt.contains(text), "the prompt lacks {text:?}");
        }
    }

    #[test]
    fn prompt_explains_how_to_record_the_flaky_tests() {
        let prompt = observer_prompt(
            ObserveMode::Hourly,
            "dagq",
            None,
            "1791005872",
            &json!({"stats": {}}),
            PROMPT_LIMIT,
        )
        .unwrap()
        .text;
        for text in [
            "`failed_tests.flaky_candidates`",
            "`integrate_event_ids`",
            "marked FLAKY from the first failure",
            "kind `flaky_test` on the queue with the test's name as the subject",
            "`dagq finding record --kind flaky_test --queue --subject '<name>' --summary '...' --evidence <id> ...`",
            "A finding that already holds those events is not recorded again.",
            "failed only in the worker's own sessions",
            "you may resolve the finding",
            "`updates.e2e.tests`",
            "`passed_on_rerun` (flaky), `quarantined` (let through by its mark), `failed_gate` (it failed the gate), `failures_in_a_row`",
            "Record each e2e test with a `failed` of 2 or more (passed on the rerun or let through by its mark included) as a finding of kind `flaky_test` on the queue",
            "its `event_ids` as the evidence",
            "add `--propose` when it needs a task to make it stable",
            "checks the existing tasks with `search` / `related`",
        ] {
            assert!(prompt.contains(text), "the prompt lacks {text:?}");
        }
    }

    /// ADR-t451-1 decision 2: a wait or leave-it reading stays on the
    /// finding; a blocked ask only for a person's decision or hands, with
    /// the reading as its recommendation and confidence.
    #[test]
    fn prompt_keeps_a_wait_or_leave_it_reading_to_the_finding() {
        let prompt = observer_prompt(
            ObserveMode::Hourly,
            "dagq",
            None,
            "1791005872",
            &json!({"stats": {}}),
            PROMPT_LIMIT,
        )
        .unwrap()
        .text;
        for text in [
            "When your reading is that waiting clears it or that it is best left alone (leave it, wait), open no ask: record or update its finding only, with that reading in its `--detail`.",
            "Raise a finding to the inbox as a blocked ask only when your reading needs a person",
            "`--because scope`",
            "`--because discard`",
            "the runtime and the recovery job cannot (`--because recovery_failed`)",
            "Authentication and cost are no blocked ask",
            "--recommend '<one of the options, or propose / dismiss>' --confidence <high|low>",
            "the queue refuses a blocked ask without `--recommend`",
            "One ask per finding stays open",
            "how many of them you left as findings without an ask",
        ] {
            assert!(prompt.contains(text), "the prompt lacks {text:?}");
        }
        assert!(!prompt.contains("that waiting does not clear) to the inbox"));
    }

    #[test]
    fn prompt_names_the_cli_that_reads_the_record() {
        let prompt = observer_prompt(
            ObserveMode::Hourly,
            "dagq",
            None,
            "1791005872",
            &json!({"stats": {}}),
            PROMPT_LIMIT,
        )
        .unwrap()
        .text;
        assert!(!prompt.contains("events --all"), "{prompt}");
        for text in [
            "`dagq events --full`",
            "`--run ID`, `--task ID`, `--goal ID`, `--kind KIND` (repeatable), `--since TIME` and `--until TIME`",
            "add `--all` for every kind",
            "`dagq timeline RUN`",
            "`dagq observe --history`",
        ] {
            assert!(prompt.contains(text), "the prompt lacks {text:?}");
        }
    }

    /// A big input: many findings, notes and asks, a large `stats` and
    /// KPI breaches with long marks.
    fn big_input() -> Value {
        const IMPACTS: [&str; 3] = ["low", "normal", "high"];
        let text = "x".repeat(2_000);
        observer_input(ObserverInput {
            stats: json!({
                "next_cursor": 99,
                "overall": {"runs": 50},
                "alerts": (0..300).map(|n| json!({"kind": "ask_unanswered", "ask_id": n, "detail": text})).collect::<Vec<_>>(),
                "running_alerts": [],
                "runs": (0..500).map(|n| json!({"run_id": n, "summary": text})).collect::<Vec<_>>(),
            }),
            kpi: json!({
                "config": {"breach_periods": 3},
                "breaches": (0..40).map(|n| json!({"subject": format!("k{n:02}/all"), "streak": n % 4,
                    "marks": (0..150).map(|m| json!({"label": format!("mark {m}")})).collect::<Vec<_>>()})).collect::<Vec<_>>(),
                "trend": {"day": (0..7).map(|_| json!({"label": "d", "marks": (0..100).collect::<Vec<_>>(), "worsened": []})).collect::<Vec<_>>()},
            }),
            findings: Value::Array(
                (0..400)
                    .map(|n| {
                        json!({"id": n, "impact": IMPACTS[n % 3], "occurrences": n % 5,
                                    "summary": text, "evidence": (0..100).collect::<Vec<_>>()})
                    })
                    .collect(),
            ),
            improvements: json!({"running": 2, "limit": 2, "reached": true}),
            notes: Value::Array(
                (0..300)
                    .map(|n| json!({"id": n, "payload": {"text": text}}))
                    .collect(),
            ),
            open_asks: Value::Array(
                (0..200)
                    .map(|n| json!({"id": n, "question": text}))
                    .collect(),
            ),
            graph: json!({"candidates": (0..5_000).collect::<Vec<_>>(), "critical": (0..100).collect::<Vec<_>>()}),
        })
    }

    fn sections(fitted: &input::Fitted) -> std::collections::BTreeMap<&str, &SectionSize> {
        fitted
            .sections
            .iter()
            .map(|section| (section.name.as_str(), section))
            .collect()
    }

    /// ADR-t1566-1 decisions 4 and 5: the whole stays within the limit
    /// with the language instruction, each section within its own, and a
    /// section that left something out says how many and how to read it.
    #[test]
    fn a_big_input_stays_within_the_limits_and_names_what_it_left_out() {
        let input = big_input();
        let fitted = observer_prompt(
            ObserveMode::Hourly,
            "dagq",
            Some(EventId::new(12)),
            "1791005872",
            &input,
            PROMPT_LIMIT,
        )
        .unwrap();
        let language = crate::domain::language::instruction("ja");
        assert!(language.len() + 2 <= LANGUAGE_RESERVE);
        assert!(fitted.text.len() + language.len() + 2 <= PROMPT_LIMIT);
        assert_eq!(
            fitted
                .sections
                .iter()
                .map(|section| section.bytes)
                .sum::<usize>(),
            fitted.text.len()
        );
        let sizes = sections(&fitted);
        // Within each section's own limits (its bytes and the lines around
        // them).
        for (name, items, bytes) in [
            ("stats", usize::MAX, 40_000),
            ("findings", 100, 48_000),
            ("notes", 20, 12_000),
            ("graph.candidates", 200, 4_000),
            ("graph.critical", 50, 2_000),
        ] {
            let size = sizes[name];
            assert!(size.kept <= items, "{size:?}");
            assert!(size.bytes <= bytes + 1_000, "{size:?}");
        }
        for (name, total) in [
            ("findings", 400),
            ("notes", 300),
            ("graph.candidates", 5_000),
            ("stats.alerts", 300),
        ] {
            let size = sizes[name];
            assert_eq!(size.total, total, "{size:?}");
            assert!(size.omitted > 0, "{size:?}");
            assert!(
                fitted.text.contains(&format!(
                    "Left out {} items (from offset {}): read them with `dagq observe --input 1791005872 --section {name} --offset {}`",
                    size.omitted, size.kept, size.kept
                )),
                "{name}: {}",
                fitted.text
            );
        }
        assert!(fitted.text.contains(
            "Left out 1 key (runs): read each with `dagq observe --input 1791005872 --section stats.<key>`, or now with `dagq stats --since 12`."
        ));
        assert!(
            fitted
                .text
                .contains("or now with `dagq findings` and `dagq findings ID --full`")
        );
        // An item keeps the start of its strings and the last of its lists.
        assert!(fitted.text.contains("\"marks_omitted\":140"));
        assert!(fitted.text.contains("… (1700 more characters)"));
        assert!(fitted.text.contains("Strings are cut at 300 characters"));
    }

    /// ADR-t1566-1 decision 4: the order is fixed. The required sections
    /// go first and take the whole's room before any other; the lists go
    /// weightiest or newest first.
    #[test]
    fn the_sections_keep_their_items_in_the_fixed_order() {
        let input = big_input();
        let breaches = input["kpi"]["breaches"].as_array().unwrap();
        assert_eq!(breaches[0]["streak"], 3);
        assert_eq!(breaches[0]["subject"], "k03/all");
        assert_eq!(breaches[39]["streak"], 0);
        let findings = input["findings"].as_array().unwrap();
        assert_eq!(findings[0]["impact"], "high");
        assert_eq!(findings[0]["occurrences"], 4);
        assert!(
            findings[0]["id"].as_i64() > findings[1]["id"].as_i64()
                || findings[0]["occurrences"] != findings[1]["occurrences"]
        );
        assert_eq!(findings[399]["impact"], "low");
        assert_eq!(input["open_asks"][0]["id"], 199);
        let fitted =
            observer_prompt(ObserveMode::Hourly, "dagq", None, "1", &input, PROMPT_LIMIT).unwrap();
        let sizes = sections(&fitted);
        // The breaches and the asks fit whole; the alerts take what is
        // left of the room before stats, which gets none.
        assert_eq!(sizes["kpi.breaches"].omitted, 0);
        assert_eq!(sizes["open_asks"].omitted, 0);
        assert!(sizes["stats.alerts"].kept > 0);
        let position = |text: &str| fitted.text.find(text).unwrap();
        assert!(position("### kpi.breaches") < position("### open_asks"));
        assert!(position("### open_asks") < position("### stats.alerts"));
        assert!(position("### stats.running_alerts") < position("### stats:"));
        assert!(position("### findings") < position("### notes"));
    }

    /// ADR-t1566-1 decisions 3 and 5: when the required sections alone
    /// pass the limit, they are cut too, never silently: the prompt names
    /// the read the observer's `dagq` takes, and the instructions stay
    /// whole.
    #[test]
    fn required_sections_past_the_limit_say_how_to_read_the_rest() {
        let input = big_input();
        let least = observer_prompt(ObserveMode::Daily, "dagq", None, "1", &input, 1).unwrap();
        assert!(
            least
                .sections
                .iter()
                .skip(1)
                .all(|section| section.kept == 0)
        );
        assert!(least.text.starts_with("You are the observer"));
        assert!(least.text.contains("Do not:"));
        let limit = least.text.len() + LANGUAGE_RESERVE + 5_000;
        let fitted = observer_prompt(ObserveMode::Daily, "dagq", None, "1", &input, limit).unwrap();
        assert!(fitted.text.len() + LANGUAGE_RESERVE <= limit);
        let sizes = sections(&fitted);
        let breaches = sizes["kpi.breaches"];
        assert!(breaches.kept > 0 && breaches.omitted > 0, "{breaches:?}");
        assert!(fitted.text.contains(&format!(
            "read them with `dagq observe --input 1 --section kpi.breaches --offset {}`, or now with `dagq kpi`.",
            breaches.kept
        )));
        for name in ["open_asks", "stats.alerts", "findings", "notes"] {
            assert_eq!(sizes[name].kept, 0, "{name}");
            assert!(
                fitted
                    .text
                    .contains(&format!("--section {name} --offset 0`")),
                "{name}"
            );
        }
        assert!(
            fitted.text.contains(
                "`dagq observe --input 1` lists the sections of this observation's whole input"
            ),
            "{}",
            fitted.text
        );
    }

    /// `observe --input` reads the whole input of an observation: its
    /// sections, a list a page at a time within the bytes, and an object
    /// too big as its keys.
    #[test]
    fn the_input_of_an_observation_reads_by_section_and_page() {
        let text = serde_json::to_string(&big_input()).unwrap();
        // Only observation 1791005872 has an input; a name that leaves the
        // observer's directory is refused before anything is read.
        let read_input = |observation: &str, section, offset, limit| {
            read_input(
                observation,
                || match observation {
                    "1791005872" | "../1791005872" => Ok(text.clone()),
                    _ => anyhow::bail!("no such file"),
                },
                section,
                offset,
                limit,
            )
        };
        let index = read_input("1791005872", None, 0, INPUT_PAGE).unwrap();
        assert!(
            index["sections"]
                .as_array()
                .unwrap()
                .iter()
                .any(|section| section["section"] == "stats.runs" && section["items"] == 500),
            "{index}"
        );
        let page = read_input("1791005872", Some("findings"), 398, INPUT_PAGE).unwrap();
        assert_eq!(
            (
                page["total"].clone(),
                page["items"].as_array().unwrap().len()
            ),
            (json!(400), 2)
        );
        assert_eq!(page["next_offset"], Value::Null);
        let page = read_input("1791005872", Some("notes"), 0, 100).unwrap();
        assert!(page["items"].as_array().unwrap().len() < 100);
        assert!(serde_json::to_string(&page["items"]).unwrap().len() <= input::INPUT_READ_BYTES);
        assert_eq!(page["next_offset"], page["items"].as_array().unwrap().len());
        let stats = read_input("1791005872", Some("stats"), 0, INPUT_PAGE).unwrap();
        assert!(
            stats["keys"]
                .as_array()
                .unwrap()
                .iter()
                .any(|key| key["section"] == "stats.runs")
        );
        let cursor = read_input("1791005872", Some("stats.next_cursor"), 0, INPUT_PAGE).unwrap();
        assert_eq!(cursor["value"], 99);
        assert!(read_input("1791005872", Some("stats.nothing"), 0, 1).is_err());
        assert!(read_input("1791005873", None, 0, 1).is_err());
        assert!(read_input("../1791005872", None, 0, 1).is_err());
    }

    /// `observe --history` gives what each observation's prompt came to,
    /// from its `observe_started` (task 1567).
    #[test]
    fn the_history_gives_the_prompt_bytes_of_an_observation() {
        let event = |kind: EventKind, payload: Value| RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.as_str().to_owned(),
            payload,
            created_at: "2026-10-03T00:00:00Z".to_owned(),
            actor: None,
        };
        let started = event(
            EventKind::ObserveStarted,
            json!({"dir": "/q/observer/1", "prompt_bytes": 120, "prompt_limit": PROMPT_LIMIT,
                   "prompt_sections": [{"name": "instructions", "bytes": 120, "total": 0, "kept": 0, "omitted": 0}]}),
        );
        let finished = event(
            EventKind::ObserveFinished,
            json!({"outcome": "error", "dir": "/q/observer/1"}),
        );
        let entry = history_entry(&finished, Some(&started));
        assert_eq!(entry["prompt_bytes"], 120);
        assert_eq!(entry["prompt_limit"], PROMPT_LIMIT);
        assert_eq!(entry["prompt_sections"][0]["name"], "instructions");
        assert_eq!(history_entry(&finished, None)["prompt_bytes"], Value::Null);
    }

    /// Observations in memory, for `observe --history`.
    struct Observations(Vec<(RunEvent, Option<RunEvent>)>);

    impl ObserverLog for Observations {
        fn event_id_before(&self, _unix: i64) -> Result<EventId> {
            unreachable!()
        }
        fn ask_high_water(&self) -> Result<AskId> {
            unreachable!()
        }
        fn written_by(
            &self,
            _role: &str,
            _event: EventId,
            _ask: AskId,
        ) -> Result<crate::application::WrittenBy> {
            unreachable!()
        }
        fn last_observation(&self, _mode: &str) -> Result<Option<(EventId, Value)>> {
            unreachable!()
        }
        fn events_besides(&self, _role: &str, _span: &str, _after: EventId) -> Result<i64> {
            unreachable!()
        }
        fn observations(&self, limit: usize) -> Result<Vec<(RunEvent, Option<RunEvent>)>> {
            Ok(self.0.iter().take(limit).cloned().collect())
        }
    }

    /// `observe --history` lists what the store gives, a skipped one
    /// marked so with its start at its finish.
    #[test]
    fn the_history_lists_the_observations_the_store_gives() {
        let event = |id: i64, kind: EventKind, payload: Value| RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.as_str().to_owned(),
            payload,
            created_at: format!("2026-10-04T00:00:0{id}Z"),
            actor: None,
        };
        let store = Observations(vec![
            (
                event(
                    3,
                    EventKind::ObserveFinished,
                    json!({"outcome": "skipped", "mode": "hourly", "since": 1, "cursor": 1}),
                ),
                None,
            ),
            (
                event(
                    2,
                    EventKind::ObserveFinished,
                    json!({"outcome": "succeeded", "ask_ids": [7], "asks": 1}),
                ),
                Some(event(1, EventKind::ObserveStarted, json!({}))),
            ),
        ]);
        let newest = history(&store, 1).unwrap();
        let observations = newest["observations"].as_array().unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0]["skipped"], true);
        assert_eq!(observations[0]["started_at"], "2026-10-04T00:00:03Z");
        assert_eq!(observations[0]["input"], json!({"since": 1, "through": 1}));
        let all = history(&store, 10).unwrap();
        let second = &all["observations"][1];
        assert_eq!(second["skipped"], false);
        assert_eq!(second["started_at"], "2026-10-04T00:00:01Z");
        assert_eq!(second["asks"], json!({"count": 1, "ids": [7]}));
    }
}
