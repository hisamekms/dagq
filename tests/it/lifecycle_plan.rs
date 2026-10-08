//! The planners against fakes of the session wrappers (and of cmux only for
//! `up`): the prompt of the inbox, the
//! sessions the runtime's planners open and record, the planner the
//! runtime opens for a proposal, how a planner session is judged, and the
//! sweeps of planners that ended (a person's planner opened before
//! `dagq plan` was abolished included, ADR-t1394-1).

use crate::common;

use common::lifecycle::*;

use anyhow::{Result, bail};
use dagq::{
    application::{
        SessionWrappers, TaskStore,
        planner::{self, OpenedPlanner, PlannerLaunch},
    },
    domain::{
        NewTask, PlannerId, PlannerOrigin, SessionRole, TaskRun,
        actor_model::ModelRole,
        background_wrapper::{StopRoute, WrapperStop},
    },
    infrastructure::{
        adapters::{GitRepository, SystemProcesses, shell_quote},
        clock::SystemClock,
        run_files::LocalRunFiles,
        sqlite::SqliteQueue,
    },
    lifecycle::{QUEUE_ENV, ROLE_ENV, inbox_command},
    runtime::inbox_prompt,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

#[test]
fn the_inbox_prompt_names_the_queue_and_its_one_job() {
    let db = Path::new("/data/q/queue.db");
    let inbox = inbox_prompt(db).unwrap();
    assert!(inbox.starts_with("You are the inbox of the dagq queue at /data/q/queue.db:"));
    assert!(inbox.lines().count() <= 5, "{inbox}");
    assert!(inbox.contains("never decide anything yourself"));
    assert!(inbox.contains("Start with `dagq status --role inbox`"));
    assert!(inbox.contains("dagq-inbox skill"));
    assert!(inbox.contains("`dagq watch --role inbox --after <cursor>` in the background"));
    assert!(inbox.contains("watch again from the cursor it returns"));
    assert!(inbox.contains("On ask_opened, read the ask with `dagq asks --open --role inbox`"));
    assert!(inbox.contains("show the person its question and options"));
    assert!(inbox.contains("AskUserQuestion"));
    assert!(inbox.contains("`dagq answer ID --text '<answer>'`"));
    // Every attention is the inbox's now (ADR-0024 decision 6).
    assert!(inbox.contains("a stopped supervisor"), "{inbox}");
    assert!(inbox.contains("an answered ask"), "{inbox}");
    // The inbox relays a stuck_exit ask like any other; it acts on nothing.
    assert!(!inbox.contains("stuck_exit"));
    assert!(!inbox.contains("/exit"));
    assert!(inbox.contains("Never open the queue database directly"));

    // The inbox's command writes its settings in the queue's directory.
    let temp = tempfile::tempdir().unwrap();
    let db = &temp.path().join("queue.db");
    let command = inbox_command(db, Path::new("/opt/claude"), Some(Path::new("/p")), None).unwrap();
    assert!(command.starts_with("'/opt/claude' '"), "{command}");
    assert!(command.contains("'--' 'You are the inbox of"), "{command}");
    // A person works in the inbox: its settings are only the denials
    // (ADR-t1228-2 decision 3), so Claude Code's prompt suggestions stay on
    // (goal 48).
    let settings = db.parent().unwrap().join("claude-inbox-settings.json");
    assert!(
        command.contains(&format!("'--settings' {}", common::shell_path(&settings))),
        "{command}"
    );
    let written = fs::read_to_string(&settings).unwrap();
    assert!(written.contains("Bash(cmux:*)"), "{written}");
    assert!(!written.contains("promptSuggestion"), "{written}");
    assert!(!written.contains("hooks"), "{written}");
    assert_eq!(ROLE_ENV, "DAGQ_ROLE");
    assert_eq!(QUEUE_ENV, "DAGQ_QUEUE");
    assert!(
        inbox_command(db, Path::new("/opt/claude"), Some(Path::new("/p")), None)
            .unwrap()
            .contains("'--plugin-dir' '/p'")
    );
}

/// A stand-in for this binary, which a planner's wrapper runs.
fn runner(fixture: &Fixture) -> PathBuf {
    let runner = fixture._dir.path().join("dagq-binary");
    fs::write(&runner, "#!/bin/sh\n").unwrap();
    runner
}

/// A planner an older binary opened in a cmux workspace, as its row and
/// directory were left: a person's opened before `dagq plan` was abolished
/// (ADR-t1394-1), or one of the runtime's opened before the interactive
/// route was retired (ADR-t1433-2). The runtime opens none any more, but
/// still sweeps and closes such rows.
fn workspace_planner(fixture: &Fixture, origin: PlannerOrigin) -> Result<OpenedPlanner> {
    let queue = SqliteQueue::open(&fixture.location.db)?;
    let planner = queue.open_planner(origin, None)?;
    let dir = planners_dir(fixture).join(planner.id.to_string());
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("prompt.txt"),
        "You are a planner of the queue these tests open.\n",
    )?;
    fs::copy(runner(fixture), dir.join("runner"))?;
    let workspace = format!("01234567-89ab-4def-8123-{:012x}", planner.id.as_i64());
    queue.planner_workspace_created(planner.id, &workspace)?;
    Ok(OpenedPlanner {
        planner: queue.planner(planner.id)?,
        dir,
        launch: dagq::domain::actor_model::ActorLaunch::default_of(match origin {
            PlannerOrigin::Person => ModelRole::Planner,
            PlannerOrigin::Runtime => ModelRole::RuntimePlanner,
        }),
    })
}

