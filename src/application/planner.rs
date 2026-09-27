//! Planner sessions (ADR-0041 decisions 1, 6, 12, 13): on-demand cmux
//! workspaces where a planner writes goals and tasks and submits them as a
//! proposal. A person opens one with `dagq plan` (as many as they like at
//! once); the runtime opens one for a proposal plan review sent back while
//! its own planner was closed ([`open_runtime_planner`]) and for a draft
//! the runtime or a job registered ([`open_draft_planner`]). Each is a
//! `planners` row with its own directory under the queue's `planners/`
//! (its prompt, the wrapper binary, the agent's settings, log and idle
//! marker).
//!
//! The workspace runs the session wrapper `planner-session`, which starts
//! the agent, registers itself and its agent, heartbeats and records the
//! agent's exit, the way a run's wrapper does; the agent's `Stop` hook
//! writes the idle marker. [`planner_view`] judges from these whether the
//! planner is alive and idle.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    thread,
};
use tracing::warn;

use super::{
    AgentProvider, AgentSignals, Clock, PlannerCommand, ProcessControl, Queue, RunFiles, Spawner,
    Streams, WorkspaceBackend,
    lifecycle::{
        PLANNER_ID_ENV, PLANNER_ORIGIN_ENV, QueueWorkspaces, ROLE_STATUS_KEY, SESSION_KIND_ENV,
        session_look,
    },
    naming::{planner_workspace_name, shell_join},
    path_text, planner_idle_marker,
    prompt::{planner_prompt, runtime_planner_prompt},
};
use crate::domain::{
    ActorContext, IdleProbe, PlannerId, PlannerOrigin, PlannerProbe, PlannerSession, PlannerState,
    ProposalId, SessionRole, Task,
    actor::set_env,
    actor_model::{ActorLaunch, ModelRole, REVISE_ESCALATION, RoleModels},
    language::{Language, with_instruction},
    sessions::{LAUNCH_ENV, RUNTIME_PLANNER},
};

/// The planner's first message, which its wrapper hands the agent.
pub const PLANNER_PROMPT_FILE: &str = "prompt.txt";
/// The snapshot of the binary the planner's workspace runs as its wrapper,
/// so rebuilding the binary does not change a running one.
pub const PLANNER_RUNNER_FILE: &str = "runner";

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
    /// with.
    pub roles: RoleModels,
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

/// Open a planner a person talks with (`dagq plan`): a new workspace every
/// time, next to any planner already open.
pub fn open_person_planner(launch: &PlannerLaunch<'_>) -> Result<OpenedPlanner> {
    open_planner(
        launch,
        PlannerOrigin::Person,
        None,
        &planner_prompt(launch.db)?,
        &launch.roles.launch(ModelRole::Planner),
    )
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
    let prompt = runtime_planner_prompt(launch.db, proposal, tasks, reasons)?;
    let actor = launch
        .roles
        .launch(ModelRole::RuntimePlanner)
        .escalated(REVISE_ESCALATION);
    open_planner(
        launch,
        PlannerOrigin::Runtime,
        Some(proposal),
        &prompt,
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
    launch_planner(launch, planner, prompt, actor)
}

/// Open the workspace of a planner the runtime recorded for a draft
/// (ADR-0041 decision 16, [`super::DraftPlannerStore::open_draft_planner`])
/// with `prompt`, the way [`open_planner`] does, its agent starting with
/// `[roles.runtime_planner]`.
pub fn open_draft_planner(
    launch: &PlannerLaunch<'_>,
    planner: PlannerSession,
    prompt: &str,
) -> Result<OpenedPlanner> {
    let actor = launch.roles.launch(ModelRole::RuntimePlanner);
    launch_planner(launch, planner, prompt, &actor)
}

fn launch_planner(
    launch: &PlannerLaunch<'_>,
    planner: PlannerSession,
    prompt: &str,
    actor: &ActorLaunch,
) -> Result<OpenedPlanner> {
    let queue = launch.queue;
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
    let prompt = with_instruction(prompt.to_owned(), launch.language.as_ref());
    let opened = create_workspace(launch, &workspaces, &planner, &dir, &name, &prompt, actor);
    let workspace_id = match opened {
        Ok(id) => id,
        Err(error) => {
            queue.close_planner(planner.id, Some(&format!("{error:#}")))?;
            return Err(error);
        }
    };
    queue.planner_workspace_created(planner.id, &workspace_id)?;
    let mut warnings = workspaces.take_warnings();
    if let Some((color, icon)) = session_look(SessionRole::Planner) {
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
    files.write(&dir.join(PLANNER_PROMPT_FILE), prompt.as_bytes())?;
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
    let mut tags = workspaces.tags(SessionRole::Planner)?;
    if planner.origin == PlannerOrigin::Runtime {
        for (key, value) in &mut tags.env {
            if key == SESSION_KIND_ENV {
                *value = RUNTIME_PLANNER.to_owned();
            }
        }
    }
    tags.env.push((
        PLANNER_ORIGIN_ENV.to_owned(),
        planner.origin.as_str().to_owned(),
    ));
    tags.env
        .push((PLANNER_ID_ENV.to_owned(), planner.id.to_string()));
    let named = ActorContext::instance(crate::domain::ActorRole::Planner, planner.id);
    for (name, value) in named.env() {
        set_env(&mut tags.env, &name, value);
    }
    tags.env
        .push((LAUNCH_ENV.to_owned(), actor.to_value().to_string()));
    if let Some(description) = &mut tags.description {
        description.push_str(&format!(" planner={}", planner.id));
    }
    launch
        .cmux
        .create_named(name, launch.repo_root, &shell_join(&argv), &tags)
}

/// What the planner's session wrapper works with, as the run's does.
pub struct PlannerWrapper<'a> {
    pub queue: &'a dyn Queue,
    pub provider: &'a dyn AgentProvider,
    pub spawner: &'a dyn Spawner,
    pub files: &'a dyn RunFiles,
    pub pid: u32,
}

/// The session wrapper of planner `id` (`planner-session`): register this
/// process, start the agent with the planner's prompt in `cwd` (with
/// `model`, the model and effort its opener chose, when there are),
/// register it, heartbeat until it exits, and record its exit code, which
/// it returns.
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
        provider,
        spawner,
        files,
        pid,
    } = ctx;
    queue.register_planner_wrapper(id, pid)?;
    // An agent that never starts is an exit too, or the planner would look
    // lost rather than over.
    let started = queue
        .planner(id)
        .and_then(|planner| {
            let prompt = files.read_to_string(&dir.join(PLANNER_PROMPT_FILE))?;
            Ok((planner.origin, prompt))
        })
        .and_then(|(origin, prompt)| {
            let mut command = provider.planner_command(&PlannerCommand {
                origin,
                dir,
                cwd,
                prompt: &prompt,
                plugin_dir,
            })?;
            if let Some((model, effort)) = model {
                provider.select_model(&mut command, model, effort);
            }
            Ok(command)
        })
        .and_then(|command| {
            spawner
                .spawn(&command, Streams::Inherit)
                .context("launch agent")
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
}

/// What judging a planner reads: cmux for its workspace and screen, the
/// processes for its wrapper, the files for its idle marker, the agent's
/// signals for the marker and the screen, and the clock.
pub struct PlannerProbes<'a> {
    pub cmux: &'a dyn WorkspaceBackend,
    pub processes: &'a dyn ProcessControl,
    pub files: &'a dyn RunFiles,
    pub signals: &'a dyn AgentSignals,
    pub clock: &'a dyn Clock,
    pub planners_dir: &'a Path,
}

