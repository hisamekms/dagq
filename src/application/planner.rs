//! Planner sessions (ADR-0041 decisions 1, 6, 12, 13): planners that write
//! goals and tasks and submit them as a proposal. Only the runtime opens
//! one (ADR-t1394-1): for a proposal plan review sent back while its own
//! planner was closed ([`open_runtime_planner`]), and for a draft, a
//! finding or a planning request the inbox recorded
//! ([`open_draft_planner`]). `dagq plan`, which opened a planner a person
//! talked with, is refused with [`PLAN_REFUSED`]; the row of a person's
//! planner opened before is closed without cmux, its workspace left to the
//! person ([`close_person_planners`], ADR-t1433-2 decision 5). Each is a
//! `planners` row with its
//! own directory under the queue's `planners/` (its prompt, the wrapper
//! binary, its `turns/`, log and idle marker).
//!
//! A planner of the runtime's runs headless only (ADR-t1433-2 decision 3):
//! its session wrapper `planner-session --headless` starts as a process
//! detached from the supervisor, without a workspace (ADR-t1404-1 decision
//! 8), registers itself, and runs its agent one call per turn the way a
//! headless worker does ([`super::headless_session`]): its supervisor
//! writes requests to its `turns/`, and its wrapper writes the idle marker
//! when a turn ends. `[roles.runtime_planner] route` and `[headless]
//! wrapper` of `dagq.toml` choose nothing for it any more. [`planner_view`]
//! judges from these whether the planner is alive and idle.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, UNIX_EPOCH},
};
use tracing::warn;

use super::{
    AgentProvider, AgentSignals, Clock, PlannerCommand, ProcessControl, Queue, RunFiles, Spawner,
    WorkspaceBackend,
    actor_executor::{
        ActorExecutionSpec, ActorExecutor, ActorProgram, HostActorExecutor, SessionAgent,
        WorkspaceAccess,
    },
    headless_session::{TurnOwner, Turns},
    naming::shell_join,
    path_text, planner_idle_marker,
    prompt::{FittedPrompt, runtime_planner_prompt},
    screen_idle::{self, MarkerState},
    session::{OwnWorkspace, wrapper_refused},
};
use crate::domain::{
    ActorContext, IdleProbe, PlannerCloseCode, PlannerId, PlannerOrigin, PlannerProbe,
    PlannerRoute, PlannerSession, PlannerState, ProposalId, Task,
    actor_model::{ActorLaunch, ModelRole, REVISE_ESCALATION, RoleModels},
    background_wrapper::{BACKGROUND_FLAG, BackgroundSession},
    language::Language,
    turn::{self, LIMITS_FILE, TurnLimits, TurnMark, exit_path, request_path, turns_dir},
};

/// The planner's first message, which its wrapper hands the agent.
pub const PLANNER_PROMPT_FILE: &str = "prompt.txt";
/// The agent's debug log in the planner's directory, where a failed idle
/// hook shows (ADR-t803-1).
pub const PLANNER_DEBUG_LOG: &str = "claude.log";
/// The snapshot of the binary the planner's wrapper runs, so rebuilding the
/// binary does not change a running one.
pub const PLANNER_RUNNER_FILE: &str = "runner";
/// The log of a planner's wrapper, which starts in the background
/// (ADR-t1404-1 decision 6), in the planner's directory.
pub const PLANNER_SESSION_LOG: &str = "session.log";
/// The flag of `planner-session` that runs the planner's agent one call
/// per turn (ADR-t1394-2 decision 2).
pub const HEADLESS_FLAG: &str = "--headless";

/// The directory of planner `id` under the queue's `planners/` directory.
pub fn planner_dir(planners_dir: &Path, id: PlannerId) -> PathBuf {
    planners_dir.join(id.to_string())
}

/// What opening a planner of the runtime's works with: the queue, the
/// backend that starts its wrapper in the background
/// ([`WorkspaceBackend::launch_background`]; no workspace is opened), the
/// files, the queue's database and `planners/` directory, the checkout the
/// planner works in, and what its wrapper runs (this binary, the agent's
/// executable, the plugin directory it loads).
pub struct PlannerLaunch<'a> {
    pub queue: &'a dyn Queue,
    pub backend: &'a dyn WorkspaceBackend,
    pub files: &'a dyn RunFiles,
    pub db: &'a Path,
    pub planners_dir: &'a Path,
    pub repo_root: &'a Path,
    pub runner: &'a Path,
    pub claude: &'a Path,
    pub plugin_dir: Option<&'a Path>,
    /// The language every planner's prompt names (ADR-t616-2), resolved
    /// when the launch is made; `None` names none.
    pub language: Option<Language>,
    /// `[roles.<role>]` of `dagq.toml` (ADR-0079 decision 7), read when
    /// the launch is made: the model and effort a planner's agent starts
    /// with.
    pub roles: RoleModels,
    /// The limits of a planner's turns, from the supervisor's `[stall]`
    /// settings, as a headless worker's.
    pub turn_limits: TurnLimits,
}

