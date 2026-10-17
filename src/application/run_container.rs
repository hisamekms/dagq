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
//! The verification of `integrate` set to the container runs its commands
//! in containers through the [`VerificationContainers`] port.
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
use crate::domain::local_checks;
use crate::domain::podman_machine::QueueKey;
use crate::domain::queue_service::{CLIENT_ENV, CREDENTIAL_FILE_ENV, Principal};
use crate::domain::run_container::{
    AGENT, ContainerLimits, HOME_DIR, WITHOUT_ENV, WorkerContainer, launcher_command,
    worker_run_args,
};
use crate::domain::{ActorRole, CommitSha, Provider, RunId, TaskRun};

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

/// One verification of `integrate` in the container, once
/// [`VerificationContainers::prepare`] made its parts ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedVerification {
    /// The verification's ID: its container's and build output's name.
    pub id: String,
    pub queue: QueueKey,
    pub image: String,
    /// The verification's directory: its clone
    /// ([`VERIFICATION_CLONE`](crate::domain::run_container::VERIFICATION_CLONE))
    /// and its token's copy.
    pub dir: PathBuf,
    /// Where the preparation command writes the materials, in the
    /// verification's directory
    /// ([`STAGED_MATERIALS`](crate::domain::run_container::STAGED_MATERIALS)),
    /// made empty.
    pub staged: PathBuf,
    /// The materials' copy in the run directory
    /// ([`MATERIALS_DIR`](crate::domain::run_container::MATERIALS_DIR)),
    /// which [`VerificationContainers::publish_materials`] makes and the
    /// container mounts.
    pub materials: PathBuf,
    /// The directory in the container the verification's build output
    /// volume is mounted at; `None` mounts none.
    pub build_output: Option<String>,
    pub limits: ContainerLimits,
}

/// The containers of the verifications of `integrate` set to the container
/// (ADR-t2113-3 decision 1, ADR-t2114-4 decision 4, ADR-t2173-1 decisions
/// 1 and 4, ADR-t2179-1 decisions 7 and 8): what makes a verification's
/// parts ready, starts its commands' containers and removes what it left.
pub trait VerificationContainers: Send + Sync {
    /// Make the parts of a verification of `run`'s commit `commit` ready:
    /// the dagq machine, the image, the run's relay and the verification's
    /// own build output volume (when `dagq.toml` names its directory); a
    /// new directory of its own under the queue's verification directory,
    /// holding a clone of `commit` the control side makes with its git
    /// guarded, a copy of the verification's token `token` and an empty
    /// directory for the materials. A path of the verification's
    /// directory that is there already, or a parent that is a symlink or no
    /// directory, is refused. `Err` says why no command runs; nothing runs
    /// on the host instead.
    fn prepare(
        &self,
        run: &TaskRun,
        commit: &CommitSha,
        token: &Path,
    ) -> Result<PreparedVerification>;
    /// Copy the staged materials of `verification` into the run
    /// directory's materials, made anew: through the run directory's
    /// descriptors, removing what is there first and following no symlink
    /// the worker put there, so no file outside the run directory is
    /// written.
    fn publish_materials(&self, verification: &PreparedVerification) -> Result<()>;
    /// The shell's command line of `podman --connection dagq` with `args`.
    fn command_line(&self, args: &[String]) -> String;
    /// Stop and remove the container of `verification`, if it is there.
    fn remove_container(&self, verification: &PreparedVerification) -> Result<()>;
    /// Remove what `verification` left: its container, its build output
    /// volume and its directory, without git and following no symlink.
    fn finish(&self, verification: &PreparedVerification) -> Result<()>;
    /// Remove, as [`Self::finish`] does, what the verifications of `run`
    /// an earlier process made left (its supervisor died while they ran):
    /// those whose directory is still in the queue's verification
    /// directory. The IDs of those removed.
    fn remove_left(&self, run: &RunId) -> Result<Vec<String>>;
}

/// The verification containers of a queue that has none: every verification
/// set to the container is refused.
pub struct NoVerificationContainers;

