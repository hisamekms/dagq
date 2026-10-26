//! The observer's doubles and reads, shared by the observer's tests and
//! the supervisor's through the queue service.
use super::*;

/// The observer's provider double: the headless job is a shell script in the
/// observation's directory, with the environment `observe` gives the agent.
pub struct ObserverProvider {
    pub(crate) script: String,
}

impl ObserverProvider {
    pub fn new(script: &str) -> Self {
        Self {
            script: script.into(),
        }
    }
}

impl AgentProvider for ObserverProvider {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn headless_command(&self, cwd: &Path, prompt: &str, access: JobAccess) -> Result<CommandSpec> {
        assert!(prompt.contains("You are the observer"), "{prompt}");
        assert_eq!(access, JobAccess::QueueCli);
        let mut command = CommandSpec::new("/bin/sh");
        command.current_dir(cwd).arg("-c").arg(&self.script);
        Ok(command)
    }
    /// The script's `$0`, which it writes to `mcp.txt` when asked to.
    fn without_mcp(&self, command: &mut CommandSpec) {
        command.option_args(["no-mcp"]);
    }
    /// `$MODEL`, which the script may write down (ADR-0079 decision 7).
    fn select_model(&self, command: &mut CommandSpec, model: &str, effort: &str) {
        command.env("MODEL", format!("{model} {effort}"));
    }
    fn review_command(&self, _: &TaskRun, _: &Path, _: &str, _: JobAccess) -> Result<CommandSpec> {
        bail!("the observer reviews no run")
    }
}

/// The actor (role, id) and the `requested_by` of each queue event of
/// `kind`, oldest first.
pub fn queue_event_actors(db: &Path, kind: &str) -> Vec<(String, String, Option<String>)> {
    Connection::open(db)
        .unwrap()
        .prepare(
            "SELECT actor_role, actor_id, requested_by
             FROM run_events WHERE kind=?1 ORDER BY id",
        )
        .unwrap()
        .query_map([kind], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// The options of an observation of `mode` with the test binary's `dagq`.
pub fn observe_options(
    mode: dagq::application::observer::ObserveMode,
) -> dagq::application::observer::ObserveOptions {
    dagq::application::observer::ObserveOptions {
        mode,
        since: None,
        dry_run: false,
        timeout: Duration::from_secs(60),
        dagq: PathBuf::from(env!("CARGO_BIN_EXE_dagq")),
        user_config: None,
        prompt_limit: dagq::application::observer::PROMPT_LIMIT,
        launch: None,
        switchable: false,
        fallback: true,
        unavailable: None,
    }
}
