//! The fixture and the doubles of plan review's tests, which the
//! planning tests share: a repository and a queue with a draft blocker,
//! the stub reviewer that prints scripted verdicts, the planners'
//! background wrappers, the supervisor's options and loop, and a
//! completed Spike.

use crate::common;
use crate::runtime_support::background_wrappers::BackgroundWrappers;
use anyhow::{Result, bail};
use dagq::domain::background_wrapper::{StopRoute, WrapperStop};
use dagq::domain::headless_job::JobAccess;
use dagq::infrastructure::adapters::shell_quote;
use dagq::{
    application::{AgentProvider, CommandSpec, SessionWrappers, TaskReads, TaskStore},
    domain::{
        ExecutionClass, GoalId, NewGoal, NewTask, PlannerOrigin, PlannerOwner, Priority,
        ProposalId, Submission, TaskEdit, TaskId, TaskRun, TaskStatus,
    },
    infrastructure::{clock, location::planners_dir, sqlite::SqliteQueue},
    runtime::{self, SuperviseOptions},
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use tempfile::TempDir;

pub(crate) struct Fixture {
    _dir: TempDir,
    pub(crate) repo: PathBuf,
    pub(crate) db: PathBuf,
    pub(crate) claude: PathBuf,
    /// Times the test while held (task 324).
    _test: common::Waiting,
}

/// A repository with one commit on main, a queue next to it, and a draft
/// task (1) every other task depends on, so none is ever claimed.
pub(crate) fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    common::template::repository(&repo, "seed\n");
    let db = dir.path().join("queue").join("queue.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let mut queue = common::template::queue(&db);
    let blocker = add(&mut queue, "blocker", &[], Priority::Normal);
    assert_eq!(blocker, TaskId::new(1));
    let claude = dir.path().join("claude-stub");
    crate::common::template::script(&claude, "#!/bin/sh\nprintf 'stub\\n'\n");

    Fixture {
        _dir: dir,
        _test: common::test(),
        repo,
        db,
        claude,
    }
}

pub(crate) fn add(
    queue: &mut SqliteQueue,
    title: &str,
    deps: &[TaskId],
    priority: Priority,
) -> TaskId {
    add_classed(queue, title, deps, priority, ExecutionClass::Implementation)
}

/// [`add`] with the task's execution class.
pub(crate) fn add_classed(
    queue: &mut SqliteQueue,
    title: &str,
    deps: &[TaskId],
    priority: Priority,
    execution_class: ExecutionClass,
) -> TaskId {
    queue
        .add(NewTask {
            change: None,
            title: title.into(),
            description: format!("{title}: change the type of Foo"),
            acceptance: format!("{title} works; tests/cli.rs is not changed"),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Some(priority),
            dependencies: deps.to_vec(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            // Headless: the fixture has no interactive workers (task 1437).
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class,
        })
        .unwrap()
        .id()
}

/// Submit `tasks` as a new proposal owned by `workspace` (a person's
/// planner, or none).
pub(crate) fn submit(
    queue: &mut SqliteQueue,
    tasks: &[TaskId],
    workspace: Option<&str>,
) -> ProposalId {
    queue
        .submit(Submission {
            tasks: tasks.to_vec(),
            goals: Vec::new(),
            proposal: None,
            owner: PlannerOwner {
                origin: PlannerOrigin::Person,
                workspace_id: workspace.map(str::to_owned),
            },
        })
        .unwrap()
        .id()
}

/// The headless plan review: each job prints the next verdict (the last
/// repeats) and its prompt is kept.
pub(crate) struct StubReviewer {
    pub(crate) verdicts: Mutex<Vec<String>>,
    prompts: Mutex<Vec<String>>,
    /// The tasks the first job's run edits through the queue at `.0`, as
    /// `dagq edit` would while the job runs.
    edit: Mutex<Option<(PathBuf, Vec<TaskId>)>>,
    /// The model and effort each job was given (ADR-0079 decision 7).
    models: Mutex<Vec<(String, String)>>,
    /// A file each job waits for before it prints its verdict; it touches
    /// a line to `<gate>.done` once printed.
    gate: Mutex<Option<PathBuf>>,
}

impl StubReviewer {
    pub(crate) fn new(verdicts: &[Value]) -> Self {
        Self {
            verdicts: Mutex::new(verdicts.iter().map(Value::to_string).collect()),
            prompts: Mutex::new(Vec::new()),
            edit: Mutex::new(None),
            models: Mutex::new(Vec::new()),
            gate: Mutex::new(None),
        }
    }
    /// The first job edits `tasks` of the queue at `db` while it runs.
    pub(crate) fn editing(self, db: &Path, tasks: &[TaskId]) -> Self {
        *self.edit.lock().unwrap() = Some((db.to_owned(), tasks.to_vec()));
        self
    }
    /// Each job waits for the file `gate` before it prints its verdict,
    /// and appends a line to `<gate>.done` after.
    pub(crate) fn gated(self, gate: &Path) -> Self {
        *self.gate.lock().unwrap() = Some(gate.to_owned());
        self
    }
    /// A job that fails: it exits non-zero.
    pub(crate) fn failing() -> Self {
        Self {
            verdicts: Mutex::new(vec!["FAIL".into()]),
            prompts: Mutex::new(Vec::new()),
            edit: Mutex::new(None),
            models: Mutex::new(Vec::new()),
            gate: Mutex::new(None),
        }
    }
    /// A first job that stops at Claude Code's usage limit (it prints the
    /// limit and exits non-zero), then jobs that print `verdict`.
    pub(crate) fn limited_then(verdict: &Value) -> Self {
        Self {
            verdicts: Mutex::new(vec![LIMIT.into(), verdict.to_string()]),
            ..Self::new(&[])
        }
    }
    pub(crate) fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
    pub(crate) fn models(&self) -> Vec<(String, String)> {
        self.models.lock().unwrap().clone()
    }
}

impl AgentProvider for StubReviewer {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn headless_command(&self, cwd: &Path, prompt: &str, access: JobAccess) -> Result<CommandSpec> {
        // The run of an in-progress task is left by a supervisor that is
        // gone: its recovery job cannot start, and it waits (task 635).
        if access == runtime::TRIAGE_ACCESS {
            bail!("no recovery job runs in these tests");
        }
        // Files and the dagq CLI; the reviewer role in its env makes the
        // CLI refuse every command that writes. The plan and the goal
        // review name the same intent.
        assert_eq!(access, JobAccess::ReadFilesAndQueueCli);
        assert_eq!(runtime::PLAN_REVIEW_ACCESS, access);
        assert_eq!(runtime::GOAL_REVIEW_ACCESS, access);
        self.prompts.lock().unwrap().push(prompt.into());
        // The job has started: its row and its event are in the queue.
        if let Some((db, tasks)) = self.edit.lock().unwrap().take() {
            let mut queue = SqliteQueue::open(&db)?;
            for task in tasks {
                queue.edit_task(
                    task,
                    TaskEdit {
                        description: Some("edited while its review ran".into()),
                        ..TaskEdit::default()
                    },
                    dagq::domain::TaskStatus::Submitted,
                )?;
            }
        }
        let mut verdicts = self.verdicts.lock().unwrap();
        let verdict = if verdicts.len() > 1 {
            verdicts.remove(0)
        } else {
            verdicts[0].clone()
        };
        let script = if verdict == "FAIL" {
            "echo 'model unavailable' >&2; exit 3".to_owned()
        } else if verdict == LIMIT {
            "printf 'Claude AI usage limit reached|1759000000\\n'; exit 1".to_owned()
        } else {
            format!("printf '%s\\n' '{verdict}'")
        };
        let script = match &*self.gate.lock().unwrap() {
            Some(gate) => {
                let done = shell_quote(&format!("{}.done", gate.display()));
                let gate = shell_quote(gate.to_str().unwrap());
                format!(
                    "{}; {script}; echo >> {done}",
                    crate::common::await_file(&gate)
                )
            }
            None => script,
        };
        // The job's actor (ADR-t728-1 decision 4), for [`job_actors`],
        // beside the queue, which the job is not named: its token's file is
        // `<queue dir>/service/credentials/<hash>` (goal 82's stage (3)).
        let script = format!(
            "printf '%s %s\\n' \"$DAGQ_ROLE\" \"$DAGQ_ACTOR_ID\" >> \"$(dirname \"$(dirname \"$(dirname \"$DAGQ_SERVICE_CREDENTIAL_FILE\")\")\")/{JOB_ACTORS}\"; {script}"
        );
        let missing = verdict.trim_matches('"') == MISSING;
        let mut command = CommandSpec::new(if missing {
            "/nonexistent/sh"
        } else {
            "/bin/sh"
        });
        command.current_dir(cwd).arg("-c").arg(script);
        if verdict.trim_matches('"') == TOO_LONG {
            command.arg("x".repeat(2 << 20));
        }
        Ok(command)
    }
    fn review_command(&self, _: &TaskRun, _: &Path, _: &str, _: JobAccess) -> Result<CommandSpec> {
        unreachable!("no run is reviewed in these tests")
    }
    fn review_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }
    fn select_model(&self, _: &mut CommandSpec, model: &str, effort: &str) {
        self.models
            .lock()
            .unwrap()
            .push((model.into(), effort.into()));
    }
}

/// The verdict of a [`StubReviewer`] job that stops at the usage limit.
pub(crate) const LIMIT: &str = "LIMIT";
/// The verdict of a [`StubReviewer`] job whose arguments pass the
/// system's limit, so it fails to start with `E2BIG` (task 1560).
pub(crate) const TOO_LONG: &str = "TOO_LONG";
/// The verdict of a [`StubReviewer`] job whose executable is not there.
pub(crate) const MISSING: &str = "MISSING";

/// Where [`StubReviewer`]'s jobs append their role and actor id, next to
/// the queue.
pub(crate) const JOB_ACTORS: &str = "job-actors.txt";

/// The role and actor id of every job [`StubReviewer`] ran on `db`, in
/// order.
pub(crate) fn job_actors(db: &Path) -> Vec<String> {
    std::fs::read_to_string(db.parent().unwrap().join(JOB_ACTORS))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The session wrappers as far as plan review uses them: the wrappers of
/// the runtime's planners started in the background (ADR-t1404-1 decision
/// 8, parked unless [`Self::running`]), and the stops. No other session ID
/// is open: the runtime opens no workspace for a planner and calls no cmux
/// (ADR-t1433-2).
pub(crate) struct PlanWorkspace {
    closed: Mutex<Vec<String>>,
    /// The session IDs `exists` was asked about, in order.
    pub(crate) asked: Mutex<Vec<String>>,
    /// The wrappers started in the background (ADR-t1404-1 decision 8).
    pub(crate) background: BackgroundWrappers,
    /// How long a worker's parked wrapper is waited for before its run is
    /// given up ([`Self::giving_up_after`]); the host's default otherwise.
    registration: Option<std::time::Duration>,
}

impl Default for PlanWorkspace {
    /// The runtime's planners' wrappers parked: the test plays them.
    fn default() -> Self {
        Self {
            closed: Mutex::default(),
            asked: Mutex::default(),
            background: BackgroundWrappers::parked(),
            registration: None,
        }
    }
}

impl PlanWorkspace {
    /// The runtime's planners' wrappers run as they do on a host.
    pub(crate) fn running() -> Self {
        Self {
            background: BackgroundWrappers::default(),
            ..Self::default()
        }
    }
    /// Parked wrappers, a worker's run given up `after` its start: a test
    /// that reads only what a claim provisions does not wait out the
    /// wrapper the session never starts.
    pub(crate) fn giving_up_after(after: std::time::Duration) -> Self {
        Self {
            registration: Some(after),
            ..Self::default()
        }
    }
    /// The handles of the wrappers started in the background, in order.
    pub(crate) fn launched(&self) -> Vec<String> {
        self.background
            .launched()
            .into_iter()
            .map(|(handle, ..)| handle)
            .collect()
    }
    pub(crate) fn closed(&self) -> Vec<String> {
        self.closed.lock().unwrap().clone()
    }
}

impl SessionWrappers for PlanWorkspace {
    fn launch_background(
        &self,
        cwd: &Path,
        command: &str,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<String> {
        self.background.launch(cwd, command, env, log)
    }
    fn stop_background(&self, handle: &str, _: StopRoute) -> Result<Option<WrapperStop>> {
        self.background.stop(handle);
        self.closed.lock().unwrap().push(handle.into());
        Ok(None)
    }
    fn exists(&self, handle: &str) -> Result<bool> {
        self.asked.lock().unwrap().push(handle.into());
        Ok(dagq::domain::background_wrapper::is_background(handle) && self.background.runs(handle))
    }
    fn registration_timeout(&self) -> std::time::Duration {
        self.registration
            .unwrap_or_else(|| std::time::Duration::from_secs(45))
    }
}

pub(crate) fn options(runtime_planners: usize, planner_timeout: Duration) -> SuperviseOptions {
    SuperviseOptions {
        tick: Duration::from_millis(20),
        idle_poll: Duration::from_millis(20),
        generators: clock::system(),
        runtime_planners: Some(runtime_planners),
        planner_timeout,
        // No Codex unless a test gives its stub: the host's `codex` is not
        // these tests', and each supervise would run its `--version`.
        codex: PathBuf::from("/nonexistent/codex"),
        ..SuperviseOptions::new(2, true)
    }
}

pub(crate) fn supervise(fx: &Fixture, backend: &PlanWorkspace, reviewer: &StubReviewer) -> Value {
    supervise_with(
        fx,
        backend,
        reviewer,
        &options(1, Duration::from_secs(3600)),
    )
}

pub(crate) fn supervise_with(
    fx: &Fixture,
    backend: &PlanWorkspace,
    reviewer: &StubReviewer,
    options: &SuperviseOptions,
) -> Value {
    runtime::supervise_with_reviewer(
        &fx.db,
        &fx.repo,
        backend,
        &fx.claude,
        reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        options,
    )
    .unwrap()
}

/// [`supervise`] given the `dagq` the test owns
/// ([`common::service::owned_executable`], in the queue's directory), so a
/// service the supervisor starts stops once the test's process is gone.
pub(crate) fn supervise_owned(
    fx: &Fixture,
    backend: &PlanWorkspace,
    reviewer: &StubReviewer,
) -> Value {
    supervise_owned_with(
        fx,
        backend,
        reviewer,
        &options(1, Duration::from_secs(3600)),
    )
}

/// [`supervise_owned`] with `options`.
pub(crate) fn supervise_owned_with(
    fx: &Fixture,
    backend: &PlanWorkspace,
    reviewer: &StubReviewer,
    options: &SuperviseOptions,
) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    runtime::supervise_with_reviewer(
        &fx.db,
        &fx.repo,
        backend,
        &fx.claude,
        reviewer,
        &common::service::owned_executable(fx.db.parent().unwrap()),
        options,
    )
    .unwrap()
}

pub(crate) fn open_goal(queue: &mut SqliteQueue) -> dagq::domain::GoalId {
    queue
        .add_goal(NewGoal {
            priority: Default::default(),
            title: "tidy the queue".into(),
            description: "d".into(),
            acceptance: "every draft is decided".into(),
            constraints: "no new tables".into(),
            doc: None,
            draft: false,
            tags: Vec::new(),
        })
        .unwrap()
        .id()
}

pub(crate) fn planner_prompt(db: &Path, planner: dagq::domain::PlannerId) -> String {
    fs::read_to_string(
        planners_dir(db)
            .join(planner.to_string())
            .join("prompt.txt"),
    )
    .unwrap()
}

/// Make `task` a Spike of `goal` that completed with `verdict` as an older
/// binary's landing leaves it: completed, its receipt recorded, no request.
pub(crate) fn completed_spike(db: &Path, task: TaskId, goal: Option<GoalId>, verdict: &str) {
    let connection = Connection::open(db).unwrap();
    connection
        .execute(
            "UPDATE tasks SET execution_class='spike', status='completed', goal_id=?2 WHERE id=?1",
            params![task.as_i64(), goal.map(GoalId::as_i64)],
        )
        .unwrap();
    let receipt = json!({"receipt": {
        "summary": format!("measured: {verdict}"),
        "follow_ups": [{"title": format!("tidy after {task}"), "description": "d", "category": "improvement"}],
        "spike_result": {
            "verdict": verdict,
            "grounds": format!("grounds of {task}"),
            "evidence": "docs/plans/x.md",
            "conditions": {"commit": "0123abc", "tools": "cargo 1.90",
                "provider": "claude headless worker", "environment": "macOS"},
            "spent": "40 minutes",
        },
    }});
    connection
        .execute(
            "INSERT INTO run_events(task_id, kind, payload) VALUES (?1, 'integration_receipt', ?2)",
            params![task.as_i64(), receipt.to_string()],
        )
        .unwrap();
}