/// A planner whose wrapper just started: its record (with the handle of
/// its background wrapper), its directory, and what its agent starts with.
#[derive(Debug, Clone, Serialize)]
pub struct OpenedPlanner {
    pub planner: PlannerSession,
    pub dir: PathBuf,
    /// What its agent starts with (ADR-0079 decision 7).
    pub launch: ActorLaunch,
}

/// Why `dagq plan` opens nothing (ADR-t1394-1 decision 1), and where a
/// person goes instead: the inbox, which records a planning request the
/// runtime opens a planner for.
pub const PLAN_REFUSED: &str = "dagq plan no longer opens a planner: planners a person opens were \
abolished (ADR-t1394-1), and planning goes through the inbox. Ask the inbox for the plan in your \
own words; it records them as a planning request (`dagq request add --text '...'`, which a person \
at a terminal without DAGQ_ROLE may run too), and the supervisor opens a planner of the runtime's \
for it, which submits a proposal or declines the request with a reason (follow it with \
`dagq requests`). A planner already open keeps running until it ends (`dagq planners`); no planner \
was opened";

/// Open a planner of the runtime's for `proposal` (ADR-0041 decision 12):
/// its initial prompt carries the proposal, its `tasks` (as the caller read
/// them) and the `reasons` plan review sent it back with. It submits as the
/// runtime's planner (`DAGQ_PLANNER_ORIGIN=runtime`), which makes it the
/// proposal's owner. Being opened for a plan review's `revise`, its agent
/// starts one effort step above `[roles.runtime_planner]` (or `medium`),
/// up to `xhigh` (ADR-0079 decision 7 (c)).
pub fn open_runtime_planner(
    launch: &PlannerLaunch<'_>,
    proposal: ProposalId,
    tasks: &[Task],
    reasons: &[String],
) -> Result<OpenedPlanner> {
    // The proposal must exist; its record is the one the planner opens for.
    launch.queue.show_proposal(proposal)?;
    // Reopens have a proposal of their own: the finished review belongs
    // to the proposal that reopened them, not to their current tasks.
    let review_anchor = launch
        .queue
        .latest_events_of("plan_review_finished", usize::MAX >> 1)?
        .into_iter()
        .find(|event| {
            event.payload["proposal_id"] == serde_json::json!(proposal)
                || event.payload["reopened"]
                    .as_array()
                    .is_some_and(|reopened| {
                        reopened
                            .iter()
                            .any(|task| task["proposal_id"] == serde_json::json!(proposal))
                    })
        })
        .and_then(|event| event.task_id);
    let prompt = runtime_planner_prompt(launch.db, proposal, tasks, reasons, review_anchor)?;
    let actor = launch
        .roles
        .launch(ModelRole::RuntimePlanner)
        .escalated(REVISE_ESCALATION);
    let planner = launch
        .queue
        .open_planner(PlannerOrigin::Runtime, Some(proposal))?;
    launch_planner(launch, planner, ("runtime", &prompt), &actor)
}

