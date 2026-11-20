//! The wiring of 計画管理 (docs/design/architecture.md): the runtime's
//! planners (`planners`, `planner request`) and a planner's session
//! wrapper. Each entry builds the adapters and calls the use case.

use super::{OneShot, wrapper_entry};
#[cfg(any(test, feature = "test-paths"))]
use crate::infrastructure::sqlite::SqliteQueue;
use crate::infrastructure::{sql_exec::TransactionEntries, store_queue::StoreQueue};

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    application::{
        AgentProvider, SessionWrappers,
        planner::{self, PlannerProbes, PlannerWrapper},
    },
    domain::{
        PlannerId, Provider,
        actor_model::{ActorLaunch, ModelRole},
    },
    infrastructure::{
        adapters::{ClaudeCode, SystemProcesses},
        location::planners_dir,
        process::LocalSpawner,
        run_files::LocalRunFiles,
    },
};

/// The default planner timeout: an hour, like a run's resume timeout
/// (ADR-0041 decision 13).
pub const PLANNER_TIMEOUT: Duration = Duration::from_secs(3600);

/// `planners` on the system clock: see [`OneShot::planners`].
#[cfg(feature = "test-paths")]
pub fn planners(db: &Path, sessions: &dyn SessionWrappers, all: bool) -> Result<Value> {
    OneShot::system().planners(db, sessions, all)
}

/// The session wrapper of a planner (`planner-session`). A planner of the
/// runtime's starts it with `--headless --background`, a process of its
/// own with no terminal and no workspace (ADR-t1433-2). Without them it is
/// a person's planner's wrapper, run from its cmux workspace: stdout must
/// remain a terminal for Claude. A refused wrapper ends and leaves any
/// workspace it runs in alone.
pub fn planner_session(
    db: &Path,
    id: PlannerId,
    (kind, executable, home): (Provider, &Path, Option<PathBuf>),
    plugin_dir: Option<&Path>,
    (model, effort): (Option<String>, Option<String>),
    entry: PlannerEntry,
) -> Result<Value> {
    // A headless planner's agent has no terminal (ADR-t1394-2); one started
    // in the background leads a session of its own (ADR-t1404-1).
    if entry.headless {
        wrapper_entry(entry.background)?;
    } else {
        ensure!(
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            "interactive Claude wrapper requires a terminal"
        );
    }
    let provider: Box<dyn AgentProvider> = match kind {
        Provider::Claude => Box::new(ClaudeCode {
            executable: executable.into(),
        }),
        Provider::Codex => Box::new(crate::infrastructure::codex::Codex {
            executable: executable.into(),
            home,
        }),
    };
    // The opener passes the launch's provenance alongside the wrapper's
    // arguments. Direct invocations have only those arguments.
    let launch = std::env::var(crate::domain::sessions::LAUNCH_ENV)
        .ok()
        .and_then(|launch| serde_json::from_str::<ActorLaunch>(&launch).ok())
        .filter(|launch| {
            launch.role == ModelRole::RuntimePlanner
                && launch.provider == kind
                && launch.model == model
                && launch.effort == effort
        })
        .unwrap_or(ActorLaunch {
            provider: kind,
            model,
            effort,
            ..ActorLaunch::default_of(ModelRole::RuntimePlanner)
        });
    let mut queue = crate::infrastructure::queue_service::ServiceWrapperStore::of_queue(
        db,
        crate::domain::session_step::StepTarget::of(
            crate::domain::session_step::TargetKind::Planner,
            id.as_i64(),
        ),
    );
    planner_session_on_host(
        &mut queue,
        db,
        id,
        &*provider,
        plugin_dir,
        &launch,
        entry.read_snapshot.as_deref(),
    )
}

/// How a planner's wrapper was started: for a headless planner
/// (`--headless`, ADR-t1394-2), and then in the background
/// (`--background`, ADR-t1404-1 decision 8).
#[derive(Debug, Clone, Default)]
pub struct PlannerEntry {
    pub headless: bool,
    pub background: bool,
    pub read_snapshot: Option<PathBuf>,
}

/// [`planner_session`] with any provider, in the working directory, on
/// the queue at `db` itself: a test's wrapper, which a library call runs.
#[cfg(feature = "test-paths")]
pub fn planner_session_with_provider(
    db: &Path,
    id: PlannerId,
    provider: &dyn AgentProvider,
    plugin_dir: Option<&Path>,
    model: Option<(&str, &str)>,
) -> Result<Value> {
    let mut queue =
        SqliteQueue::open(db)?.with_actor(crate::domain::actor::ActorContext::instance(
            crate::domain::actor::ActorRole::Wrapper,
            format_args!("planner:{id}"),
        ));
    let mut launch = ActorLaunch::default_of(ModelRole::RuntimePlanner);
    if let Some((model, effort)) = model {
        launch.model = Some(model.to_owned());
        launch.effort = Some(effort.to_owned());
    }
    planner_session_on_host(&mut queue, db, id, provider, plugin_dir, &launch, None)
}

