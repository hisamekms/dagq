//! The observer's files under `<queue dir>/observer` (one directory per
//! observation and the hourly cursor), the configuration its agent starts
//! with, and the headless agent's process, for
//! [`crate::application::observer`] and the throughput review.
use crate::{
    application::{
        AgentProvider, ProcessControl, Streams,
        actor_executor::{
            ActorExecutionSpec, ActorExecutor, ActorProgram, HeadlessProgram, HostActorExecutor,
            WorkspaceAccess,
        },
        observer::{self as use_case, HeadlessAgent, ObserverHost},
    },
    domain::{
        EventId,
        actor_model::{ActorLaunch, ModelRole},
        language::Language,
    },
    infrastructure::{adapters::SystemProcesses, process::LocalSpawner},
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

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
/// `[roles.observer]` of the bound checkout's `dagq.toml` (its provider
/// too, ADR-t1222-1); none, no checkout, or a file that cannot be read
/// starts it as before.
pub fn observer_launch(checkout: Option<&Path>) -> ActorLaunch {
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

/// `observe --input`: [`use_case::read_input`] of the observation's
/// `input.json` under `<queue dir>/observer`.
pub fn read_input(
    db: &Path,
    observation: &str,
    section: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<Value> {
    use_case::read_input(
        observation,
        || {
            let path = observer_dir(db).join(observation).join("input.json");
            Ok(fs::read_to_string(path)?)
        },
        section,
        offset,
        limit,
    )
}

/// Start the agent in `dir` with stdout in `output.out` and stderr in
/// `output.err`, and wait for it up to the timeout (then kill it and what
/// it started, so no Bash child of the agent outlives it: an error).
/// The exit code, or `None`
/// when a signal ended it.
pub fn run_agent(
    provider: &dyn AgentProvider,
    db: &Path,
    dir: &Path,
    prompt: &str,
    agent: &HeadlessAgent<'_>,
) -> Result<Option<i32>> {
    let stdout = dir.join("output.out");
    let stderr = dir.join("output.err");
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    if let Some(bin) = agent.dagq.parent() {
        let mut paths = vec![bin.to_path_buf()];
        paths.extend(std::env::split_paths(&path));
        path = std::env::join_paths(paths)?;
    }
    let path = path
        .into_string()
        .map_err(|_| anyhow::anyhow!("PATH is not UTF-8"))?;
    // Where an agent found again by its provider's name is recorded
    // (ADR-t2079-1); a queue that does not open leaves it to the log.
    let events = crate::infrastructure::sqlite::SqliteQueue::open(db).ok();
    let mut executor = HostActorExecutor::new(db)
        .with_provider(provider)
        .with_spawner(&LocalSpawner)
        .with_queue_service(&crate::infrastructure::queue_service::SystemServiceAccess);
    if let Some(events) = &events {
        executor = executor.with_events(events);
    }
    let mut child = executor
        .spawn(
            ActorExecutionSpec::new(
                agent.actor.clone(),
                WorkspaceAccess::Scratch(dir.to_path_buf()),
                ActorProgram::Headless {
                    program: HeadlessProgram::Job {
                        cwd: dir,
                        prompt,
                        access: agent.access,
                    },
                    session_id: agent.session_id,
                    launch: Some(agent.launch),
                    without_mcp: true,
                    without_env: &[],
                    env: vec![("PATH".to_owned(), path)],
                    streams: Streams::Files {
                        stdout: &stdout,
                        stderr: &stderr,
                    },
                },
            )
            .with_timeout(agent.timeout),
        )
        .with_context(|| HeadlessAgent::start_context(agent.what))?
        .process()?;
    let deadline = Instant::now() + agent.timeout;
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
                "{} did not finish within {}s",
                agent.what,
                agent.timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// The observer's host: the files under `<queue dir>/observer`, the bound
/// checkout's configuration and the agent's local process.
pub struct LocalObserver;

impl ObserverHost for LocalObserver {
    fn read_cursor(&self, db: &Path) -> Result<Option<EventId>> {
        read_cursor(db)
    }
    fn write_cursor(&self, db: &Path, cursor: EventId) -> Result<()> {
        write_cursor(db, cursor)
    }
    fn observation_dir(&self, db: &Path, started: i64) -> Result<PathBuf> {
        observation_dir(db, started)
    }
    fn write(&self, path: &Path, contents: &str) -> Result<()> {
        Ok(fs::write(path, contents)?)
    }
    fn output(&self, dir: &Path) -> (String, String) {
        (
            fs::read_to_string(dir.join("output.out")).unwrap_or_default(),
            fs::read_to_string(dir.join("output.err")).unwrap_or_default(),
        )
    }
    fn launch(&self, checkout: Option<&Path>) -> ActorLaunch {
        observer_launch(checkout)
    }
    fn language(&self, checkout: Option<&Path>, user_config: Option<&Path>) -> Option<Language> {
        crate::infrastructure::language::language_for_prompt(checkout, user_config)
    }
    fn run(
        &self,
        provider: &dyn AgentProvider,
        db: &Path,
        dir: &Path,
        prompt: &str,
        agent: &HeadlessAgent<'_>,
    ) -> Result<Option<i32>> {
        run_agent(provider, db, dir, prompt, agent)
    }
}