/// Start the wrapper of a planner the runtime recorded for a draft
/// (ADR-0041 decision 16, [`super::DraftPlannerStore::open_draft_planner`]),
/// a finding or a planning request (`kind`: `draft`, `finding`, `request`)
/// with `prompt`, its agent starting with `[roles.runtime_planner]`.
pub fn open_draft_planner(
    launch: &PlannerLaunch<'_>,
    planner: PlannerSession,
    (kind, prompt): (&'static str, &FittedPrompt),
) -> Result<OpenedPlanner> {
    let actor = launch.roles.launch(ModelRole::RuntimePlanner);
    launch_planner(launch, planner, (kind, prompt), &actor)
}

/// Record `planner` headless, write its directory and start its wrapper in
/// the background with `prompt` as its first turn (ADR-t1394-2 decision 2,
/// ADR-t1433-2 decision 3), with `DAGQ_ROLE=planner`, `DAGQ_QUEUE`, the
/// origin and the planner's ID in the wrapper's environment, and record
/// the wrapper's handle as the planner's session. A wrapper that does not
/// start, or whose handle cannot be recorded, closes the record with the
/// error. The bytes of the prompt the runtime held to its limits (with
/// its kind) are recorded as `planner_prompt_written` (task 1571,
/// ADR-t1566-1 decision 6). Its agent starts as `actor` says.
fn launch_planner(
    launch: &PlannerLaunch<'_>,
    planner: PlannerSession,
    (kind, prompt): (&'static str, &FittedPrompt),
    actor: &ActorLaunch,
) -> Result<OpenedPlanner> {
    let queue = launch.queue;
    // A planner of the runtime's runs headless only, whatever `dagq.toml`
    // says of its route (ADR-t1433-2 decision 3).
    if let Err(error) = queue.set_planner_route(planner.id, PlannerRoute::Headless) {
        queue.close_planner(planner.id, Some(&format!("{error:#}")))?;
        return Err(error);
    }
    let planner = queue.planner(planner.id)?;
    let dir = planner_dir(launch.planners_dir, planner.id);
    let prompt = prompt.clone().with_language(launch.language.as_ref());
    record_prompt_bytes(queue, &planner, kind, &prompt);
    let handle = match start_wrapper(launch, &planner, &dir, &prompt.text, actor) {
        Ok(handle) => handle,
        Err(error) => {
            queue.close_planner(planner.id, Some(&format!("{error:#}")))?;
            return Err(error);
        }
    };
    if let Err(error) = queue.planner_workspace_created(planner.id, &handle) {
        // Unrecorded, the wrapper would run with nothing to find it by,
        // and it is refused (task 806); its stop is recorded as
        // `wrapper_stopped` (task 1657).
        let error = match super::supervise::stop_session(
            launch.backend,
            &handle,
            crate::domain::background_wrapper::StopRoute::Planner,
        ) {
            Ok(()) => error.context(format!(
                "the planner's wrapper {handle} could not be recorded and was stopped"
            )),
            Err(stop) => error.context(format!(
                "the planner's wrapper {handle} could not be recorded, and stopping it failed: {stop:#}"
            )),
        };
        queue.close_planner(planner.id, Some(&format!("{error:#}")))?;
        return Err(error);
    }
    Ok(OpenedPlanner {
        planner: queue.planner(planner.id)?,
        dir,
        launch: actor.clone(),
    })
}

/// Record what the prompt of a planner of the runtime's takes as
/// `planner_prompt_written` (task 1571, ADR-t1566-1 decision 6); a record
/// that fails is logged and does not keep the planner from opening.
fn record_prompt_bytes(
    queue: &dyn super::Queue,
    planner: &PlannerSession,
    kind: &str,
    prompt: &FittedPrompt,
) {
    let recorded = queue.record_queue_event(
        crate::domain::EventKind::PlannerPromptWritten,
        serde_json::json!({
            "planner_id": planner.id,
            "subject": "planner",
            "prompt": kind,
            "prompt_bytes": prompt.bytes,
        }),
    );
    if let Err(error) = recorded {
        warn!(error = %format_args!("{error:#}"), "planner {}: what its prompt takes could not be recorded: {error:#}", planner.id);
    }
}

/// Write the directory `dir` of `planner` (its prompt, its turn limits, a
/// snapshot of the runner) and start its wrapper in the background; the
/// wrapper's handle.
fn start_wrapper(
    launch: &PlannerLaunch<'_>,
    planner: &PlannerSession,
    dir: &Path,
    prompt: &str,
    actor: &ActorLaunch,
) -> Result<String> {
    let files = launch.files;
    files
        .create_dir_all(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    // Planner IDs start over with a new queue database, so the directory
    // may be one an earlier planner of the same ID left: its idle marker
    // would make this planner look idle before its agent ever stopped.
    let marker = planner_idle_marker(dir);
    match files.remove_file(&marker) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("remove {}", marker.display()));
        }
    }
    files.write(&dir.join(PLANNER_PROMPT_FILE), prompt.as_bytes())?;
    prepare_planner_turns(files, dir, launch.turn_limits)?;
    let runner = dir.join(PLANNER_RUNNER_FILE);
    files
        .copy(launch.runner, &runner)
        .context("snapshot runtime binary")?;
    let mut argv = vec![
        path_text(&runner)?,
        "--db".into(),
        path_text(launch.db)?,
        "planner-session".into(),
        "--planner".into(),
        planner.id.to_string(),
        "--claude".into(),
        path_text(launch.claude)?,
    ];
    if let Some(plugin_dir) = launch.plugin_dir {
        argv.push("--plugin-dir".into());
        argv.push(path_text(plugin_dir)?);
    }
    // The wrapper gives its agent the model and effort; the session's hook
    // records what it was started with from the wrapper's environment.
    if let Some((model, effort)) = actor.arguments() {
        argv.extend([
            "--model".into(),
            model.to_owned(),
            "--effort".into(),
            effort.to_owned(),
        ]);
    }
    argv.push(HEADLESS_FLAG.into());
    argv.push(BACKGROUND_FLAG.into());
    let log = dir.join(PLANNER_SESSION_LOG);
    HostActorExecutor::new(launch.db)
        .with_workspaces(launch.backend)
        .spawn(ActorExecutionSpec::new(
            ActorContext::instance(crate::domain::ActorRole::Planner, planner.id),
            WorkspaceAccess::Write(launch.repo_root.to_path_buf()),
            ActorProgram::PlannerSession {
                cwd: launch.repo_root,
                wrapper: shell_join(&argv),
                planner: (planner.origin, planner.id),
                launch: Some(actor),
                log: &log,
            },
        ))?
        .workspace()
}

