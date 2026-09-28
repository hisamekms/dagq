//! The observer job (ADR-0044 decision 4): a headless agent run that reads
//! `stats`, the unsettled findings, the latest notes, the open asks and the
//! dependency graph, and may write only findings (ADR-0044 decision 18) and
//! `blocked` asks. The CLI refuses everything else under
//! `DAGQ_ROLE=observer`, notes and goals included. The supervisor starts it on
//! a timer (`--observe-interval`, `--observe-daily`); `observe` starts it
//! by hand. An observation that finds no event since the last one but its
//! own starts no agent and records a skipped `observe_finished`; the agent
//! loads no MCP server; `observe --history` reads what each observation
//! read and wrote.
use crate::domain::EventKind;
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::{
    application::{
        AgentProvider, ProcessControl, Streams, TaskStore,
        actor_executor::{
            ActorExecutionSpec, ActorExecutor, ActorProgram, HeadlessProgram, HostActorExecutor,
            WorkspaceAccess,
        },
        dependency_graph,
    },
    domain::{
        ActorContext, ActorRole, AskId, EventId, FindingQuery, NewHold, NoteQuery, RunEvent,
        actor_model::{ActorLaunch, ModelRole},
        queue_hold::{HoldJob, Wall},
        stats::StatsQuery,
    },
    infrastructure::{
        adapters::{SystemProcesses, shell_join},
        asks::AskQuery,
        process::LocalSpawner,
        sqlite::SqliteQueue,
    },
    lifecycle::OBSERVER_ROLE,
};

/// Observations `observe --history` lists by default.
pub const HISTORY_LIMIT: usize = 20;
/// Notes the prompt carries.
pub const PROMPT_NOTES: usize = 20;
/// How long one observer run may take before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The tools the observer may use beyond reading: the queue CLI only.
pub const ALLOWED_TOOLS: &[&str] = &["Bash(dagq:*)"];

/// The supervisor starts the observations on its timer, so their modes
/// belong to its use case.
pub use crate::application::supervise::{DAILY_WINDOW_SECS, ObserveMode};

#[derive(Debug, Clone)]
pub struct ObserveOptions {
    pub mode: ObserveMode,
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
}

/// `<queue dir>/observer`: one directory per observation and the cursor.
pub fn observer_dir(db: &Path) -> PathBuf {
    db.parent().unwrap_or(Path::new(".")).join("observer")
}