impl VerificationContainers for NoVerificationContainers {
    fn prepare(&self, _: &TaskRun, _: &CommitSha, _: &Path) -> Result<PreparedVerification> {
        bail!("this process runs no verification in a container")
    }
    fn publish_materials(&self, _: &PreparedVerification) -> Result<()> {
        bail!("this process runs no verification in a container")
    }
    fn command_line(&self, _: &[String]) -> String {
        String::new()
    }
    fn remove_container(&self, _: &PreparedVerification) -> Result<()> {
        Ok(())
    }
    fn finish(&self, _: &PreparedVerification) -> Result<()> {
        Ok(())
    }
    fn remove_left(&self, _: &RunId) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// The shell's command line that runs `command` of `run` with `env` in the
/// container of `verification`: `dagq relay verify` in a container
/// [`verification_run_args`](crate::domain::run_container::verification_run_args)
/// hardens, its environment naming the materials, the run and the
/// verification's home besides `env`. What `integrate`'s verification and
/// the landing recheck of a run in a container run through `podman` on
/// the host.
pub fn verification_line(
    containers: &dyn VerificationContainers,
    verification: &PreparedVerification,
    run: &RunId,
    command: &str,
    env: &[(String, String)],
) -> Result<String> {
    use crate::domain::run_container::{
        MATERIALS_ENV, VerificationContainer, verification_command, verification_run_args,
    };
    let mut env = verification_env(env);
    env.extend([
        (
            MATERIALS_ENV.to_owned(),
            super::path_text(&verification.materials)?,
        ),
        (
            crate::domain::actor::RUN_ID_ENV.to_owned(),
            run.as_str().to_owned(),
        ),
        (
            "HOME".to_owned(),
            super::path_text(&verification.dir.join(HOME_DIR))?,
        ),
    ]);
    let command = verification_command(&verification.dir, command);
    let args = verification_run_args(&VerificationContainer {
        queue: &verification.queue,
        run: run.as_str(),
        verification: &verification.id,
        image: &verification.image,
        dir: &verification.dir,
        materials: &verification.materials,
        build_output: verification.build_output.as_deref(),
        limits: verification.limits,
        env: &env,
        command: &command,
    })
    .map_err(|why| anyhow!("the verification's container cannot be made: {why}"))?;
    Ok(containers.command_line(&args))
}

/// `env` without what names a worker's local checks' directory: a
/// verification's results are the runtime's own record, and it is never
/// told where the worker's are (ADR-t2589-1 decision 2).
fn verification_env(env: &[(String, String)]) -> Vec<(String, String)> {
    env.iter()
        .filter(|(key, _)| key != local_checks::DIR_ENV)
        .cloned()
        .collect()
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
        env.push(super::actor_executor::local_checks_env(run)?);
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
/// which the executor sets itself, nor Claude Code's configuration
/// directory, which stays the home's for the control side to find the
/// sessions there ([`crate::domain::run_container::CLAUDE_DIR`]).
fn turn_variables(run_env: &[(String, String)], agent: &CommandSpec) -> Vec<(String, String)> {
    let reserved = |key: &str| {
        WITHOUT_ENV.contains(&key)
            || ACTOR_ENV.contains(&key)
            || CLIENT_ENV.contains(&key)
            || key == "HOME"
            || key == "CLAUDE_CONFIG_DIR"
            || key == local_checks::DIR_ENV
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
    fn a_verification_is_not_told_the_local_checks_directory() {
        let env = vec![
            ("CARGO_TARGET_DIR".to_owned(), "/build".to_owned()),
            (
                "DAGQ_LOCAL_CHECKS_DIR".to_owned(),
                "/q/runs/r1/local-checks".to_owned(),
            ),
        ];
        assert_eq!(verification_env(&env), env[..1]);
    }

    #[test]
    fn a_turn_s_variables_leave_the_host_s_compiler_wrapper_and_the_actor_s_names_out() {
        let run_env = vec![
            ("RUSTC_WRAPPER".to_owned(), "sccache".to_owned()),
            ("CARGO_TARGET_DIR".to_owned(), "/build".to_owned()),
            ("DAGQ_RUN_ID".to_owned(), "other".to_owned()),
            ("DAGQ_SERVICE_SOCKET".to_owned(), "/x".to_owned()),
            ("DAGQ_LOCAL_CHECKS_DIR".to_owned(), "/elsewhere".to_owned()),
            ("HOME".to_owned(), "/Users/me".to_owned()),
            (
                "CLAUDE_CONFIG_DIR".to_owned(),
                "/Users/me/.claude".to_owned(),
            ),
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
