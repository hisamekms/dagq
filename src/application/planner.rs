//! Planner sessions (ADR-0041 decisions 1, 6, 12, 13): on-demand cmux
//! workspaces where a planner writes goals and tasks and submits them as a
//! proposal. Only the runtime opens one (ADR-t1394-1): for a proposal plan
//! review sent back while its own planner was closed
//! ([`open_runtime_planner`]), and for a draft, a finding or a planning
//! request the inbox recorded ([`open_draft_planner`]). `dagq plan`, which
//! opened a planner a person talked with, is refused with
//! [`PLAN_REFUSED`]; a person's planner opened before stays a `planners`
//! row until it ends ([`close_exited_person_planners`]). Each is a
//! `planners` row with its own directory under the queue's `planners/`
//! (its prompt, the wrapper binary, the agent's settings, log and idle
//! marker).
//!
//! The workspace runs the session wrapper `planner-session`, which starts
//! the agent, registers itself and its agent, heartbeats and records the
//! agent's exit, the way a run's wrapper does; the agent's `Stop` hook
//! writes the idle marker. [`planner_view`] judges from these whether the
//! planner is alive and idle.
//!
//! A planner of the runtime's opened under `[roles.runtime_planner] route
//! = "headless"` (ADR-t1394-2) runs its agent one call per turn instead,
//! the way a headless worker does ([`super::headless_session`]): its
//! directory holds the `turns/` its supervisor writes requests to, its
//! wrapper writes the idle marker when a turn ends, and the wrapper runs in
//! a workspace or, under `[headless] wrapper = "background"`, as a process
//! detached from the supervisor (ADR-t1404-1 decision 8).

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
        WorkspaceAccess, WorkspaceCommand,
    },
    headless_session::{TurnOwner, Turns},
    lifecycle::{QueueWorkspaces, ROLE_STATUS_KEY, session_look},
    naming::{planner_workspace_name, shell_join},
    path_text, planner_idle_marker,
    prompt::{FittedPrompt, runtime_planner_prompt},
    screen_idle::{self, Inference, MarkerState, ScreenIdle, ScreenProbe},
    session::{OwnWorkspace, wrapper_refused},
};
use crate::domain::{
    ActorContext, IdleProbe, PERSON_PLANNER_CLOSE_GRACE_SECS, PlannerCloseCode, PlannerId,
    PlannerOrigin, PlannerProbe, PlannerRoute, PlannerSession, PlannerState, ProposalId,
    SessionRole, Task,
    actor_model::{ActorLaunch, ModelRole, REVISE_ESCALATION, RoleModels},
    background_wrapper::{BACKGROUND_FLAG, BackgroundSession, HeadlessWrapper},
    language::{Language, with_instruction},
    turn::{self, LIMITS_FILE, TurnLimits, TurnMark, exit_path, request_path, turns_dir},
};

/// The planner's first message, which its wrapper hands the agent.
pub const PLANNER_PROMPT_FILE: &str = "prompt.txt";
/// The agent's debug log in the planner's directory, where a failed idle
/// hook shows (ADR-t803-1).
pub const PLANNER_DEBUG_LOG: &str = "claude.log";
/// The snapshot of the binary the planner's workspace runs as its wrapper,
/// so rebuilding the binary does not change a running one.
pub const PLANNER_RUNNER_FILE: &str = "runner";
/// The log of a headless planner's wrapper started in the background
/// (ADR-t1404-1 decision 6), in the planner's directory.
pub const PLANNER_SESSION_LOG: &str = "session.log";
/// The flag of `planner-session` that runs the planner's agent one call
/// per turn (ADR-t1394-2 decision 2).
pub const HEADLESS_FLAG: &str = "--headless";

/// The directory of planner `id` under the queue's `planners/` directory.
pub fn planner_dir(planners_dir: &Path, id: PlannerId) -> PathBuf {
    planners_dir.join(id.to_string())
}