/// Before a headless planner's wrapper starts in its directory `dir`: its
/// turn limits, and neither an exit request nor a request a planner of the
/// same ID in a queue made again left there (ADR-t1394-2 decision 2).
fn prepare_planner_turns(files: &dyn RunFiles, dir: &Path, limits: TurnLimits) -> Result<()> {
    let turns = turns_dir(dir);
    files.create_dir_all(&turns)?;
    files.write(
        &turns.join(LIMITS_FILE),
        serde_json::to_string(&limits)?.as_bytes(),
    )?;
    let names: Vec<String> = files
        .read_dir(&turns)?
        .iter()
        .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
        .collect();
    for seq in turn::pending(names.iter().map(String::as_str)) {
        let path = request_path(dir, seq);
        files
            .rename(&path, &path.with_extension("dropped"))
            .with_context(|| format!("drop the untaken request {}", path.display()))?;
    }
    match files.remove_file(&exit_path(dir)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("remove an earlier exit request"),
    }
}

/// What the planner's session wrapper works with, as the run's does.
pub struct PlannerWrapper<'a> {
    pub queue: &'a mut dyn Queue,
    /// The queue's database, which the executor starts the agent on.
    pub db: &'a Path,
    pub provider: &'a dyn AgentProvider,
    pub spawner: &'a dyn Spawner,
    pub files: &'a dyn RunFiles,
    /// Lists and signals processes: a headless planner's turn is stopped
    /// with its descendants, as a headless worker's.
    pub processes: &'a dyn ProcessControl,
    pub pid: u32,
    /// The workspace the wrapper runs in, closed when the planner refuses
    /// the wrapper and records no such workspace (task 806); `None`
    /// outside cmux.
    pub own_workspace: Option<OwnWorkspace<'a>>,
    /// The clock a headless planner's turns read their times on.
    pub clock: &'a dyn Clock,
}

/// The session wrapper of planner `id` (`planner-session`): register this
/// process, start the agent with the planner's prompt in `cwd` (with
/// `model`, the model and effort its opener chose, when there are),
/// register it, heartbeat until it exits, and record its exit code, which
/// it returns. A headless planner's wrapper runs its turns instead
/// ([`Turns`], ADR-t1394-2 decision 2) until the exit request or a turn
/// that ends the session, and records the exit the same way.
pub fn run_planner_session(
    ctx: PlannerWrapper<'_>,
    id: PlannerId,
    dir: &Path,
    cwd: &Path,
    plugin_dir: Option<&Path>,
    model: Option<(&str, &str)>,
) -> Result<Value> {
    let PlannerWrapper {
        queue,
        db: queue_path,
        provider,
        spawner,
        files,
        processes,
        pid,
        own_workspace,
        clock,
    } = ctx;
    // A wrapper started for a planner already given up (its create
    // reported failing although cmux made the workspace, task 806) closes
    // the workspace nothing records.
    if let Err(error) = queue.register_planner_wrapper(id, pid) {
        let recorded = |workspace: &str| -> Result<bool> {
            Ok(queue.planner(id)?.workspace_id.as_deref() == Some(workspace))
        };
        return Err(wrapper_refused(
            own_workspace.as_ref(),
            recorded,
            &format!("planner {id}"),
            error,
        ));
    }
    let planner = match queue.planner(id) {
        Ok(planner) => planner,
        Err(error) => {
            let _ = queue.planner_exited(id, pid, 127);
            drop_own_runner(files, id, dir);
            return Err(error);
        }
    };
    if planner.route == PlannerRoute::Headless {
        // Its turns submit as the owner its record names: the opener
        // records the background handle after it started this wrapper, so
        // the first turn waits for it.
        let recorded = super::session::WrapperStart::Workspace
            .wait(|| Ok(queue.planner(id)?.workspace_id.is_some()));
        if let Err(error) = recorded {
            let _ = queue.planner_exited(id, pid, 127);
            drop_own_runner(files, id, dir);
            return Err(error);
        }
        let session = turn::planner_session_name(&dir.display().to_string(), planner.created_at);
        // Started in the background, its record is its handle (a row an
        // older binary started in a workspace is not).
        let background = queue
            .planner(id)
            .ok()
            .and_then(|planner| planner.workspace_id)
            .is_some_and(|handle| crate::domain::background_wrapper::is_background(&handle));
        let mut child_may_be_alive = false;
        let driven = Turns {
            queue: &mut *queue,
            db: queue_path,
            owner: TurnOwner::Planner {
                id,
                dir,
                cwd,
                plugin_dir,
                model,
                session,
            },
            provider,
            other: None,
            spawner,
            queue_service: None,
            processes,
            files,
            pid,
            resume: false,
            sccache: None,
            background,
            clock,
        }
        .drive(&mut child_may_be_alive);
        let code = match driven {
            Ok(code) => code,
            Err(error) => {
                if !child_may_be_alive {
                    let _ = queue.planner_exited(id, pid, 127);
                    drop_own_runner(files, id, dir);
                }
                return Err(error);
            }
        };
        queue.planner_exited(id, pid, code)?;
        drop_own_runner(files, id, dir);
        return Ok(json!({"planner_id": id, "exit_code": code, "route": PlannerRoute::Headless}));
    }
    // A planner in a terminal: only a person's runs here (ADR-t1394-1); the
    // runtime's run headless (ADR-t1433-2 decision 3). An agent that never
    // starts is an exit too, or the planner would look lost rather than
    // over.
    let started = queue
        .planner(id)
        .and_then(|_| Ok(files.read_to_string(&dir.join(PLANNER_PROMPT_FILE))?))
        .and_then(|prompt: String| {
            HostActorExecutor::new(queue_path)
                .with_provider(provider)
                .with_spawner(spawner)
                .spawn(ActorExecutionSpec::new(
                    ActorContext::instance(crate::domain::ActorRole::Planner, id),
                    WorkspaceAccess::Write(cwd.to_path_buf()),
                    ActorProgram::SessionAgent {
                        agent: SessionAgent::Planner(PlannerCommand {
                            dir,
                            cwd,
                            prompt: &prompt,
                            plugin_dir,
                        }),
                        model,
                    },
                ))?
                .process()
        });
    let mut child = match started {
        Ok(child) => child,
        Err(error) => {
            let _ = queue.planner_exited(id, pid, 127);
            drop_own_runner(files, id, dir);
            return Err(error);
        }
    };
    if let Err(error) = queue.register_planner_agent(id, pid, child.id()) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = queue.planner_exited(id, pid, 127);
        drop_own_runner(files, id, dir);
        return Err(error);
    }
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code.unwrap_or(128),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = queue.planner_exited(id, pid, 127);
                drop_own_runner(files, id, dir);
                return Err(error);
            }
        }
        if let Err(error) = queue.heartbeat_planner(id, pid) {
            // Keep waiting on the agent through a queue outage.
            warn!(planner_id = %id, error = %format_args!("{error:#}"), "planner heartbeat failed: {error:#}");
        }
        thread::sleep(provider.wait_interval());
    };
    queue.planner_exited(id, pid, code)?;
    drop_own_runner(files, id, dir);
    Ok(json!({"planner_id": id, "exit_code": code}))
}