/// The hourly observation's cursor, if one was saved.
pub fn read_cursor(db: &Path) -> Result<Option<EventId>> {
    let path = observer_dir(db).join("cursor");
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(EventId::new(text.trim().parse().with_context(
            || format!("parse the observer cursor in {}", path.display()),
        )?))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn write_cursor(db: &Path, cursor: EventId) -> Result<()> {
    let dir = observer_dir(db);
    fs::create_dir_all(&dir)?;
    let temporary = dir.join(format!(".cursor.{}.tmp", std::process::id()));
    fs::write(&temporary, format!("{cursor}\n"))?;
    fs::rename(&temporary, dir.join("cursor"))?;
    Ok(())
}

/// Run one observation: gather the inputs, start the agent headless with
/// `DAGQ_ROLE=observer`, wait for it, then record `observe_finished` with
/// what it wrote and, for a succeeded hourly one, save the new cursor.
pub fn observe(db: &Path, provider: &dyn AgentProvider, options: &ObserveOptions) -> Result<Value> {
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let mut queue = SqliteQueue::open(&db)?;
    let started = queue.generators().clock.now();
    let since = match (options.since, options.mode) {
        (Some(since), _) => Some(since),
        (None, ObserveMode::Hourly) => read_cursor(&db)?,
        (None, ObserveMode::Daily) => Some(queue.event_id_before(started - DAILY_WINDOW_SECS)?),
    };
    // A cursor given by hand asks to read past it whatever happened since.
    if options.since.is_none()
        && !options.dry_run
        && let Some(payload) = skip(&queue, options.mode, since)?
    {
        return Ok(payload);
    }
    // The cmux on PATH lists the workspaces for `workspace_mismatch`;
    // without one, only that alert is left unjudged.
    let cmux = crate::infrastructure::adapters::executable(Path::new("cmux"))
        .ok()
        .map(|executable| crate::infrastructure::adapters::Cmux { executable });
    let one_shot = crate::compose::OneShot::new(queue.generators().clone());
    let stats = one_shot.stats_of(
        &queue,
        &db,
        &StatsQuery {
            since: since.map(Into::into),
            ..StatsQuery::default()
        },
        cmux.as_ref()
            .map(|cmux| cmux as &dyn crate::application::stats::WorkspaceListing),
    )?;
    let cursor = EventId::new(stats["next_cursor"].as_i64().unwrap_or_default());
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
    let kpi = one_shot
        .observer_kpi(&queue, &db)
        .unwrap_or_else(|error| json!({"error": format!("{error:#}")}));
    let improvements = one_shot
        .improvements_of(&queue)
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
    let command = shell_join(&[
        "dagq".into(),
        "--db".into(),
        db.to_string_lossy().into_owned(),
    ]);
    let checkout = crate::compose::bound_checkout(&queue)?;
    let language = crate::infrastructure::language::language_for_prompt(
        checkout.as_deref(),
        options.user_config.as_deref(),
    );
    let prompt = crate::domain::language::with_instruction(
        observer_prompt(options.mode, &command, since, &input)?,
        language.as_ref(),
    );
    if options.dry_run {
        return Ok(json!({
            "dry_run": true,
            "mode": options.mode.as_str(),
            "since": since,
            "cursor": cursor,
            "prompt": prompt,
        }));
    }
    let dir = observation_dir(&db, started)?;
    fs::write(dir.join("prompt.md"), &prompt)?;
    fs::write(
        dir.join("input.json"),
        serde_json::to_string_pretty(&input)?,
    )?;
    let ask_mark = queue.ask_high_water()?;
    // The job's Claude session id (ADR-0048 decision 4).
    let session_id = uuid::Uuid::new_v4().to_string();
    let launch = observer_launch(checkout.as_deref());
    let event_mark = queue.record_queue_event(
        EventKind::ObserveStarted,
        json!({"mode": options.mode.as_str(), "since": since, "dir": dir, "session_id": session_id, "launch": launch.to_value()}),
    )?;
    tracing::info!(
        mode = options.mode.as_str(),
        since = since.map(EventId::as_i64),
        "observer ({}) started",
        options.mode.as_str()
    );
    let clock = Instant::now();
    // `failed`: the agent exited non-zero or by a signal; `error`: it could
    // not start or ran past the timeout.
    let (outcome, exit_code, error) =
        match run_agent(provider, &db, &dir, &prompt, &session_id, &launch, options) {
            Ok(Some(0)) => ("succeeded", Some(0), None),
            Ok(code) => ("failed", code, None),
            Err(error) => ("error", None, Some(format!("{error:#}"))),
        };
    // An agent stopped at a login that ran out or the usage limit joins
    // the queue's hold ask (ADR-0047 decision 42, task 438): no observer
    // starts again until a person answers it.
    let wall = if outcome == "succeeded" {
        None
    } else {
        let output = fs::read_to_string(dir.join("output.log")).unwrap_or_default();
        crate::infrastructure::claude::job_wall(&output)
    };
    // A hold that could not be written is logged: the observation's
    // finish is recorded either way.
    let hold = wall.and_then(|wall| {
        hold_wall(&mut queue, wall, checkout.as_deref(), cmux.as_ref())
            .inspect_err(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "the observer stopped at the {} wall, and its hold ask could not be written: {error:#}", wall.as_str());
            })
            .ok()
    });
    let written = queue.written_by(OBSERVER_ROLE, event_mark, ask_mark)?;
    let (recorded, updated, closed, asks) = (
        written.recorded.len(),
        written.updated.len(),
        written.closed.len(),
        written.asks.len(),
    );
    let saved = outcome == "succeeded" && options.mode == ObserveMode::Hourly;
    if saved {
        write_cursor(&db, cursor)?;
    }
    let payload = json!({
        "mode": options.mode.as_str(),
        "outcome": outcome,
        "exit_code": exit_code,
        "error": error,
        "since": since,
        "cursor": cursor,
        "cursor_saved": saved,
        "findings_recorded": recorded,
        "findings_updated": updated,
        "findings_closed": closed,
        "asks": asks,
        "recorded_finding_ids": written.recorded,
        "updated_finding_ids": written.updated,
        "closed_finding_ids": written.closed,
        "ask_ids": written.asks,
        "duration_secs": clock.elapsed().as_secs(),
        "dir": dir,
        "wall": wall.map(Wall::as_str),
        "hold_ask_id": hold,
    });
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
        "observer ({}) finished: {outcome}",
        options.mode.as_str()
    );
    Ok(payload)
}

