//! The observer's doubles and reads, shared by the observer's tests and
//! the supervisor's through the queue service.
use super::*;

/// Observe an unbound fixture queue with an injected clock, including the
/// real ledger-backed stats reads. Advancing the clock leaves recorded
/// event times and the rows that already contain them unchanged.
pub fn observe_with_generators(
    db: &Path,
    provider: &dyn AgentProvider,
    options: &dagq::application::observer::ObserveOptions,
    generators: &Generators,
) -> Result<Value> {
    common::service::serve(db);
    common::ledger::build(db);
    let mut queue = SqliteQueue::open(db)?.with_generators(generators.clone());
    ensure!(
        queue.repository_binding()?.is_none(),
        "an unbound fixture queue"
    );
    let events = SqliteQueue::open(db)?.with_generators(generators.clone());
    let sources = ObserverReads(dagq::compose::OneShot::new(generators.clone()));
    let telemetry = dagq::infrastructure::queue_service::RequestedTelemetryTokens::of_caller(db);
    dagq::application::observer::observe(
        &mut queue,
        db,
        provider,
        Some(&dagq::infrastructure::adapters::ClaudeCode {
            executable: "/unused".into(),
        }),
        options,
        &dagq::application::observer::ObserverEnvironment {
            sources: &sources,
            host: &dagq::infrastructure::observer::LocalObserver {
                events: Some(&events),
            },
            generators,
            pid: std::process::id(),
            telemetry: Some(dagq::application::telemetry_tokens::TargetTelemetry {
                tokens: &telemetry,
                db,
                lease: None,
            }),
            steps: Some(dagq::application::step_report::CommandSteps {
                files: &dagq::infrastructure::run_files::LocalRunFiles,
                sinks: Arc::new(dagq::infrastructure::queue_service::SystemServiceAccess),
            }),
        },
    )
}

struct ObserverReads(dagq::compose::OneShot);

impl dagq::application::observer::ObserverSources<SqliteQueue> for ObserverReads {
    fn stats(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        query: &dagq::domain::stats::StatsQuery,
    ) -> Result<Value> {
        self.0.stats_of(queue, db, query)
    }

    fn kpi(&self, queue: &SqliteQueue, db: &Path) -> Result<Value> {
        self.0.observer_kpi(queue, db)
    }

    fn improvements(&self, queue: &SqliteQueue) -> Result<Value> {
        self.0.improvements_of(queue)
    }

    fn checkout(&self, _: &SqliteQueue) -> Result<Option<PathBuf>> {
        Ok(None)
    }
}

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