/// Remove the runner the wrapper of planner `id` runs from, once its exit
/// is recorded: nothing runs it again (a planner is never reopened), and
/// the process keeps its image. A failure is logged; the supervisor's
/// sweep ([`remove_unused_planner_runners`]) retries it.
fn drop_own_runner(files: &dyn RunFiles, id: PlannerId, dir: &Path) {
    if let Err(error) = remove_runner(files, &dir.join(PLANNER_RUNNER_FILE)) {
        warn!(planner_id = %id, error = %format_args!("{error:#}"), "planner {id}: its runner could not be removed: {error:#}");
    }
}

/// Remove the runner at `path`; whether there was one.
pub(crate) fn remove_runner(files: &dyn RunFiles, path: &Path) -> Result<bool> {
    match files.remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

/// A planner with how it stands now: its state, whether it is alive, and
/// since when its agent is idle (Unix seconds of its idle marker, while
/// the state is `idle`).
#[derive(Debug, Clone, Serialize)]
pub struct PlannerView {
    #[serde(flatten)]
    pub planner: PlannerSession,
    pub state: PlannerState,
    pub alive: bool,
    pub idle_since: Option<i64>,
    pub dir: PathBuf,
    /// The wrapper of a planner started in the background (ADR-t1404-1
    /// decision 8): its pid and the log `planner log` reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<BackgroundSession>,
    /// The bundle of drafts a planner of the runtime's was opened for
    /// (ADR-t807-1), with what became of each draft.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<crate::domain::DraftBundleView>,
}

/// What judging a planner reads: the backend for its background wrapper's
/// handle (no cmux workspace is looked up or read, ADR-t1433-2 decisions 3
/// and 5), the processes for its wrapper, the files for its idle marker,
/// the agent's signals for the marker, and the clock.
pub struct PlannerProbes<'a> {
    pub cmux: &'a dyn WorkspaceBackend,
    pub processes: &'a dyn ProcessControl,
    pub files: &'a dyn RunFiles,
    pub signals: &'a dyn AgentSignals,
    pub clock: &'a dyn Clock,
    pub planners_dir: &'a Path,
}