/// One wrapper started: its working directory, its command line, its
/// environment and its log.
type Launch = (PathBuf, String, Vec<(String, String)>, PathBuf);

/// Starts a planner's session wrapper in the background the way the
/// runtime does, and records each start, stop and liveness question: the
/// session wrappers of a planner, with no cmux (ADR-t1433-2 decision 3).
/// `exists` says whether a wrapper it started was not stopped.
#[derive(Default)]
struct Background {
    launched: Mutex<Vec<Launch>>,
    stopped: Mutex<Vec<String>>,
    /// The handles `exists` was asked about, in order.
    asked: Mutex<Vec<String>>,
    /// Starting a wrapper fails.
    fails: bool,
}

impl Background {
    fn handle(index: usize) -> String {
        format!("background:{}:start", 1000 + index)
    }
}

impl SessionWrappers for Background {
    fn launch_background(
        &self,
        cwd: &Path,
        command: &str,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<String> {
        if self.fails {
            bail!("the wrapper could not start");
        }
        let mut launched = self.launched.lock().unwrap();
        let handle = Self::handle(launched.len());
        launched.push((cwd.into(), command.into(), env.to_vec(), log.into()));
        Ok(handle)
    }
    fn stop_background(&self, handle: &str, _: StopRoute) -> Result<Option<WrapperStop>> {
        self.stopped.lock().unwrap().push(handle.to_owned());
        Ok(None)
    }
    fn exists(&self, handle: &str) -> Result<bool> {
        self.asked.lock().unwrap().push(handle.to_owned());
        let started =
            (0..self.launched.lock().unwrap().len()).any(|index| Self::handle(index) == handle);
        Ok(started && !self.stopped.lock().unwrap().iter().any(|id| id == handle))
    }
}

/// What opening a planner of the runtime's works with in these tests: the
/// fixture's Claude Code stub and plugin directory and a stand-in for this
/// binary, with `language`.
fn launch<'a>(
    fixture: &'a Fixture,
    queue: &'a SqliteQueue,
    backend: &'a Background,
    paths: &'a (PathBuf, PathBuf, PathBuf, PathBuf, Option<PathBuf>),
    language: Option<dagq::domain::language::Language>,
) -> PlannerLaunch<'a> {
    let (db, planners, root, runner, plugin_dir) = paths;
    PlannerLaunch {
        queue,
        backend,
        files: &LocalRunFiles,
        db,
        planners_dir: planners,
        repo_root: root,
        runner,
        claude: &fixture.options.claude,
        plugin_dir: plugin_dir.as_deref(),
        language,
        roles: Default::default(),
        turn_limits: dagq::domain::stall::StallConfig::default().turn_limits(),
    }
}

/// The paths [`launch`] borrows: the queue, `planners/`, the checkout,
/// the runner and the plugin directory (with `plugin`).
fn paths(fixture: &Fixture, plugin: bool) -> (PathBuf, PathBuf, PathBuf, PathBuf, Option<PathBuf>) {
    (
        fixture.location.db.canonicalize().unwrap(),
        planners_dir(fixture),
        GitRepository::inspect(&fixture.repo).unwrap().root,
        runner(fixture),
        fixture
            .options
            .plugin_dir
            .as_ref()
            .filter(|_| plugin)
            .map(|dir| dir.canonicalize().unwrap()),
    )
}

/// A proposal of one task a person's planner (closed since) submitted.
fn proposal(
    queue: &mut SqliteQueue,
    title: &str,
) -> (dagq::domain::ProposalId, dagq::domain::Task) {
    use dagq::domain::{PlannerOwner, Submission};
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: vec![],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
        })
        .unwrap();
    let proposal = queue
        .submit(Submission {
            tasks: vec![task.id()],
            goals: vec![],
            proposal: None,
            owner: PlannerOwner {
                origin: PlannerOrigin::Person,
                workspace_id: Some("closed-planner".into()),
            },
        })
        .unwrap();
    (proposal.id(), queue.show(task.id()).unwrap().task)
}