/// What opening a planner works with: the queue and cmux, the files, the
/// queue's database, hash and `planners/` directory, the checkout the
/// planner works in, and what its workspace runs (this binary as the
/// wrapper, the agent's executable, the plugin directory it loads).
pub struct PlannerLaunch<'a> {
    pub queue: &'a dyn Queue,
    pub cmux: &'a dyn WorkspaceBackend,
    pub files: &'a dyn RunFiles,
    pub db: &'a Path,
    pub queue_hash: &'a str,
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
    /// with, and the route a planner of the runtime's opens on
    /// (ADR-t1394-2 decision 1).
    pub roles: RoleModels,
    /// `[headless] wrapper` of `dagq.toml`: where a headless planner's
    /// wrapper runs (ADR-t1404-1 decision 8).
    pub headless_wrapper: HeadlessWrapper,
    /// The limits of a headless planner's turns, from the supervisor's
    /// `[stall]` settings, as a headless worker's.
    pub turn_limits: TurnLimits,
}

/// A planner whose workspace just opened: its record, the workspace's
/// title, its directory, and what cmux refused about its look.
#[derive(Debug, Clone, Serialize)]
pub struct OpenedPlanner {
    pub planner: PlannerSession,
    pub name: String,
    pub dir: PathBuf,
    pub warnings: Vec<String>,
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

/// The route a planner of `origin` opens on: a person's is interactive, one
/// of the runtime's takes `[roles.runtime_planner] route` (ADR-t1394-2
/// decision 1).
fn route_of(launch: &PlannerLaunch<'_>, origin: PlannerOrigin) -> PlannerRoute {
    match origin {
        PlannerOrigin::Person => PlannerRoute::Interactive,
        PlannerOrigin::Runtime => launch.roles.planner_route().0,
    }
}

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
    launch_planner(
        launch,
        planner,
        (&prompt.text, Some(("runtime", &prompt))),
        &actor,
    )
}

/// Record a planner, write its directory and open its workspace running
/// the wrapper, with `DAGQ_ROLE=planner`, `DAGQ_QUEUE`, the origin and the
/// planner's ID in the workspace's environment and the queue's group
/// (ADR-0026). A workspace cmux does not open closes the record with the
/// error. The look (Blue, the `map` pill) is a warning when refused; a
/// planner is not pinned, being on demand. Its agent starts as `actor`
/// says.
pub fn open_planner(
    launch: &PlannerLaunch<'_>,
    origin: PlannerOrigin,
    proposal: Option<ProposalId>,
    prompt: &str,
    actor: &ActorLaunch,
) -> Result<OpenedPlanner> {
    let planner = launch.queue.open_planner(origin, proposal)?;
    launch_planner(launch, planner, (prompt, None), actor)
}