/// Judge `planner` the way a worker session is judged: its session (a
/// background handle's process; a row in a cmux workspace, see
/// [`retired_workspace`], reads `closed`), its wrapper's pid and
/// heartbeat, and the idle marker its agent's turn wrote, counted only
/// when no older than the planner's last input (its input marker, the
/// supervisor's stamp of a request, or its opening). No screen is read. A
/// closed planner is not looked at.
pub fn planner_view(probes: &PlannerProbes<'_>, planner: PlannerSession) -> Result<PlannerView> {
    let dir = planner_dir(probes.planners_dir, planner.id);
    let now = probes.clock.now();
    let mut probe = PlannerProbe {
        now,
        workspace_listed: false,
        wrapper_alive: false,
        idle: None,
        working: None,
        screen_idle: None,
    };
    if planner.closed_at.is_none() {
        if let Some(workspace) = &planner.workspace_id {
            // A planner of the runtime's has no workspace: its handle is
            // its background wrapper's, judged by its process. A row in a
            // cmux workspace (an older binary's of the runtime's, or a
            // person's) is not looked up in cmux (ADR-t1433-2 decisions 3
            // and 5): it reads `closed`.
            probe.workspace_listed = if retired_workspace(&planner) {
                false
            } else {
                probes.cmux.exists(workspace)?
            };
        }
        probe.wrapper_alive = planner
            .wrapper_pid
            .is_some_and(|pid| probes.processes.alive(pid));
        let waiting = planner.route == PlannerRoute::Headless
            && match probes.files.read_dir(&turns_dir(&dir)) {
                Ok(paths) => {
                    !turn::pending(paths.iter().filter_map(|path| path.file_name()?.to_str()))
                        .is_empty()
                }
                Err(_) => false,
            };
        let marker_path = planner_idle_marker(&dir);
        let opened = UNIX_EPOCH + Duration::from_secs(u64::try_from(planner.created_at)?);
        let last_input = screen_idle::last_input(probes.files, &marker_path, opened);
        let marker = match probes.files.read_stamped(&marker_path)? {
            None => Err(MarkerState::Missing),
            Some((modified, _)) if modified < last_input => Err(MarkerState::Stale),
            Some((modified, bytes)) => Ok((
                IdleProbe {
                    since: super::unix_seconds(modified),
                    background_running: probes.signals.idle_hook(&bytes).background_running,
                },
                TurnMark::parse(&bytes),
            )),
        };
        // A turn that met Claude's wall leaves the requests written behind
        // it waiting until its `provider retry` (the wrapper takes none
        // before it, task 1596): the planner is idle at the wall all the
        // same, for `tend_planner_walls` ([`turn::idle_after_turn`]).
        let marker =
            marker.map(|(idle, mark)| (idle, turn::idle_after_turn(mark.as_ref(), waiting)));
        if planner.route == PlannerRoute::Headless {
            // A headless planner has no screen (ADR-t1394-2 decision 3):
            // it is idle once the marker its wrapper wrote at the end of a
            // turn is newer than the last input stamped for it and no
            // request waits in its `turns/` (or that turn met Claude's
            // wall, above), and at work until then (a
            // request written while a turn ran, such as `planner request`'s,
            // waits for the next turn; the wrapper stamps the input as it
            // takes one, before it leaves `turns/`, which is why `turns/` is
            // read before the marker). Its wrapper ends what a turn left
            // running.
            probe.idle = marker
                .ok()
                .filter(|&(_, idle)| idle)
                .map(|(idle, _)| IdleProbe {
                    background_running: false,
                    ..idle
                });
        } else if let Ok((idle, _)) = marker {
            probe.idle = Some(idle);
        }
    }
    let state = planner.state(&probe);
    let idle = state == PlannerState::Idle;
    Ok(PlannerView {
        state,
        alive: state.alive(),
        idle_since: probe.idle.map(|idle| idle.since).filter(|_| idle),
        background: planner.workspace_id.as_deref().and_then(|handle| {
            BackgroundSession::of(handle, dir.join(PLANNER_SESSION_LOG).display().to_string())
        }),
        dir,
        planner,
        bundle: None,
    })
}

/// Whether `planner`'s row records a cmux workspace: a planner of the
/// runtime's an older binary opened in one (interactive, or headless under
/// `[headless] wrapper = "workspace"`, ADR-t1433-2 decision 3), or a
/// person's planner opened before `dagq plan` was abolished (decision 5).
/// The runtime calls no cmux for planners any more (ADR-t1433-1), so it
/// neither looks the workspace up nor types into it, and ends the row
/// ([`close_person_planners`] for a person's), the workspace left for a
/// person to close.
pub fn retired_workspace(planner: &PlannerSession) -> bool {
    planner
        .workspace_id
        .as_deref()
        .is_some_and(|workspace| !crate::domain::background_wrapper::is_background(workspace))
}

/// When anything was last seen of the planner whose directory is `dir`,
/// opened at `created_at` (Unix seconds): the latest of its last input
/// ([`screen_idle::last_input`]) and its idle marker, fresh or stale. The
/// runtime's planner timeout counts from it (task 805).
pub fn planner_last_activity(files: &dyn RunFiles, dir: &Path, created_at: i64) -> Result<i64> {
    let marker = planner_idle_marker(dir);
    let opened = UNIX_EPOCH + Duration::from_secs(u64::try_from(created_at)?);
    let last = screen_idle::last_input(files, &marker, opened);
    let last = files
        .modified(&marker)
        .map_or(last, |modified| last.max(modified));
    Ok(super::unix_seconds(last))
}

