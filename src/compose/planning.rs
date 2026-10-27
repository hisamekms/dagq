//! The wiring of 計画管理 (docs/design/architecture.md): the runtime's
//! planners (`planners`, `planner request`) and a planner's session
//! wrapper. Each entry builds the adapters and calls the use case.

use super::{OneShot, wrapper_entry};
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
    domain::PlannerId,
    infrastructure::{
        adapters::{ClaudeCode, SystemProcesses},
        location::planners_dir,
        process::LocalSpawner,
        run_files::LocalRunFiles,
        sqlite::SqliteQueue,
    },
};

/// The default planner timeout: an hour, like a run's resume timeout
/// (ADR-0041 decision 13).
pub const PLANNER_TIMEOUT: Duration = Duration::from_secs(3600);

/// `planners` on the system clock: see [`OneShot::planners`].
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
    claude: &Path,
    plugin_dir: Option<&Path>,
    model: Option<(&str, &str)>,
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
    let provider = ClaudeCode {
        executable: claude.into(),
    };
    let mut queue = crate::infrastructure::queue_service::ServiceWrapperStore::of_queue(
        db,
        crate::domain::session_step::StepTarget::of(
            crate::domain::session_step::TargetKind::Planner,
            id.as_i64(),
        ),
    );
    planner_session_on(&mut queue, db, id, &provider, plugin_dir, model)
}

/// How a planner's wrapper was started: for a headless planner
/// (`--headless`, ADR-t1394-2), and then in the background
/// (`--background`, ADR-t1404-1 decision 8).
#[derive(Debug, Clone, Copy, Default)]
pub struct PlannerEntry {
    pub headless: bool,
    pub background: bool,
}

/// [`planner_session`] with any provider, in the working directory, on
/// the queue at `db` itself: a test's wrapper, which a library call runs.
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
    planner_session_on(&mut queue, db, id, provider, plugin_dir, model)
}

/// The wrapper of planner `id` of the queue at `db`, reading and recording
/// on `queue`.
fn planner_session_on(
    queue: &mut dyn crate::application::session::WrapperStore,
    db: &Path,
    id: PlannerId,
    provider: &dyn AgentProvider,
    plugin_dir: Option<&Path>,
    model: Option<(&str, &str)>,
) -> Result<Value> {
    let cwd = std::env::current_dir().context("working directory is unavailable")?;
    planner::run_planner_session(
        PlannerWrapper {
            queue,
            db,
            provider,
            spawner: &LocalSpawner,
            files: &LocalRunFiles,
            processes: &SystemProcesses,
            pid: std::process::id(),
            clock: &crate::infrastructure::clock::SystemClock,
            telemetry: &crate::infrastructure::queue_service::SystemServiceAccess,
            queue_service: Some(&crate::infrastructure::queue_service::HandedServiceAccess),
        },
        id,
        &planner::planner_dir(&planners_dir(db), id),
        &cwd,
        plugin_dir,
        model,
    )
}

impl OneShot {
    /// `planners`: every planner not closed (with `all`, every one), with
    /// its state judged by [`planner::planner_views`].
    pub fn planners(&self, db: &Path, sessions: &dyn SessionWrappers, all: bool) -> Result<Value> {
        self.planners_of(&self.open_read_only(db)?, db, sessions, all)
    }

    /// [`Self::planners`] on `queue`, the queue at `db` the caller already
    /// opened, so a command opens it once.
    pub fn planners_of(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        sessions: &dyn SessionWrappers,
        all: bool,
    ) -> Result<Value> {
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
    pub fn request_planner(
        &self,
        queue: &mut SqliteQueue,
        db: &Path,
        sessions: &dyn SessionWrappers,
        planner: PlannerId,
        words: &str,
    ) -> Result<Value> {
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
pub fn write_queue(
    queue: &mut SqliteQueue,
    actor: &crate::domain::ActorContext,
    write: &crate::application::queue_writes::QueueWrite,
    owner: impl FnOnce(&SqliteQueue) -> Result<crate::domain::PlannerOwner>,
) -> Result<Value> {
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
pub fn service_writes() -> crate::infrastructure::queue_service::ServiceWrites {
    std::sync::Arc::new(|queue, actor, write| {
        write_queue(queue, actor, write, |queue| {
            crate::application::queue_writes::principal_owner(queue, actor)
        })
    })
}