/// What the supervisor's sweep does of the planners that ended
/// ([`planner::close_abandoned_planners`],
/// [`planner::remove_unused_planner_runners`]), on the real processes: the
/// IDs whose record it closed, or the first error.
fn sweep(fixture: &Fixture, sessions: &dyn SessionWrappers) -> Result<Vec<PlannerId>> {
    let queue = SqliteQueue::open(&fixture.location.db)?;
    let closed =
        planner::close_abandoned_planners(&queue, sessions, &SystemProcesses, &SystemClock);
    planner::remove_unused_planner_runners(
        &queue,
        &SystemProcesses,
        &LocalRunFiles,
        &SystemClock,
        &planners_dir(fixture),
    )?;
    closed
}

fn planners_dir(fixture: &Fixture) -> PathBuf {
    fixture
        .location
        .db
        .canonicalize()
        .unwrap()
        .parent()
        .unwrap()
        .join("planners")
}

/// Each planner the runtime opens runs headless in the background
/// (ADR-t1394-2 decision 2, ADR-t1433-2 decision 3): no workspace is
/// opened, colored or grouped; its wrapper `planner-session --headless
/// --background` starts in the checkout with the planner's role, queue,
/// origin and ID in its environment and its output in `session.log` of its
/// directory, which holds its prompt, its turn limits and the wrapper
/// binary; its row records the route `headless` and the wrapper's handle.
/// `up` opens none. A wrapper that does not start leaves a closed record
/// with the error.
#[test]
fn each_runtime_planner_starts_its_wrapper_in_the_background_without_a_workspace() {
    use dagq::application::planner::open_runtime_planner;
    use dagq::domain::PlannerRoute;
    let fixture = fixture();
    let backend = Background::default();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let (first_proposal, first_task) = proposal(&mut queue, "first");
    let (second_proposal, second_task) = proposal(&mut queue, "second");
    let paths = paths(&fixture, true);
    let (db, _, root, _, plugin_dir) = &paths;
    let launch = launch(&fixture, &queue, &backend, &paths, None);
    let reasons = vec!["split it".to_owned()];
    let first = open_runtime_planner(
        &launch,
        first_proposal,
        &[first_task],
        &reasons,
        Default::default(),
    )
    .unwrap();
    let second = open_runtime_planner(
        &launch,
        second_proposal,
        &[second_task],
        &reasons,
        Default::default(),
    )
    .unwrap();
    let launched = backend.launched.lock().unwrap().clone();
    assert_eq!(launched.len(), 2);
    for (index, (opened, id)) in [(&first, 1), (&second, 2)].into_iter().enumerate() {
        assert_eq!(opened.planner.id, PlannerId::new(id), "{opened:?}");
        assert_eq!(opened.planner.origin, PlannerOrigin::Runtime);
        assert_eq!(opened.planner.route, PlannerRoute::Headless);
        assert_eq!(
            opened.planner.workspace_id.as_deref(),
            Some(Background::handle(index).as_str())
        );
        let dir = planners_dir(&fixture).join(id.to_string());
        assert_eq!(opened.dir, dir);
        assert!(dir.join("prompt.txt").is_file());
        assert!(dir.join("turns").join("limits.json").is_file());
        assert!(dir.join("runner").is_file());
        let (cwd, command, env, log) = &launched[index];
        assert_eq!(cwd, root);
        assert_eq!(log, &dir.join("session.log"));
        let quoted = |path: &Path| shell_quote(path.to_str().unwrap());
        assert_eq!(
            command,
            &format!(
                "{} '--db' {} 'planner-session' '--planner' '{id}' '--claude' {} '--plugin-dir' {} '--model' 'claude-opus-5-5' '--effort' 'high' '--headless' '--background'",
                quoted(&dir.join("runner")),
                quoted(db),
                quoted(&fixture.options.claude),
                quoted(plugin_dir.as_deref().unwrap()),
            )
        );
        for (name, value) in [
            ("DAGQ_ROLE", "planner".to_owned()),
            ("DAGQ_QUEUE", db.to_str().unwrap().to_owned()),
            // One actor per planner (ADR-t728-1 decision 4).
            ("DAGQ_ACTOR_ID", format!("planner:{id}")),
            ("DAGQ_SESSION_KIND", "runtime_planner".to_owned()),
            ("DAGQ_PLANNER_ORIGIN", "runtime".to_owned()),
            ("DAGQ_PLANNER_ID", id.to_string()),
        ] {
            assert!(
                env.iter().any(|(k, v)| k == name && *v == value),
                "{name}: {env:?}"
            );
        }
    }
    // Nothing of cmux's: no workspace, look, group or screen.
    assert!(backend.stopped.lock().unwrap().is_empty());
    let recorded: Vec<Option<String>> = queue
        .planners(false)
        .unwrap()
        .into_iter()
        .map(|planner| planner.workspace_id)
        .collect();
    assert_eq!(
        recorded,
        [Some(Background::handle(0)), Some(Background::handle(1))]
    );
    // No planner is a session workspace of `up`'s.
    assert_eq!(queue.session_workspace(SessionRole::Planner).unwrap(), None);
    // `up` opens the inbox and leaves the planners alone.
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let report = up(&fixture, &cmux, &launchd, &FakeProcesses::default());
    assert_eq!(report.get("planner"), None, "{report}");
    assert_eq!(queue.planners(false).unwrap().len(), 2);

    // A wrapper that does not start leaves a closed record with the error.
    let failing = Background {
        fails: true,
        ..Background::default()
    };
    let (third_proposal, third_task) = proposal(&mut queue, "third");
    let launch = self::launch(&fixture, &queue, &failing, &paths, None);
    let error = open_runtime_planner(
        &launch,
        third_proposal,
        &[third_task],
        &reasons,
        Default::default(),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("the wrapper could not start"),
        "{error:#}"
    );
    let all = queue.planners(true).unwrap();
    assert_eq!(all.len(), 3);
    assert!(all[2].closed_at.is_some());
    assert!(
        all[2]
            .error
            .as_deref()
            .unwrap()
            .contains("the wrapper could not start"),
        "{:?}",
        all[2].error
    );
}

