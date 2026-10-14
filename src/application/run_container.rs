//! The executor of a worker set to the container ([Container runtime],
//! ADR-t2113-3 decision 1, ADR-t2114-2 decision 1): each turn of a run
//! whose worker works in a clone runs in a container of the dagq machine
//! instead of a process of the host ([`PodmanActorExecutor`]), through the
//! [`RunContainers`] port, which makes the machine, the image, the run's
//! relay and its build output ready, hands the run token over as a file
//! of the run directory, starts `podman` and removes a run's container.
//!
//! The session wrapper stays on the host (the control side): it reads the
//! turn's output and the run's files as for a turn on the host, and the
//! container's launcher (`dagq relay turn`) writes the run's bundle and
//! state at the turn's end. Whatever stops a run's session stops its
//! container too ([`RunContainers::remove`]).
//!
//! [Container runtime]: ../../docs/design/container-runtime.md

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};

use super::actor_executor::{
    ActorExecutionSpec, ActorExecutor, ActorHandle, ActorProgram, EnforcementLevel,
    ExecutorBackend, SessionAgent,
};
use super::queue_service::ServiceAccess;
use super::{AgentProvider, CommandSpec, Spawner, Streams, TurnTarget};
use crate::domain::actor::ACTOR_ENV;
use crate::domain::podman_machine::QueueKey;
use crate::domain::queue_service::{CLIENT_ENV, CREDENTIAL_FILE_ENV, Principal};
use crate::domain::run_container::{
    AGENT, ContainerLimits, HOME_DIR, WITHOUT_ENV, WorkerContainer, launcher_command,
    worker_run_args,
};
use crate::domain::{ActorRole, Provider, RunId, TaskRun};

/// What a turn's container runs with, once [`RunContainers::prepare`] made
/// its parts ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRun {
    /// The queue's name among the machine's containers and volumes.
    pub queue: QueueKey,
    /// The image to run.
    pub image: String,
    /// The directory in the container the run's build output volume is
    /// mounted at (`[container.build_output]` of `dagq.toml`); `None`
    /// mounts none.
    pub build_output: Option<String>,
    /// `[run.env]` of the main checkout's `dagq.toml`, for the run.
    pub run_env: Vec<(String, String)>,
    /// The limits of the container (`host.toml`).
    pub limits: ContainerLimits,
}

/// The containers of the runs of one queue in the dagq machine.
pub trait RunContainers: Send + Sync {
    /// Make what a turn of `run` needs ready: the dagq machine running
    /// with the run as a user, the image built, the run's relay open and
    /// its build output volume made (when `dagq.toml` names its directory).
    /// `Err` says why the turn cannot run in the container; it is not run
    /// anywhere else (fail closed).
    fn prepare(&self, run: &TaskRun) -> Result<PreparedRun>;
    /// Write the token of `credential` (the worker's token file of the
    /// queue service) to the run directory `run_dir` as
    /// [`TOKEN_FILE`](crate::domain::run_container::TOKEN_FILE) with mode
    /// 0600, following no symlink, and make the agent's home there; the
    /// token file's path.
    fn prepare_run_dir(&self, run_dir: &Path, credential: &Path) -> Result<PathBuf>;
    /// `podman --connection dagq` with `args`.
    fn command(&self, args: &[String]) -> CommandSpec;
    /// Stop and remove the container of the run `run`; nothing when there
    /// is none.
    fn remove(&self, run: &RunId) -> Result<()>;
}

/// Starts a worker's turns in their container. Only a Claude worker's
/// turn of a run in a clone is started: any other program, role or
/// provider is refused, and never started on the host instead.
pub struct PodmanActorExecutor<'a> {
    queue: &'a Path,
    provider: &'a dyn AgentProvider,
    spawner: &'a dyn Spawner,
    service: &'a dyn ServiceAccess,
    containers: &'a dyn RunContainers,
}

impl<'a> PodmanActorExecutor<'a> {
    /// The executor of the queue at `queue`, whose agent's command
    /// `provider` makes and whose `podman` `spawner` starts.
    pub fn new(
        queue: &'a Path,
        provider: &'a dyn AgentProvider,
        spawner: &'a dyn Spawner,
        service: &'a dyn ServiceAccess,
        containers: &'a dyn RunContainers,
    ) -> Self {
        Self {
            queue,
            provider,
            spawner,
            service,
            containers,
        }
    }
}