/// Every planner not closed (with `all`, every planner), each judged by
/// [`planner_view`]. Nothing is written: a planner that only looks
/// `closed` here is not given up in the queue on that evidence.
pub fn planner_views(
    queue: &dyn Queue,
    probes: &PlannerProbes<'_>,
    all: bool,
) -> Result<Vec<PlannerView>> {
    queue
        .planners(all)?
        .into_iter()
        .map(|planner| {
            let bundle = queue.draft_bundle(planner.id)?;
            Ok(PlannerView {
                bundle,
                ..planner_view(probes, planner)?
            })
        })
        .collect()
}

/// Close the rows of the planners of the runtime's whose session is over
/// for good ([`PlannerSession::abandoned`]): their wrapper is done and their
/// session is gone. A person's planner's row is left to
/// [`close_person_planners`], so it closes as `person_retired` alone. No
/// cmux workspace is listed (ADR-t1433-2 decisions 3 and 5): a row in one
/// ([`retired_workspace`]) counts as not listed. A headless planner's wrapper in the background
/// (ADR-t1404-1 decision 8) is judged by its process (`exists`, which also
/// holds while a turn its dead wrapper left runs), so a live one whose
/// heartbeat is late keeps its row, and one whose agent's exit is recorded
/// is closed as `runtime_exited`, as the supervisor's pass would. Returns
/// the IDs closed.
pub fn close_abandoned_planners(
    queue: &dyn Queue,
    cmux: &dyn WorkspaceBackend,
    processes: &dyn ProcessControl,
    clock: &dyn Clock,
) -> Result<Vec<PlannerId>> {
    use crate::domain::background_wrapper::is_background;
    let open: Vec<PlannerSession> = queue
        .planners(false)?
        .into_iter()
        .filter(|planner| planner.workspace_id.is_some() && !planner.person_retired())
        .collect();
    let now = clock.now();
    let mut closed = Vec::new();
    for planner in open {
        let workspace = planner.workspace_id.as_deref().unwrap_or_default();
        let background = is_background(workspace);
        // One that cannot be judged now is not given up.
        let workspace_listed = background && cmux.exists(workspace).unwrap_or(true);
        let probe = PlannerProbe {
            now,
            workspace_listed,
            wrapper_alive: planner.wrapper_pid.is_some_and(|pid| processes.alive(pid)),
            idle: None,
            working: None,
            screen_idle: None,
        };
        if planner.abandoned(&probe) {
            let (code, reason) = if background && planner.exited_at.is_some() {
                (
                    PlannerCloseCode::RuntimeExited,
                    format!(
                        "planner {} of the runtime: its agent exited and its background wrapper ended",
                        planner.id
                    ),
                )
            } else {
                (
                    PlannerCloseCode::Abandoned,
                    format!(
                        "planner {}: its workspace is not listed and its wrapper is done; its record is closed",
                        planner.id
                    ),
                )
            };
            let payload = planner_closed_payload(&planner, code, false, &reason);
            if queue.end_planner(planner.id, &payload)? {
                closed.push(planner.id);
            }
        }
    }
    Ok(closed)
}

/// The payload of the `planner_closed` the runtime records as it closes
/// `planner` (ADR-t1300-1): who opened it, its workspace, why it closed
/// (`code`, with `reason` in words), the agent's exit and whether the
/// workspace was closed then. The screen is not in it.
pub fn planner_closed_payload(
    planner: &PlannerSession,
    code: PlannerCloseCode,
    workspace_closed: bool,
    reason: &str,
) -> Value {
    json!({
        "planner_id": planner.id,
        "origin": planner.origin.as_str(),
        "workspace_id": planner.workspace_id,
        "proposal_id": planner.proposal_id,
        "draft_task_id": planner.draft_task_id,
        "finding_id": planner.finding_id,
        "request_id": planner.request_id,
        "code": code.as_str(),
        "reason": reason,
        "exit_code": planner.exit_code,
        "exited_at": planner.exited_at,
        "workspace_closed": workspace_closed,
    })
}

/// Close the row of every person's planner still open
/// ([`PlannerSession::person_retired`], ADR-t1433-2 decision 5), alive or
/// not, with `planner_closed` (`person_retired`, the workspace not closed,
/// ADR-t1300-1 decision 2). No cmux is called: its workspace is neither
/// looked at, typed into nor closed, and a person closes it in their own
/// terminal. A revise or an answer for what it owned then goes the way of
/// an owner that closed (a new planner of the runtime's, ADR-0047 decision
/// 12, ADR-t1394-1 decision 7). Returns the IDs closed.
pub fn close_person_planners(queue: &dyn Queue) -> Result<Vec<PlannerId>> {
    let mut closed = Vec::new();
    for planner in queue.planners(false)? {
        if !planner.person_retired() {
            continue;
        }
        let reason = format!(
            "planner {} of a person, opened before dagq plan was abolished: its record is closed without cmux; a person closes its workspace{} in their own terminal (ADR-t1433-2)",
            planner.id,
            planner
                .workspace_id
                .as_deref()
                .map_or_else(String::new, |workspace| format!(" {workspace}")),
        );
        let payload =
            planner_closed_payload(&planner, PlannerCloseCode::PersonRetired, false, &reason);
        if queue.end_planner(planner.id, &payload)? {
            closed.push(planner.id);
        }
    }
    Ok(closed)
}