/// The runtime opens a planner for a proposal plan review sent back
/// (ADR-0041 decision 12): the proposal's planner is recorded as the
/// runtime's, and its first turn's prompt carries the proposal, its tasks
/// and the reasons. A proposal that does not exist opens nothing.
#[test]
fn the_runtime_opens_a_planner_for_a_proposal_with_its_reasons() {
    use dagq::application::planner::open_runtime_planner;
    use dagq::domain::ProposalId;
    let fixture = fixture();
    let backend = Background::default();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let (proposal, task) = self::proposal(&mut queue, "planned change");
    let paths = paths(&fixture, false);
    let db = paths.0.clone();
    // The language resolved for the launch (ADR-t616-2).
    let launch = launch(
        &fixture,
        &queue,
        &backend,
        &paths,
        Some(dagq::domain::language::Language {
            tag: "ja".into(),
            source: dagq::domain::language::LanguageSource::User,
        }),
    );
    let reasons = vec!["the acceptance is not testable".to_owned()];
    let opened = open_runtime_planner(
        &launch,
        proposal,
        std::slice::from_ref(&task),
        &reasons,
        Default::default(),
    )
    .unwrap();
    assert_eq!(opened.planner.origin, PlannerOrigin::Runtime);
    assert_eq!(opened.planner.proposal_id, Some(proposal));
    let prompt = fs::read_to_string(opened.dir.join("prompt.txt")).unwrap();
    // What it took, the language's instruction included (task 1571).
    crate::runtime_support::planner_prompt_bytes::assert_planner_prompt_bytes(
        &db,
        opened.planner.id,
        "runtime",
        dagq::application::prompt::RUNTIME_PLANNER_PROMPT_LIMIT,
    );
    assert!(
        prompt.starts_with(&format!(
            "You are a planner the dagq runtime opened for proposal {proposal}"
        )),
        "{prompt}"
    );
    assert!(
        prompt.contains("- the acceptance is not testable"),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("- task {} (submitted): planned change", task.id())),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("`dagq submit --proposal {proposal}`")),
        "{prompt}"
    );
    assert!(
        prompt.ends_with(&dagq::domain::language::instruction("ja")),
        "{prompt}"
    );
    let launched = backend.launched.lock().unwrap();
    let env = &launched[0].2;
    assert!(
        env.contains(&("DAGQ_PLANNER_ORIGIN".into(), "runtime".into())),
        "{env:?}"
    );
    // Its span is the runtime planner's, never the person's planner's.
    assert!(
        env.contains(&("DAGQ_SESSION_KIND".into(), "runtime_planner".into())),
        "{env:?}"
    );
    assert!(!env.contains(&("DAGQ_SESSION_KIND".into(), "planner".into())));
    // Opened for a revise: one effort step above the default, and why
    // (ADR-0079 decision 7 (c)).
    assert_eq!(opened.launch.effort.as_deref(), Some("high"));
    assert_eq!(opened.launch.escalated_from.as_deref(), Some("medium"));
    let recorded = env
        .iter()
        .find(|(key, _)| key == "DAGQ_LAUNCH")
        .map(|(_, value)| serde_json::from_str::<Value>(value).unwrap())
        .unwrap();
    assert_eq!(
        recorded,
        json!({"role": "runtime_planner", "provider": "claude", "model": "claude-opus-5-5", "effort": "high",
               "source": "revise_escalation", "escalated_from": "medium",
               "escalation_reason": "plan_review_revise"})
    );
    let command = &launched[0].1;
    assert!(!command.contains("--plugin-dir"), "{command}");
    assert!(
        command
            .ends_with("'--model' 'claude-opus-5-5' '--effort' 'high' '--headless' '--background'"),
        "{command}"
    );
    drop(launched);
    assert_eq!(
        queue.planner(opened.planner.id).unwrap().proposal_id,
        Some(proposal)
    );
    // Nothing to fix and no reasons still makes a prompt that says so.
    let empty = dagq::application::prompt::runtime_planner_prompt(
        &db,
        proposal,
        &[],
        &[],
        None,
        Default::default(),
    )
    .unwrap()
    .text;
    assert!(
        empty.contains("(none given)") && empty.contains("(none)"),
        "{empty}"
    );
    // It names the CLI that reads the record (ADR-0044 decision 22).
    for expected in ["`dagq events --full --task ID`", "`dagq timeline RUN`"] {
        assert!(empty.contains(expected), "{expected}\n{empty}");
    }

    let missing = open_runtime_planner(
        &launch,
        ProposalId::new(99),
        &[],
        &reasons,
        Default::default(),
    );
    assert!(missing.is_err());
    assert_eq!(queue.planners(true).unwrap().len(), 1);
}