impl ActorExecutor for PodmanActorExecutor<'_> {
    fn backend(&self) -> ExecutorBackend {
        ExecutorBackend::Container
    }

    fn enforcement(&self) -> EnforcementLevel {
        EnforcementLevel::Sandbox
    }

    fn spawn(&self, spec: ActorExecutionSpec<'_>) -> Result<ActorHandle> {
        spec.check()?;
        ensure!(
            spec.role() == ActorRole::Worker,
            "only a worker's turn runs in the container, not {}",
            spec.role().as_str()
        );
        // The queue broker has no destination of OpenAI's (ADR-t2114-1
        // decision 2).
        ensure!(
            spec.program.provider() == Provider::Claude,
            "a Codex worker does not run in the container: the queue broker has no destination of its provider"
        );
        let ActorExecutionSpec { actor, program, .. } = spec;
        let ActorProgram::SessionAgent {
            agent:
                SessionAgent::Turn {
                    run,
                    prompt,
                    session,
                    stdout,
                    stderr,
                    ..
                },
            model,
        } = program
        else {
            bail!("only a worker's turn runs in the container");
        };
        ensure!(
            run.in_container(),
            "run {} works in a worktree of the host, not a clone: its turn does not run in the container",
            run.id()
        );
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let clone = Path::new(run.worktree_path().context("missing clone")?);
        let branch = run.branch().context("missing run branch")?;
        let target = TurnTarget::of_run(run)?;
        let mut agent = self.provider.turn_command(&target, prompt, session)?;
        if let Some((model, effort)) = model {
            self.provider.select_model(&mut agent, model, effort);
        }
        // The container's failures are this executor's, never the
        // provider's: no cause of theirs is kept as an `io::Error`, which
        // the wrapper would take for an agent that cannot be run.
        let prepared = self
            .containers
            .prepare(run)
            .map_err(|error| anyhow!("make the run's container ready: {error:#}"))?;
        // The token the supervisor issued at the claim or the resume; one
        // is issued only when there is none.
        let credential = match self.service.credential(self.queue, actor.actor_id()) {
            Some(file) => file,
            None => self.service.issue(self.queue, &Principal::of(&actor))?,
        };
        let token = self
            .containers
            .prepare_run_dir(run_dir, &credential)
            .map_err(|error| anyhow!("hand the run token to the container: {error:#}"))?;
        let mut env = turn_variables(&prepared.run_env, &agent);
        env.extend(actor.env());
        env.push((
            CREDENTIAL_FILE_ENV.to_owned(),
            token
                .to_str()
                .context("the token's path is not UTF-8")?
                .to_owned(),
        ));
        env.push((
            "HOME".to_owned(),
            run_dir
                .join(HOME_DIR)
                .to_str()
                .context("the run directory is not UTF-8")?
                .to_owned(),
        ));
        let mut agent_args = vec![AGENT.to_owned()];
        for arg in agent.get_args() {
            agent_args.push(
                arg.to_str()
                    .context("an argument of the agent is not UTF-8")?
                    .to_owned(),
            );
        }
        let command = launcher_command(
            run_dir,
            clone,
            run.base_commit().as_str(),
            branch,
            &agent_args,
        );
        let args = worker_run_args(&WorkerContainer {
            queue: &prepared.queue,
            run: run.id().as_str(),
            image: &prepared.image,
            clone,
            run_dir,
            build_output: prepared.build_output.as_deref(),
            limits: prepared.limits,
            env: &env,
            command: &command,
        })
        .map_err(|why| anyhow!("the run's container cannot be made: {why}"))?;
        let mut podman = self.containers.command(&args);
        // Never in the clone: the control side starts no program there
        // (ADR-t2179-1 decision 1).
        podman.current_dir(run_dir).new_session();
        // A podman that cannot be started is this executor's failure, not
        // the provider's (whose agent never started on the host).
        let child = self
            .spawner
            .spawn(&podman, Streams::Files { stdout, stderr })
            .map_err(|error| anyhow!("start the run's container with podman: {error:#}"))?;
        Ok(ActorHandle::Process(child))
    }
}

/// The variables a turn's container is given beside the actor's and the
/// token file's: `[run.env]` but [`WITHOUT_ENV`], then what the agent's
/// command sets; none naming the actor, the queue service or the home,
/// which the executor sets itself.
fn turn_variables(run_env: &[(String, String)], agent: &CommandSpec) -> Vec<(String, String)> {
    let reserved = |key: &str| {
        WITHOUT_ENV.contains(&key)
            || ACTOR_ENV.contains(&key)
            || CLIENT_ENV.contains(&key)
            || key == "HOME"
    };
    let from_agent = agent
        .get_envs()
        .filter_map(|(key, value)| Some((key.to_str()?.to_owned(), value?.to_str()?.to_owned())));
    run_env
        .iter()
        .cloned()
        .chain(from_agent)
        .filter(|(key, _)| !reserved(key))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_s_variables_leave_the_host_s_compiler_wrapper_and_the_actor_s_names_out() {
        let run_env = vec![
            ("RUSTC_WRAPPER".to_owned(), "sccache".to_owned()),
            ("CARGO_TARGET_DIR".to_owned(), "/build".to_owned()),
            ("DAGQ_RUN_ID".to_owned(), "other".to_owned()),
            ("DAGQ_SERVICE_SOCKET".to_owned(), "/x".to_owned()),
            ("HOME".to_owned(), "/Users/me".to_owned()),
        ];
        let mut agent = CommandSpec::new("claude");
        agent.env("CLAUDE_X", "1").env_remove("GONE");
        assert_eq!(
            turn_variables(&run_env, &agent),
            [
                ("CARGO_TARGET_DIR".to_owned(), "/build".to_owned()),
                ("CLAUDE_X".to_owned(), "1".to_owned()),
            ]
        );
    }
}