/// Judge `planner` the way a worker session is judged: its workspace UUID
/// in every window's `cmux workspace list`, its wrapper's pid and heartbeat, the idle
/// marker its agent's `Stop` hook wrote and, with a marker, whether the
/// screen shows the agent at work on a new turn. A closed planner is not
/// looked at.
pub fn planner_view(probes: &PlannerProbes<'_>, planner: PlannerSession) -> Result<PlannerView> {
    let dir = planner_dir(probes.planners_dir, planner.id);
    let mut probe = PlannerProbe {
        now: probes.clock.now(),
        workspace_listed: false,
        wrapper_alive: false,
        idle: None,
        working: None,
    };
    if planner.closed_at.is_none() {
        if let Some(workspace) = &planner.workspace_id {
            probe.workspace_listed = probes.cmux.exists(workspace)?;
        }
        probe.wrapper_alive = planner
            .wrapper_pid
            .is_some_and(|pid| probes.processes.alive(pid));
        if let Some((modified, bytes)) = probes.files.read_stamped(&planner_idle_marker(&dir))? {
            probe.idle = Some(IdleProbe {
                since: super::unix_seconds(modified),
                background_running: probes.signals.idle_hook(&bytes).background_running,
            });
            if probe.workspace_listed
                && let Some(workspace) = &planner.workspace_id
            {
                probe.working = probes
                    .cmux
                    .capture(workspace)
                    .ok()
                    .map(|screen| probes.signals.working(&screen));
            }
        }
    }
    let state = planner.state(&probe);
    Ok(PlannerView {
        state,
        alive: state.alive(),
        idle_since: probe
            .idle
            .filter(|_| state == PlannerState::Idle)
            .map(|idle| idle.since),
        dir,
        planner,
    })
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
        .map(|planner| planner_view(probes, planner))
        .collect()
}

/// Close the rows of the planners whose session is over for good
/// ([`PlannerSession::abandoned`]), a person's included: their workspace is
/// not in cmux's one listing of all windows and their wrapper is done. The
/// rows are read before the listing, so a workspace opened after it is not
/// taken for gone. A listing cmux fails to give closes nothing (the error
/// is returned): a passing failure gives no planner up. Returns the IDs
/// closed.
pub fn close_abandoned_planners(
    queue: &dyn Queue,
    cmux: &dyn WorkspaceBackend,
    processes: &dyn ProcessControl,
    clock: &dyn Clock,
) -> Result<Vec<PlannerId>> {
    let open: Vec<PlannerSession> = queue
        .planners(false)?
        .into_iter()
        .filter(|planner| planner.workspace_id.is_some())
        .collect();
    if open.is_empty() {
        return Ok(Vec::new());
    }
    let listed = cmux
        .listed_workspace_ids()
        .context("the planners' workspaces could not be listed")?;
    let now = clock.now();
    let mut closed = Vec::new();
    for planner in open {
        let probe = PlannerProbe {
            now,
            workspace_listed: planner.workspace_id.as_deref().is_some_and(|workspace| {
                listed.iter().any(|id| id.eq_ignore_ascii_case(workspace))
            }),
            wrapper_alive: planner.wrapper_pid.is_some_and(|pid| processes.alive(pid)),
            idle: None,
            working: None,
        };
        if planner.abandoned(&probe) {
            queue.close_planner(planner.id, None)?;
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