/// The wrapper of planner `id` of the queue at `db`, reading and recording
/// on `queue`.
fn planner_session_on_host(
    queue: &mut dyn crate::application::session::WrapperStore,
    db: &Path,
    id: PlannerId,
    provider: &dyn AgentProvider,
    plugin_dir: Option<&Path>,
    launch: &ActorLaunch,
    read_snapshot: Option<&Path>,
) -> Result<Value> {
    let cwd = std::env::current_dir().context("working directory is unavailable")?;
    super::planner_session_on(
        PlannerWrapper {
            queue,
            db,
            provider,
            spawner: &LocalSpawner,
            files: &LocalRunFiles,
            processes: &SystemProcesses,
            pid: std::process::id(),
            clock: &crate::infrastructure::clock::SystemClock,
            poll_interval: provider.wait_interval(),
            telemetry: &crate::infrastructure::queue_service::SystemServiceAccess,
            queue_service: Some(&crate::infrastructure::queue_service::HandedServiceAccess),
            read_snapshot,
        },
        id,
        &cwd,
        plugin_dir,
        launch,
    )
}

impl OneShot {
    /// `planners`: every planner not closed (with `all`, every one), with
    /// its state judged by [`planner::planner_views`].
    #[cfg(feature = "test-paths")]
    pub fn planners(&self, db: &Path, sessions: &dyn SessionWrappers, all: bool) -> Result<Value> {
        self.planners_of(&self.open_read_only(db)?, db, sessions, all)
    }

    /// [`Self::planners`] on `queue`, the queue at `db` the caller already
    /// opened, so a command opens it once.
    pub fn planners_of<E: TransactionEntries>(
        &self,
        queue: &StoreQueue<E>,
        db: &Path,
        sessions: &dyn SessionWrappers,
        all: bool,
    ) -> Result<Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        // Claude Code's signals only read what its hook shows.
        let signals = ClaudeCode {
            executable: PathBuf::from("claude"),
        };
        let views = planner::planner_views(
            queue,
            &PlannerProbes {
                sessions,
                processes: &SystemProcesses,
                files: &LocalRunFiles,
                signals: &signals,
                clock: &*self.generators.clock,
                planners_dir: &planners_dir(db),
            },
            all,
        )?;
        Ok(serde_json::json!({ "planners": views }))
    }

    /// `planner request ID`: hand `words` to the headless planner `planner`
    /// of `queue` (at `db`) as its next turn, the planner judged as
    /// [`Self::planners_of`] judges it ([`crate::application::planner_request::request_planner`]).
    pub fn request_planner<E: TransactionEntries>(
        &self,
        queue: &mut StoreQueue<E>,
        db: &Path,
        sessions: &dyn SessionWrappers,
        planner: PlannerId,
        words: &str,
    ) -> Result<Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        let signals = ClaudeCode {
            executable: PathBuf::from("claude"),
        };
        let probes = PlannerProbes {
            sessions,
            processes: &SystemProcesses,
            files: &LocalRunFiles,
            signals: &signals,
            clock: &*self.generators.clock,
            planners_dir: &planners_dir(db),
        };
        crate::application::planner_request::request_planner(queue, &probes, planner, words)
    }
}

/// Run the planning command `write` on `queue` as `actor`, as the command
/// line and the queue service both run it
/// ([`crate::application::queue_writes::answer`]): the queue first held to
/// the repository's rules the command needs (its set of changes for the
/// tasks `add`, `edit` and `submit` register, its goal tags for the goals
/// `goal add`, `goal edit` and `submit` register, ADR-t1639-1 decision 6,
/// and a goal's limit of Spike cycles and the prefixes of a task's title
/// for `submit`, ADR-t1487-1 decision 8). `owner` gives the planner a
/// submit's proposal belongs to.
pub fn write_queue<E: TransactionEntries>(
    queue: &mut StoreQueue<E>,
    actor: &crate::domain::ActorContext,
    write: &crate::application::queue_writes::QueueWrite,
    owner: impl FnOnce(&StoreQueue<E>) -> Result<crate::domain::PlannerOwner>,
) -> Result<Value>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    use crate::application::queue_writes::QueueWrite;
    let mut rules = crate::infrastructure::sqlite::PlanningRules::default();
    if matches!(
        write,
        QueueWrite::Add(_) | QueueWrite::Edit(_) | QueueWrite::Submit(_)
    ) {
        rules.changes = Some(super::task_changes(queue)?);
    }
    if matches!(
        write,
        QueueWrite::GoalAdd(_) | QueueWrite::GoalEdit(_) | QueueWrite::Submit(_)
    ) {
        rules.goal_tags = Some(super::goal_tags(queue)?);
    }
    if let QueueWrite::Submit(submit) = write {
        rules.max_spike_cycles = Some(super::max_spike_cycles(queue)?);
        rules.title_prefixes = Some(super::task_title_prefixes(queue)?);
        // The replacement plan of a replanning operation's request
        // (ADR-t1521-2 decision 4), as the caller read it.
        rules.replacement = Some(
            submit
                .replacement
                .as_deref()
                .map(crate::domain::replan_plan::ReplacementPlan::parse)
                .transpose()?,
        );
    }
    queue.hold_planning_to(rules);
    // Read only for a submit, the one write it is asked for.
    let owner = matches!(write, QueueWrite::Submit(_)).then(|| owner(queue));
    crate::application::queue_writes::answer(
        queue,
        actor,
        &crate::domain::StaticPolicy,
        write,
        || owner.context("only a submit has an owner")?,
    )
}

/// How the queue service runs a planning command: [`write_queue`], with a
/// submit's proposal owned by the caller's principal
/// ([`crate::application::queue_writes::principal_owner`], the queue as its
/// planners' records).
pub fn service_writes<E: TransactionEntries + 'static>()
-> crate::infrastructure::queue_service::ServiceWrites<StoreQueue<E>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    std::sync::Arc::new(|queue, actor, write| {
        write_queue(queue, actor, write, |queue| {
            crate::application::queue_writes::principal_owner(queue, actor)
        })
    })
}