/// Add the observer to the hold ask of `wall`, or open it (notifying the
/// inbox through `cmux` when there is one), and record `auth_required` or
/// `usage_limited` on the queue when it joined. Returns the ask's ID.
fn hold_wall(
    queue: &mut SqliteQueue,
    wall: Wall,
    checkout: Option<&Path>,
    cmux: Option<&crate::infrastructure::adapters::Cmux>,
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
    /// [`crate::compose::OneShot::observer_kpi`], or `{"error": ...}`.
    pub kpi: Value,
    pub findings: Value,
    /// [`crate::compose::OneShot::improvements_of`], or `{"error": ...}`.
    pub improvements: Value,
    pub notes: Value,
    pub open_asks: Value,
    pub graph: Value,
}

/// The input the prompt carries and `input.json` keeps: `stats`, the KPIs
/// (ADR-0051 decision 24), the unsettled findings with the improvements
/// running and their limit (decision 25), the notes, the open asks and
/// the graph.
pub fn observer_input(input: ObserverInput) -> Value {
    json!({
        "stats": input.stats,
        "kpi": input.kpi,
        "findings": input.findings,
        "improvements": input.improvements,
        "notes": input.notes,
        "open_asks": input.open_asks,
        "graph": input.graph,
    })
}

/// When the last observation of `mode` that ran its agent succeeded and no
/// event but the observer's own came after the events it read, record a skipped
/// `observe_finished` without starting anything, and return its payload.
fn skip(queue: &SqliteQueue, mode: ObserveMode, since: Option<EventId>) -> Result<Option<Value>> {
    let Some((previous, last)) = queue.last_observation(mode.as_str())? else {
        return Ok(None);
    };
    // Counted past what its input read, not past its finish: events of
    // others while its agent ran are unread too.
    let read = last["cursor"].as_i64().map_or(previous, EventId::new);
    if last["outcome"] != "succeeded"
        || queue.events_besides(OBSERVER_ROLE, crate::domain::sessions::OBSERVER, read)? > 0
    {
        return Ok(None);
    }
    let payload = json!({
        "mode": mode.as_str(),
        "outcome": "skipped",
        "reason": "no events but the observer's own since the last observation",
        "previous_event_id": previous,
        "since": since,
        "cursor": since,
        "cursor_saved": false,
        "findings_recorded": 0,
        "findings_updated": 0,
        "findings_closed": 0,
        "asks": 0,
        "recorded_finding_ids": [],
        "updated_finding_ids": [],
        "closed_finding_ids": [],
        "ask_ids": [],
        "duration_secs": 0,
        "dir": null,
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
pub fn history(queue: &SqliteQueue, limit: usize) -> Result<Value> {
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
        "exit_code": field("exit_code"),
        "error": field("error"),
        "dir": field("dir"),
    })
}

/// `<queue dir>/observer/<started_at>/`, suffixed when one already exists
/// for that second.
fn observation_dir(db: &Path, started: i64) -> Result<PathBuf> {
    let root = observer_dir(db);
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    for n in 0.. {
        let name = if n == 0 {
            started.to_string()
        } else {
            format!("{started}-{n}")
        };
        let dir = root.join(name);
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("create {}", dir.display())),
        }
    }
    unreachable!("the suffixes do not run out")
}

/// What the observer's agent starts with (ADR-0079 decision 7):
/// `[roles.observer]` of the bound checkout's `dagq.toml`; none, no
/// checkout, or a file that cannot be read starts it as before.
fn observer_launch(checkout: Option<&Path>) -> ActorLaunch {
    let Some(checkout) = checkout else {
        return ActorLaunch::default_of(ModelRole::Observer);
    };
    match crate::infrastructure::run_env::load_role_models(checkout) {
        Ok(models) => models.launch(ModelRole::Observer),
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "[roles.observer] could not be read; starting it as before: {error:#}");
            ActorLaunch::default_of(ModelRole::Observer)
        }
    }
}

/// The actor the observer's agent runs as (ADR-t728-1 decision 4):
/// `observer`, one actor per session.
fn observer_actor(session_id: &str) -> ActorContext {
    ActorContext::instance(ActorRole::Observer, session_id)
}