/// Remove the runner of every planner, closed or not, that nothing runs
/// any more ([`PlannerSession::runner_unused`]: its exit recorded, its
/// wrapper dead or silent, or never registered in time), for the planners
/// left before their wrapper removed its own, or whose wrapper did not end
/// cleanly. A live wrapper's runner is kept. Every removal is tried; the
/// first failure is returned after them. Returns the IDs whose runner went.
pub fn remove_unused_planner_runners(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    files: &dyn RunFiles,
    clock: &dyn Clock,
    planners_dir: &Path,
) -> Result<Vec<PlannerId>> {
    let now = clock.now();
    let mut removed = Vec::new();
    let mut failure = None;
    for planner in queue.planners(true)? {
        let runner = planner_dir(planners_dir, planner.id).join(PLANNER_RUNNER_FILE);
        if !files.exists(&runner) {
            continue;
        }
        let probe = PlannerProbe {
            now,
            workspace_listed: false,
            wrapper_alive: planner.wrapper_pid.is_some_and(|pid| processes.alive(pid)),
            idle: None,
            working: None,
            screen_idle: None,
        };
        if !planner.runner_unused(&probe) {
            continue;
        }
        match remove_runner(files, &runner) {
            Ok(true) => removed.push(planner.id),
            Ok(false) => {}
            Err(error) => {
                failure.get_or_insert(error);
            }
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(removed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{FindingId, PlannerRoute, RequestId};

    /// A row that records a cmux workspace is retired, whoever opened it:
    /// a planner of the runtime's an older binary opened in one
    /// (interactive, or headless under `[headless] wrapper = "workspace"`),
    /// or a person's planner (ADR-t1433-2 decision 5). No cmux is called
    /// for it. One with a background handle or with no session yet is not.
    #[test]
    fn only_a_row_in_a_cmux_workspace_is_retired() {
        let row = |origin, route, workspace: Option<&str>| PlannerSession {
            id: PlannerId::new(1),
            origin,
            proposal_id: None,
            draft_task_id: None,
            finding_id: None,
            request_id: None,
            workspace_id: workspace.map(str::to_owned),
            wrapper_pid: Some(1),
            agent_pid: Some(2),
            heartbeat_at: Some(0),
            exit_code: None,
            exited_at: None,
            closed_at: None,
            error: None,
            created_at: 0,
            route,
        };
        let cmux = Some("01234567-89AB-4DEF-8123-000000000000");
        let handle = Some("background:7:start");
        for route in [PlannerRoute::Interactive, PlannerRoute::Headless] {
            for origin in [PlannerOrigin::Runtime, PlannerOrigin::Person] {
                assert!(
                    retired_workspace(&row(origin, route, cmux)),
                    "{origin:?} {route:?}"
                );
                for workspace in [handle, None] {
                    assert!(
                        !retired_workspace(&row(origin, route, workspace)),
                        "{origin:?} {route:?} {workspace:?}"
                    );
                }
            }
        }
    }

    // Moved here by task 1711 from the tests/it cases it removed:
    // planner_headless_turns::a_headless_finding_planner_takes_the_answer_as_its_next_turn_and_closes_with_its_finding
    // and a_headless_request_planners_answer_reaches_a_new_one_and_undecided_ends_exhaust_the_request.
    #[test]
    fn planner_closed_names_what_the_planner_was_opened_for_and_why_it_closed() {
        let planner = PlannerSession {
            id: PlannerId::new(3),
            origin: PlannerOrigin::Runtime,
            proposal_id: None,
            draft_task_id: None,
            finding_id: Some(FindingId::new(1)),
            request_id: Some(RequestId::new(2)),
            workspace_id: Some("background:1:x".to_owned()),
            wrapper_pid: Some(1),
            agent_pid: Some(2),
            heartbeat_at: Some(0),
            exit_code: Some(0),
            exited_at: Some(40),
            closed_at: None,
            error: None,
            created_at: 0,
            route: PlannerRoute::Headless,
        };
        assert_eq!(
            planner_closed_payload(&planner, PlannerCloseCode::RuntimeExited, false, "ended"),
            json!({
                "planner_id": 3,
                "origin": "runtime",
                "workspace_id": "background:1:x",
                "proposal_id": null,
                "draft_task_id": null,
                "finding_id": 1,
                "request_id": 2,
                "code": "runtime_exited",
                "reason": "ended",
                "exit_code": 0,
                "exited_at": 40,
                "workspace_closed": false,
            })
        );
    }
}