/// Open the workspace of a planner the runtime recorded for a draft
/// (ADR-0041 decision 16, [`super::DraftPlannerStore::open_draft_planner`]),
/// a finding or a planning request (`kind`: `draft`, `finding`, `request`)
/// with `prompt`, the way [`open_planner`] does, its agent starting with
/// `[roles.runtime_planner]`.
pub fn open_draft_planner(
    launch: &PlannerLaunch<'_>,
    planner: PlannerSession,
    (kind, prompt): (&'static str, &FittedPrompt),
) -> Result<OpenedPlanner> {
    let actor = launch.roles.launch(ModelRole::RuntimePlanner);
    launch_planner(
        launch,
        planner,
        (&prompt.text, Some((kind, prompt))),
        &actor,
    )
}

/// Open the workspace of `planner` with `prompt`. A prompt the runtime
/// held to its limits (`fitted`, with its kind) has its bytes recorded as
/// `planner_prompt_written` (task 1571, ADR-t1566-1 decision 6).
fn launch_planner(
    launch: &PlannerLaunch<'_>,
    planner: PlannerSession,
    (prompt, fitted): (&str, Option<(&'static str, &FittedPrompt)>),
    actor: &ActorLaunch,
) -> Result<OpenedPlanner> {
    let queue = launch.queue;
    // The route is read as the planner opens and kept with it: a planner
    // that runs keeps its route (ADR-t1394-2 decision 1).
    let planner = match route_of(launch, planner.origin) {
        PlannerRoute::Interactive => planner,
        route => {
            if let Err(error) = queue.set_planner_route(planner.id, route) {
                queue.close_planner(planner.id, Some(&format!("{error:#}")))?;
                return Err(error);
            }
            queue.planner(planner.id)?
        }
    };
    let proposal = planner.proposal_id;
    let dir = planner_dir(launch.planners_dir, planner.id);
    let workspaces = QueueWorkspaces::new(
        launch.cmux,
        launch.db,
        launch.queue_hash.to_owned(),
        launch.repo_root,
    );
    let mut name = planner_workspace_name(launch.repo_root, planner.id, proposal);
    if let Some(draft) = planner.draft_task_id {
        name.push_str(&format!(" - draft task {draft}"));
    }
    if let Some(finding) = planner.finding_id {
        name.push_str(&format!(" - finding {finding}"));
    }
    if let Some(request) = planner.request_id {
        name.push_str(&format!(" - request {request}"));
    }
    let prompt = match fitted {
        Some((kind, fitted)) => {
            let fitted = fitted.clone().with_language(launch.language.as_ref());
            record_prompt_bytes(queue, &planner, kind, &fitted);
            fitted.text
        }
        None => with_instruction(prompt.to_owned(), launch.language.as_ref()),
    };
    let opened = create_workspace(launch, &workspaces, &planner, &dir, &name, &prompt, actor);
    let workspace_id = match opened {
        Ok(id) => id,
        Err(error) => {
            queue.close_planner(planner.id, Some(&format!("{error:#}")))?;
            return Err(error);
        }
    };
    if let Err(error) = queue.planner_workspace_created(planner.id, &workspace_id) {
        // Unrecorded, the workspace would be left open with nothing to
        // find it by, and its wrapper is refused (task 806).
        let error = match super::supervise::stop_session(
            launch.cmux,
            &workspace_id,
            crate::domain::background_wrapper::StopRoute::Planner,
        ) {
            Ok(()) => error.context(format!(
                "the planner workspace {workspace_id} could not be recorded and was closed"
            )),
            Err(close) => error.context(format!(
                "the planner workspace {workspace_id} could not be recorded, and closing it failed: {close:#}"
            )),
        };
        queue.close_planner(planner.id, Some(&format!("{error:#}")))?;
        return Err(error);
    }
    let mut warnings = workspaces.take_warnings();
    // A wrapper in the background has no workspace to look at.
    let look = session_look(SessionRole::Planner)
        .filter(|_| !crate::domain::background_wrapper::is_background(&workspace_id));
    if let Some((color, icon)) = look {
        for (what, result) in [
            ("color", launch.cmux.set_color(&workspace_id, color)),
            (
                "status pill",
                launch.cmux.set_status(
                    &workspace_id,
                    ROLE_STATUS_KEY,
                    SessionRole::Planner.as_str(),
                    icon,
                ),
            ),
        ] {
            if let Err(error) = result {
                warnings.push(format!(
                    "cmux could not set the {what} of the planner workspace {workspace_id}: {error:#}"
                ));
            }
        }
    }
    Ok(OpenedPlanner {
        planner: queue.planner(planner.id)?,
        name,
        dir,
        warnings,
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

fn create_workspace(
    launch: &PlannerLaunch<'_>,
    workspaces: &QueueWorkspaces<'_>,
    planner: &PlannerSession,
    dir: &Path,
    name: &str,
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
    let headless = planner.route == PlannerRoute::Headless;
    if headless {
        prepare_planner_turns(files, dir, launch.turn_limits)?;
    }
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
    // records what it was started with from the workspace's environment.
    if let Some((model, effort)) = actor.arguments() {
        argv.extend([
            "--model".into(),
            model.to_owned(),
            "--effort".into(),
            effort.to_owned(),
        ]);
    }
    // A headless planner's wrapper runs its turns, in a workspace or in
    // the background (ADR-t1404-1 decision 8).
    let background = (headless && launch.headless_wrapper == HeadlessWrapper::Background)
        .then(|| dir.join(PLANNER_SESSION_LOG));
    if headless {
        argv.push(HEADLESS_FLAG.into());
    }
    if background.is_some() {
        argv.push(BACKGROUND_FLAG.into());
    }
    let mut description = workspaces.description(SessionRole::Planner);
    if let Some(description) = &mut description {
        description.push_str(&format!(" planner={}", planner.id));
    }
    HostActorExecutor::new(launch.db)
        .with_workspaces(launch.cmux)
        .spawn(ActorExecutionSpec::new(
            ActorContext::instance(crate::domain::ActorRole::Planner, planner.id),
            WorkspaceAccess::Write(launch.repo_root.to_path_buf()),
            ActorProgram::NamedWorkspace {
                name,
                cwd: launch.repo_root,
                command: WorkspaceCommand::Wrapper(shell_join(&argv)),
                planner: Some((planner.origin, planner.id)),
                launch: Some(actor),
                description,
                group: workspaces.group(),
                background: background.as_deref(),
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
    /// The workspace's terminal (stderr), where the wrapper of a person's
    /// planner says when its workspace closes (ADR-t1300-1).
    pub terminal: &'a mut dyn std::io::Write,
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
        terminal,
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
        // records the workspace (or the background handle) after it
        // started this wrapper, so the first turn waits for it.
        let recorded = super::session::WrapperStart::Workspace
            .wait(|| Ok(queue.planner(id)?.workspace_id.is_some()));
        if let Err(error) = recorded {
            let _ = queue.planner_exited(id, pid, 127);
            drop_own_runner(files, id, dir);
            return Err(error);
        }
        let session = turn::planner_session_name(&dir.display().to_string(), planner.created_at);
        // Started in the background, its record is its handle.
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
    // An agent that never starts is an exit too, or the planner would look
    // lost rather than over.
    let started = queue
        .planner(id)
        .and_then(|planner| {
            let prompt = files.read_to_string(&dir.join(PLANNER_PROMPT_FILE))?;
            Ok((planner.origin, prompt))
        })
        .and_then(|(origin, prompt)| {
            HostActorExecutor::new(queue_path)
                .with_provider(provider)
                .with_spawner(spawner)
                .spawn(ActorExecutionSpec::new(
                    ActorContext::instance(crate::domain::ActorRole::Planner, id),
                    WorkspaceAccess::Write(cwd.to_path_buf()),
                    ActorProgram::SessionAgent {
                        agent: SessionAgent::Planner(PlannerCommand {
                            origin,
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
    // A person's planner is closed by the supervisor after the grace
    // (ADR-t1300-1); the terminal says so.
    if queue
        .planner(id)
        .is_ok_and(|planner| planner.origin == PlannerOrigin::Person)
    {
        let notice = exited_notice(id, code, dir);
        if let Err(error) = writeln!(terminal, "{notice}").and_then(|()| terminal.flush()) {
            warn!(planner_id = %id, error = %error, "planner {id}: the closing notice could not be written: {error}");
        }
        return Ok(json!({"planner_id": id, "exit_code": code, "notice": notice}));
    }
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
/// since when its agent is idle (Unix seconds of its idle marker, or of
/// the first capture its screen was inferred idle from, while the state is
/// `idle`).
#[derive(Debug, Clone, Serialize)]
pub struct PlannerView {
    #[serde(flatten)]
    pub planner: PlannerSession,
    pub state: PlannerState,
    pub alive: bool,
    pub idle_since: Option<i64>,
    /// The idle inferred from the screen (ADR-t803-1), when the idle
    /// marker could not tell: while the state is `idle`, or while the
    /// screen shows background work, which keeps it `working`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_inferred: Option<Inference>,
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

/// What judging a planner reads: cmux for its workspace and screen, the
/// processes for its wrapper, the files for its idle marker, the agent's
/// signals for the marker and the screen, and the clock; and for a
/// screen standing in for the marker (ADR-t803-1), how long it must look
/// idle (`[stall].screen_idle_secs`) and whether its captures are kept.
pub struct PlannerProbes<'a> {
    pub cmux: &'a dyn WorkspaceBackend,
    pub processes: &'a dyn ProcessControl,
    pub files: &'a dyn RunFiles,
    pub signals: &'a dyn AgentSignals,
    pub clock: &'a dyn Clock,
    pub planners_dir: &'a Path,
    pub screen_idle_threshold: Duration,
    pub screen_idle: ScreenIdle<'a>,
}

/// Judge `planner` the way a worker session is judged: its workspace UUID
/// in every window's `cmux workspace list`, its wrapper's pid and heartbeat, the idle
/// marker its agent's `Stop` hook wrote and, with a marker, whether the
/// screen shows the agent at work on a new turn. Without a marker, or with
/// one older than the planner's last input (its input marker, the
/// supervisor's stamp of a text it typed, or its opening), the screen
/// stands in ([`ScreenProbe::infer`]). A closed planner is not looked at.
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
    let mut inferred = None;
    if planner.closed_at.is_none() {
        if let Some(workspace) = &planner.workspace_id {
            probe.workspace_listed = probes.cmux.exists(workspace)?;
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
        } else if probe.workspace_listed
            && let Some(workspace) = &planner.workspace_id
        {
            match marker {
                Ok((idle, _)) => {
                    probe.idle = Some(idle);
                    probe.working = probes
                        .cmux
                        .capture(workspace)
                        .ok()
                        .map(|screen| probes.signals.working(&screen));
                }
                Err(state) => {
                    inferred = ScreenProbe {
                        cmux: probes.cmux,
                        signals: probes.signals,
                        files: probes.files,
                        mode: probes.screen_idle,
                        threshold: probes.screen_idle_threshold,
                    }
                    .infer(
                        workspace,
                        &marker_path,
                        state,
                        super::unix_millis(probes.clock.system_time()),
                        super::unix_millis(last_input),
                    );
                    probe.screen_idle = inferred.map(|inference| IdleProbe {
                        since: inference.since,
                        background_running: inference.background_running == Some(true),
                    });
                }
            }
        } else if let Ok((idle, _)) = marker {
            probe.idle = Some(idle);
        }
    }
    let state = planner.state(&probe);
    let idle = state == PlannerState::Idle;
    Ok(PlannerView {
        state,
        alive: state.alive(),
        idle_since: probe
            .idle
            .map(|idle| idle.since)
            .or(probe.screen_idle.map(|idle| idle.since))
            .filter(|_| idle),
        // Also while the screen shows background work: it is why the
        // planner is `working`, and its span is recorded.
        idle_inferred: inferred
            .filter(|inference| idle || inference.background_running == Some(true)),
        background: planner.workspace_id.as_deref().and_then(|handle| {
            BackgroundSession::of(handle, dir.join(PLANNER_SESSION_LOG).display().to_string())
        }),
        dir,
        planner,
        bundle: None,
    })
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

/// Close the rows of the planners whose session is over for good
/// ([`PlannerSession::abandoned`]), a person's included: their workspace is
/// not in cmux's one listing of all windows and their wrapper is done. The
/// rows are read before the listing, so a workspace opened after it is not
/// taken for gone. A listing cmux fails to give closes nothing (the error
/// is returned): a passing failure gives no planner up. A headless
/// planner's wrapper in the background (ADR-t1404-1 decision 8) is never
/// listed: its handle is judged by its process (`exists`, which also holds
/// while a turn its dead wrapper left runs), so a live one whose heartbeat
/// is late keeps its row, and one whose agent's exit is recorded is closed
/// as `runtime_exited`, as the supervisor's pass would. Returns the IDs
/// closed.
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
        .filter(|planner| planner.workspace_id.is_some())
        .collect();
    if open.is_empty() {
        return Ok(Vec::new());
    }
    let needs_listing = open
        .iter()
        .any(|planner| !planner.workspace_id.as_deref().is_some_and(is_background));
    let listed = if needs_listing {
        cmux.listed_workspace_ids()
            .context("the planners' workspaces could not be listed")?
    } else {
        Vec::new()
    };
    let now = clock.now();
    let mut closed = Vec::new();
    for planner in open {
        let workspace = planner.workspace_id.as_deref().unwrap_or_default();
        let background = is_background(workspace);
        let workspace_listed = if background {
            // One that cannot be judged now is not given up.
            cmux.exists(workspace).unwrap_or(true)
        } else {
            listed.iter().any(|id| id.eq_ignore_ascii_case(workspace))
        };
        let probe = PlannerProbe {
            now,
            workspace_listed,
            wrapper_alive: planner.wrapper_pid.is_some_and(|pid| processes.alive(pid)),
            idle: None,
            working: None,
            screen_idle: None,
        };
        if planner.abandoned(&probe) {
            let (code, reason) = if background
                && planner.origin == PlannerOrigin::Runtime
                && planner.exited_at.is_some()
            {
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

/// What [`close_exited_person_planners`] did: the planners it closed, and
/// the workspaces it could not close (their rows stay open for the next
/// try).
#[derive(Debug, Default)]
pub struct PersonPlannersClosed {
    pub closed: Vec<PlannerId>,
    pub failures: Vec<String>,
}

/// Close the person's planners whose agent exited more than
/// [`PERSON_PLANNER_CLOSE_GRACE_SECS`] ago (ADR-t1300-1,
/// [`PlannerSession::person_exit_closes`]): the workspace, when cmux's one
/// listing of all windows has it, is closed the way a planner of the
/// runtime's is (unpinned first), then the row, with `planner_closed`. A
/// workspace not listed is left to [`close_abandoned_planners`]. A listing
/// cmux fails to give closes nothing (the error is returned); a close that
/// fails keeps the row open and is reported in `failures`.
pub fn close_exited_person_planners(
    queue: &dyn Queue,
    cmux: &dyn WorkspaceBackend,
    clock: &dyn Clock,
) -> Result<PersonPlannersClosed> {
    let now = clock.now();
    let due: Vec<PlannerSession> = queue
        .planners(false)?
        .into_iter()
        .filter(|planner| planner.person_exit_closes(now))
        .collect();
    let mut done = PersonPlannersClosed::default();
    if due.is_empty() {
        return Ok(done);
    }
    let listed = cmux
        .listed_workspace_ids()
        .context("the planners' workspaces could not be listed")?;
    for planner in due {
        let Some(workspace) = planner.workspace_id.as_deref() else {
            continue;
        };
        if !listed.iter().any(|id| id.eq_ignore_ascii_case(workspace)) {
            continue;
        }
        if let Err(error) = cmux.close(workspace) {
            done.failures.push(format!(
                "planner {}: its workspace {workspace} could not be closed: {error:#}",
                planner.id
            ));
            continue;
        }
        let reason = format!(
            "planner {} of a person: its agent exited (exit code {}) more than {PERSON_PLANNER_CLOSE_GRACE_SECS} seconds ago; its workspace and record are closed",
            planner.id,
            planner
                .exit_code
                .map_or_else(|| "unknown".to_owned(), |code| code.to_string()),
        );
        let payload =
            planner_closed_payload(&planner, PlannerCloseCode::PersonExited, true, &reason);
        if queue.end_planner(planner.id, &payload)? {
            done.closed.push(planner.id);
        }
    }
    Ok(done)
}

/// The line the wrapper of a person's planner prints once its agent exited
/// (ADR-t1300-1): the supervisor, not the wrapper, closes the workspace
/// after the grace.
pub fn exited_notice(id: PlannerId, code: i32, dir: &Path) -> String {
    format!(
        "dagq closes this workspace in {PERSON_PLANNER_CLOSE_GRACE_SECS} seconds (planner {id}, exit code {code}, log at {})",
        dir.join(PLANNER_DEBUG_LOG).display()
    )
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
