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
    planner_session_with_provider(db, id, &provider, plugin_dir, model)
}

/// How a planner's wrapper was started: for a headless planner
/// (`--headless`, ADR-t1394-2), and then in the background
/// (`--background`, ADR-t1404-1 decision 8).
#[derive(Debug, Clone, Copy, Default)]
pub struct PlannerEntry {
    pub headless: bool,
    pub background: bool,
}

/// [`planner_session`] with any provider, in the working directory.
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
    let cwd = std::env::current_dir().context("working directory is unavailable")?;
    planner::run_planner_session(
        PlannerWrapper {
            queue: &mut queue,
            db,
            provider,
            spawner: &LocalSpawner,
            files: &LocalRunFiles,
            processes: &SystemProcesses,
            pid: std::process::id(),
            clock: &crate::infrastructure::clock::SystemClock,
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