/// Stands in for Claude Code in a planner's workspace: it goes idle the
/// way the `Stop` hook marks it, then exits with `code`.
struct PlannerAgent {
    code: i32,
}

impl dagq::application::AgentProvider for PlannerAgent {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn review_command(
        &self,
        _: &TaskRun,
        _: &str,
        _: dagq::domain::headless_job::JobAccess,
    ) -> Result<dagq::application::CommandSpec> {
        bail!("not a run")
    }
    fn wait_interval(&self) -> Duration {
        Duration::from_millis(20)
    }
    fn select_model(
        &self,
        command: &mut dagq::application::CommandSpec,
        model: &str,
        effort: &str,
    ) {
        command.env("PLANNER_MODEL", format!("{model} {effort}"));
    }
    fn planner_command(
        &self,
        planner: &dagq::application::PlannerCommand<'_>,
    ) -> Result<dagq::application::CommandSpec> {
        assert!(planner.prompt.starts_with("You are a planner of"));
        assert_eq!(planner.plugin_dir, Some(Path::new("/plugins")));
        let marker = planner.idle_marker();
        let mut command = dagq::application::CommandSpec::new("/bin/sh");
        command.current_dir(planner.cwd).arg("-c").arg(format!(
            "printf '%s' \"$PLANNER_MODEL\" > {model}; sleep 0.2; printf '{{\"hook_event_name\":\"Stop\"}}' > {marker}; exit {code}",
            model = shell_quote(planner.dir.join("model.txt").to_str().unwrap()),
            marker = shell_quote(marker.to_str().unwrap()),
            code = self.code,
        ));
        Ok(command)
    }
}

/// A provider without planner sessions (the default refusal).
struct NoPlanner;

impl dagq::application::AgentProvider for NoPlanner {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn review_command(
        &self,
        _: &TaskRun,
        _: &str,
        _: dagq::domain::headless_job::JobAccess,
    ) -> Result<dagq::application::CommandSpec> {
        bail!("not a run")
    }
}