/// Start the agent in `dir` with its output in `output.log`, and wait for
/// it up to the timeout (then kill it and what it started, so no Bash
/// child of the agent outlives it: an error). The exit code, or `None`
/// when a signal ended it.
fn run_agent(
    provider: &dyn AgentProvider,
    db: &Path,
    dir: &Path,
    prompt: &str,
    session_id: &str,
    launch: &ActorLaunch,
    options: &ObserveOptions,
) -> Result<Option<i32>> {
    let log = dir.join("output.log");
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    if let Some(bin) = options.dagq.parent() {
        let mut paths = vec![bin.to_path_buf()];
        paths.extend(std::env::split_paths(&path));
        path = std::env::join_paths(paths)?;
    }
    let path = path
        .into_string()
        .map_err(|_| anyhow::anyhow!("PATH is not UTF-8"))?;
    let mut child = HostActorExecutor::new(db)
        .with_provider(provider)
        .with_spawner(&LocalSpawner)
        .spawn(
            ActorExecutionSpec::new(
                observer_actor(session_id),
                WorkspaceAccess::Scratch(dir.to_path_buf()),
                ActorProgram::Headless {
                    program: HeadlessProgram::Job {
                        cwd: dir,
                        prompt,
                        allowed_tools: ALLOWED_TOOLS,
                    },
                    session_id: Some(session_id),
                    launch: Some(launch),
                    without_mcp: true,
                    env: vec![("PATH".to_owned(), path)],
                    streams: Streams::Log(&log),
                },
            )
            .with_timeout(options.timeout),
        )
        .context("start the observer agent")?
        .process()?;
    let deadline = Instant::now() + options.timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code);
        }
        if Instant::now() >= deadline {
            // Listed before the kill: once the agent is gone, its children
            // are no longer its descendants.
            let descendants = SystemProcesses.descendants(child.id());
            let _ = child.kill();
            let _ = child.wait();
            for pid in descendants {
                let _ = SystemProcesses.kill(pid);
            }
            anyhow::bail!(
                "the observer did not finish within {}s",
                options.timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// The observer's instructions and inputs.
pub fn observer_prompt(
    mode: ObserveMode,
    dagq: &str,
    since: Option<EventId>,
    input: &Value,
) -> Result<String> {
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
    Ok(format!(
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
         - Raise what needs a person now (an alert past its threshold that waiting does not clear) to the inbox as a blocked ask on its finding: `{dagq} ask --kind blocked --because <scope|discard|recovery_failed> --finding ID --question '...' --option '...' [--task ID | --run ID]`, with your reading of it and the next moves a person can choose as options. \
           One ask per finding stays open: do not ask again when an open ask below already covers it.\n\
         - Read more when needed: `{dagq} findings [ID] [--full]`, `{dagq} stats`, `{dagq} kpi`, `{dagq} marks`, `{dagq} notes`, `{dagq} show ID`, `{dagq} asks`, `{dagq} graph`, `{dagq} forecast`, `{dagq} goal show ID`.\n\
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
         When you are done, print one line saying how many findings you recorded or updated and how many asks you wrote.\n\
         \n\
         Inputs (JSON: stats, the KPIs, the open and proposed findings, the improvements running, the latest {PROMPT_NOTES} notes, the open asks, and the graph's candidates and critical chain):\n\
         ```json\n{}\n```\n",
        serde_json::to_string_pretty(input)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let prompt = observer_prompt(ObserveMode::Daily, "dagq", None, &input).unwrap();
        assert!(prompt.contains("\"lead_time/all\""), "{prompt}");
    }

    #[test]
    fn prompt_explains_how_to_read_the_kpis() {
        let prompt =
            observer_prompt(ObserveMode::Hourly, "dagq", None, &json!({"kpi": {}})).unwrap();
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
        let prompt =
            observer_prompt(ObserveMode::Hourly, "dagq", None, &json!({"stats": {}})).unwrap();
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
        let prompt =
            observer_prompt(ObserveMode::Hourly, "dagq", None, &json!({"stats": {}})).unwrap();
        for text in [
            "`failed_tests.flaky_candidates`",
            "`integrate_event_ids`",
            "marked FLAKY from the first failure",
            "kind `flaky_test` on the queue with the test's name as the subject",
            "`dagq finding record --kind flaky_test --queue --subject '<name>' --summary '...' --evidence <id> ...`",
            "A finding that already holds those events is not recorded again.",
            "failed only in the worker's own sessions",
            "you may resolve the finding",
        ] {
            assert!(prompt.contains(text), "the prompt lacks {text:?}");
        }
    }

    #[test]
    fn prompt_names_the_cli_that_reads_the_record() {
        let prompt =
            observer_prompt(ObserveMode::Hourly, "dagq", None, &json!({"stats": {}})).unwrap();
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
}