/// A person's planner's session wrapper, in the terminal of the workspace
/// it was opened in before `dagq plan` was abolished, registers itself and
/// its agent, heartbeats and records the agent's exit, and tells the
/// terminal nothing more (the grace and its notice went with ADR-t1433-2
/// decision 5). Its row, in a cmux workspace, reads `closed` whatever its
/// wrapper does, without a call to cmux: the runtime looks up no planner's
/// workspace.
#[test]
fn a_persons_planners_wrapper_records_its_agent_and_its_row_reads_closed_without_cmux() {
    use dagq::application::planner::{PlannerProbes, planner_views};
    use dagq::domain::PlannerState;
    let fixture = fixture();
    let sessions = Background::default();
    let opened = workspace_planner(&fixture, PlannerOrigin::Person).unwrap();
    let id = opened.planner.id;
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let processes = FakeProcesses::default();
    let signals = dagq::infrastructure::adapters::ClaudeCode {
        executable: "claude".into(),
    };
    let planners = planners_dir(&fixture);
    let probes = PlannerProbes {
        sessions: &sessions,
        processes: &processes,
        files: &LocalRunFiles,
        signals: &signals,
        clock: &SystemClock,
        planners_dir: &planners,
    };
    let state = |all: bool| -> Vec<(PlannerState, bool, bool)> {
        planner_views(&queue, &probes, all)
            .unwrap()
            .into_iter()
            .map(|view| (view.state, view.alive, view.idle_since.is_some()))
            .collect()
    };
    assert_eq!(state(false), [(PlannerState::Closed, false, false)]);

    // The wrapper runs the agent, which goes idle and exits.
    assert!(planners.join("1/runner").is_file());
    let db = fixture.location.db.canonicalize().unwrap();
    let result = dagq::compose::planner_session_with_provider(
        &db,
        id,
        &PlannerAgent { code: 3 },
        Some(Path::new("/plugins")),
        Some(("claude-opus-5-5", "high")),
    )
    .unwrap();
    // The wrapper gave its agent the model and effort (ADR-0079 decision 7).
    assert_eq!(
        fs::read_to_string(planners.join("1/model.txt")).unwrap(),
        "claude-opus-5-5 high"
    );
    // No notice of a close: the runtime does not close its workspace.
    assert_eq!(result, json!({"planner_id": 1, "exit_code": 3}));
    let planner = queue.planner(id).unwrap();
    assert_eq!(planner.wrapper_pid, Some(std::process::id()));
    assert!(planner.agent_pid.is_some());
    assert_eq!(planner.exit_code, Some(3));
    assert!(planner.heartbeat_at.is_some());
    assert!(planners.join("1/idle.json").is_file());
    assert_eq!(state(false), [(PlannerState::Closed, false, false)]);
    // Task 696: its exit recorded, the wrapper removed its runner and left
    // the rest of the directory.
    assert!(!planners.join("1/runner").exists());
    assert!(planners.join("1/prompt.txt").is_file());
    // An agent that cannot start is recorded as an exit of 127.
    let third_id = workspace_planner(&fixture, PlannerOrigin::Person)
        .unwrap()
        .planner
        .id;
    let error = dagq::compose::planner_session_with_provider(&db, third_id, &NoPlanner, None, None)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("no planner session"),
        "{error:#}"
    );
    assert_eq!(queue.planner(third_id).unwrap().exit_code, Some(127));
    assert!(!planners.join(format!("{third_id}/runner")).exists());
    queue.close_planner(third_id, None).unwrap();
    // One session per planner.
    assert!(
        dagq::compose::planner_session_with_provider(
            &db,
            id,
            &PlannerAgent { code: 0 },
            None,
            None,
        )
        .is_err()
    );

    // A live, idle one in a workspace reads `closed` too.
    let second = workspace_planner(&fixture, PlannerOrigin::Person).unwrap();
    let second_id = second.planner.id;
    queue.register_planner_wrapper(second_id, 4242).unwrap();
    queue.register_planner_agent(second_id, 4242, 4242).unwrap();
    queue.heartbeat_planner(second_id, 4242).unwrap();
    fs::write(
        planners.join(format!("{second_id}/idle.json")),
        r#"{"hook_event_name":"Stop","background_tasks":[]}"#,
    )
    .unwrap();
    assert_eq!(state(false)[1], (PlannerState::Closed, false, false));
    queue.close_planner(second_id, None).unwrap();
    assert_eq!(state(false).len(), 1);
    assert_eq!(state(true).len(), 3);

    // `planners` reads the same through the real processes.
    let listed = dagq::lifecycle::planners(&fixture.location.db, &sessions, true).unwrap();
    let states: Vec<&str> = listed["planners"]
        .as_array()
        .unwrap()
        .iter()
        .map(|planner| planner["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["closed", "closed", "closed"], "{listed}");
    assert_eq!(listed["planners"][0]["alive"], false);
    assert_eq!(
        listed["planners"][0]["workspace_id"],
        json!(opened.planner.workspace_id)
    );
    // Judging them asked nothing of the session wrappers: a row in a
    // workspace is not a wrapper's.
    assert!(sessions.asked.lock().unwrap().is_empty());
    assert!(sessions.stopped.lock().unwrap().is_empty());
}

/// A pid no process has: a child that exited and was reaped.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Goal 54 (1): the sweep closes the record of a planner of the runtime's
/// an older binary opened in a cmux workspace whose wrapper is dead or
/// exited, so `planners` stops showing it. Its workspace is not looked up
/// in cmux nor asked of the session wrappers (ADR-t1433-2 decisions 3 and
/// 5): such a row counts as gone. A wrapper
/// still alive, or one not registered within its startup time yet, keeps
/// its row; a person's planner's row is left to `close_person_planners`.
#[test]
fn the_records_of_planners_whose_wrapper_is_gone_are_closed_without_listing_cmux() {
    let fixture = fixture();
    let sessions = Background::default();
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let open = |origin: PlannerOrigin| {
        let opened = workspace_planner(&fixture, origin).unwrap();
        (opened.planner.id, opened.planner.workspace_id.unwrap())
    };
    let (dead, _) = open(PlannerOrigin::Runtime);
    let (exited, _) = open(PlannerOrigin::Runtime);
    let (alive, _) = open(PlannerOrigin::Runtime);
    let (listed, _) = open(PlannerOrigin::Runtime);
    let (person, _) = open(PlannerOrigin::Person);
    queue.register_planner_wrapper(person, dead_pid()).unwrap();
    queue.register_planner_wrapper(dead, dead_pid()).unwrap();
    queue
        .register_planner_wrapper(exited, std::process::id())
        .unwrap();
    queue
        .register_planner_agent(exited, std::process::id(), std::process::id())
        .unwrap();
    queue.planner_exited(exited, std::process::id(), 0).unwrap();
    queue
        .register_planner_wrapper(alive, std::process::id())
        .unwrap();
    queue
        .register_planner_agent(alive, std::process::id(), std::process::id())
        .unwrap();
    queue.register_planner_wrapper(listed, dead_pid()).unwrap();
    let open_ids = || -> Vec<PlannerId> {
        queue
            .planners(false)
            .unwrap()
            .into_iter()
            .map(|planner| planner.id)
            .collect()
    };

    let (fresh, _) = open(PlannerOrigin::Runtime);
    // A row in a workspace is not asked of the session wrappers.
    assert_eq!(sweep(&fixture, &sessions).unwrap(), [dead, exited, listed]);
    assert!(sessions.asked.lock().unwrap().is_empty());
    assert_eq!(open_ids(), [alive, person, fresh]);
    assert!(sweep(&fixture, &sessions).unwrap().is_empty());
    for id in [dead, exited, listed] {
        let planner = queue.planner(id).unwrap();
        assert!(planner.closed_at.is_some());
        assert_eq!(planner.error, None);
    }
    // Each close is recorded once, with why (ADR-t1300-1).
    let mut closes: Vec<(i64, String, String)> = queue
        .latest_events_of("planner_closed", 10)
        .unwrap()
        .into_iter()
        .map(|event| {
            (
                event.payload["planner_id"].as_i64().unwrap(),
                event.payload["origin"].as_str().unwrap().to_owned(),
                event.payload["code"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    closes.sort();
    assert_eq!(
        closes,
        [dead, exited, listed].map(|id| (
            id.as_i64(),
            "runtime".to_owned(),
            "abandoned".to_owned()
        ))
    );
    let shown: Vec<i64> = dagq::lifecycle::planners(&fixture.location.db, &sessions, false)
        .unwrap()["planners"]
        .as_array()
        .unwrap()
        .iter()
        .map(|planner| planner["id"].as_i64().unwrap())
        .collect();
    assert_eq!(shown, [alive, person, fresh].map(PlannerId::as_i64));
    // Task 696: the runners of the planners whose wrapper is done went,
    // their row closed or not; a live wrapper's, and one not registered
    // within its startup time yet, stay with the rest of the directory.
    let dir = |id: PlannerId| planners_dir(&fixture).join(id.to_string());
    for id in [dead, exited, listed] {
        assert!(!dir(id).join("runner").exists(), "planner {id}");
        assert!(dir(id).join("prompt.txt").is_file());
    }
    for id in [alive, fresh] {
        assert!(dir(id).join("runner").is_file(), "planner {id}");
    }
}

/// Task 344: planner IDs start over with a new queue database, so the
/// directory of planner 1 may still hold the idle marker a planner 1 of
/// the old database left. Opening the new planner removes it: its session
/// is `working` until its own agent stops, not `idle` from the old marker.
#[test]
fn a_new_planner_does_not_take_the_idle_marker_an_old_database_left() {
    use dagq::application::planner::{PlannerProbes, open_runtime_planner, planner_views};
    use dagq::domain::PlannerState;
    let fixture = fixture();
    let backend = Background::default();
    let planners = planners_dir(&fixture);
    fs::create_dir_all(planners.join("1")).unwrap();
    fs::write(
        planners.join("1/idle.json"),
        r#"{"hook_event_name":"Stop","background_tasks":[]}"#,
    )
    .unwrap();

    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let (proposal, task) = self::proposal(&mut queue, "planned");
    let paths = paths(&fixture, false);
    let id = open_runtime_planner(
        &launch(&fixture, &queue, &backend, &paths, None),
        proposal,
        &[task],
        &[],
        Default::default(),
    )
    .unwrap()
    .planner
    .id;
    assert_eq!(id, PlannerId::new(1));
    assert!(!planners.join("1/idle.json").exists());
    assert!(planners.join("1/prompt.txt").is_file());

    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    queue.register_planner_wrapper(id, 4242).unwrap();
    queue.register_planner_agent(id, 4242, 4242).unwrap();
    queue.heartbeat_planner(id, 4242).unwrap();
    let processes = FakeProcesses::default();
    let signals = dagq::infrastructure::adapters::ClaudeCode {
        executable: "claude".into(),
    };
    let probes = PlannerProbes {
        sessions: &backend,
        processes: &processes,
        files: &LocalRunFiles,
        signals: &signals,
        clock: &SystemClock,
        planners_dir: &planners,
    };
    let views = planner_views(&queue, &probes, false).unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].state, PlannerState::Working);
    assert_eq!(views[0].idle_since, None);
}

/// A person's planner's wrapper started as an older binary started it, in
/// a workspace, is refused by a closed planner record and by a planner that
/// has a session already, and ends without recording itself or calling
/// cmux: the workspace it runs in is left to whatever opened it.
#[test]
fn a_refused_planner_wrapper_records_nothing_and_closes_no_workspace() {
    let fixture = fixture();
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let planner = queue.open_planner(PlannerOrigin::Person, None).unwrap();
    assert_eq!(planner.id, PlannerId::new(1));
    queue
        .close_planner(planner.id, Some("the workspace create failed"))
        .unwrap();
    let db = fixture.location.db.canonicalize().unwrap();
    let error =
        dagq::compose::planner_session_with_provider(&db, planner.id, &NoPlanner, None, None)
            .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("already has a session or is closed"),
        "{text}"
    );
    assert!(!text.contains("workspace"), "{text}");
    assert_eq!(queue.planner(planner.id).unwrap().wrapper_pid, None);

    // A second wrapper of a planner with a session is refused and leaves
    // the planner's record as it was.
    let opened = workspace_planner(&fixture, PlannerOrigin::Person).unwrap();
    let id = opened.planner.id;
    let workspace = opened.planner.workspace_id.unwrap();
    queue.register_planner_wrapper(id, 4242).unwrap();
    queue.register_planner_agent(id, 4242, 4242).unwrap();
    let error =
        dagq::compose::planner_session_with_provider(&db, id, &NoPlanner, None, None).unwrap_err();
    assert!(
        format!("{error:#}").contains("already has a session or is closed"),
        "{error:#}"
    );
    let planner = queue.planner(id).unwrap();
    assert_eq!(planner.wrapper_pid, Some(4242));
    assert_eq!(planner.workspace_id.as_deref(), Some(workspace.as_str()));
}

/// ADR-t1433-2 decision 5 (amending ADR-t1394-1 decisions 1, 7 and 9): the
/// row of every person's planner still open, alive, exited, lost or never
/// registered, is closed with `planner_closed` once (`person_retired`,
/// `workspace_closed: false`, ADR-t1300-1 decision 2), and cmux is not
/// called: its workspace is neither listed, typed into nor closed. A
/// planner of the runtime's is not closed by this rule.
#[test]
fn every_open_row_of_a_persons_planner_is_closed_without_cmux() {
    let fixture = fixture();
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let open = |origin: PlannerOrigin| {
        let opened = workspace_planner(&fixture, origin).unwrap();
        (opened.planner.id, opened.planner.workspace_id.unwrap())
    };
    let (exited, exited_ws) = open(PlannerOrigin::Person);
    let (lost, _) = open(PlannerOrigin::Person);
    let (alive, _) = open(PlannerOrigin::Person);
    let (unregistered, _) = open(PlannerOrigin::Person);
    let (runtime, _) = open(PlannerOrigin::Runtime);
    let me = std::process::id();
    for id in [exited, alive, runtime] {
        queue.register_planner_wrapper(id, me).unwrap();
        queue.register_planner_agent(id, me, me).unwrap();
    }
    queue.register_planner_wrapper(lost, dead_pid()).unwrap();
    queue.planner_exited(exited, me, 2).unwrap();
    let open_ids = || -> Vec<PlannerId> {
        queue
            .planners(false)
            .unwrap()
            .into_iter()
            .map(|planner| planner.id)
            .collect()
    };

    let closed = planner::close_person_planners(&queue).unwrap();
    assert_eq!(closed, [exited, lost, alive, unregistered]);
    assert_eq!(open_ids(), [runtime]);
    let mut closes: Vec<Value> = queue
        .latest_events_of("planner_closed", 10)
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .collect();
    closes.sort_by_key(|close| close["planner_id"].as_i64());
    assert_eq!(closes.len(), 4, "{closes:?}");
    for (close, id) in closes.iter().zip([exited, lost, alive, unregistered]) {
        assert_eq!(close["planner_id"], id.as_i64());
        assert_eq!(close["origin"], "person");
        assert_eq!(close["code"], "person_retired");
        assert_eq!(close["workspace_closed"], false);
    }
    let close_event = &closes[0];
    assert_eq!(close_event["workspace_id"], exited_ws.as_str());
    assert_eq!(close_event["exit_code"], 2);
    assert!(close_event["exited_at"].is_i64());
    let reason = close_event["reason"].as_str().unwrap();
    assert!(
        reason.contains("closed without cmux")
            && reason.contains(&format!("a person closes its workspace {exited_ws}")),
        "{reason}"
    );

    // Once closed it is not closed again.
    assert!(planner::close_person_planners(&queue).unwrap().is_empty());
    assert_eq!(
        queue.latest_events_of("planner_closed", 10).unwrap().len(),
        4
    );
}
