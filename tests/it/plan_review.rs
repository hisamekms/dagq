//! Plan review (ADR-0041 decisions 11-15, 17) through the supervisor loop,
//! with the headless plan review played by a stub provider that prints a
//! scripted verdict. A planner of the runtime's runs headless in the
//! background (ADR-t1433-2): by default its wrapper is parked and the test
//! plays the planner (its registration, its idle marker, the turns it
//! takes); the headless tests run the real wrapper. Only a person's
//! planner, opened before `dagq plan` was abolished, has a workspace the
//! double lists and types into. No task is claimed in these tests: every
//! task that becomes ready waits for a draft blocker.

use crate::common;
use crate::runtime_support::background_wrappers::BackgroundWrappers;
use dagq::infrastructure::adapters::shell_quote;
use dagq::infrastructure::git_binary::git_executable;

use common::Bounded;

use anyhow::{Result, bail};
use dagq::domain::background_wrapper::{StopRoute, WrapperStop};
use dagq::domain::headless_job::JobAccess;
use dagq::{
    application::{AgentProvider, CommandSpec, SessionWrappers, TaskStore, planner_idle_marker},
    domain::{
        AskKind, DraftOrigin, NewAsk, NewGoal, NewTask, PlannerOrigin, PlannerOwner, Priority,
        ProposalId, ProposalStatus, Submission, TaskAction, TaskEdit, TaskId, TaskRun, TaskStatus,
    },
    infrastructure::{
        clock,
        location::{plan_reviews_dir, planners_dir},
        sqlite::SqliteQueue,
    },
    runtime::{self, SuperviseOptions},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::Duration,
};
use tempfile::TempDir;

pub(crate) fn git(repo: &Path, args: &[&str]) {
    let out = Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(repo)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

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
    verdicts: Mutex<Vec<String>>,
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
    fn editing(self, db: &Path, tasks: &[TaskId]) -> Self {
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
    fn review_command(&self, _: &TaskRun, _: &str, _: JobAccess) -> Result<CommandSpec> {
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
const LIMIT: &str = "LIMIT";
/// The verdict of a [`StubReviewer`] job whose arguments pass the
/// system's limit, so it fails to start with `E2BIG` (task 1560).
pub(crate) const TOO_LONG: &str = "TOO_LONG";
/// The verdict of a [`StubReviewer`] job whose executable is not there.
pub(crate) const MISSING: &str = "MISSING";

/// Where [`StubReviewer`]'s jobs append their role and actor id, next to
/// the queue.
const JOB_ACTORS: &str = "job-actors.txt";

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
}

impl Default for PlanWorkspace {
    /// The runtime's planners' wrappers parked: the test plays them.
    fn default() -> Self {
        Self {
            closed: Mutex::default(),
            asked: Mutex::default(),
            background: BackgroundWrappers::parked(),
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
        update: dagq::application::supervise::UpdateSettings {
            cmux: Some(PathBuf::from("/usr/bin/true")),
            ..Default::default()
        },
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

pub(crate) fn status(queue: &mut SqliteQueue, id: TaskId) -> TaskStatus {
    queue.show(id).unwrap().task.status()
}

/// The payloads of the task's events of `kind`.
pub(crate) fn events(queue: &mut SqliteQueue, id: TaskId, kind: &str) -> Vec<Value> {
    queue
        .show(id)
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

fn proposal_column(db: &Path, id: ProposalId, column: &str) -> Value {
    let connection = Connection::open(db).unwrap();
    connection
        .query_row(
            &format!("SELECT {column} FROM proposals WHERE id=?1"),
            [id.as_i64()],
            |r| {
                Ok(match r.get_ref(0)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => json!(n),
                    rusqlite::types::ValueRef::Text(t) => {
                        json!(String::from_utf8_lossy(t).into_owned())
                    }
                    _ => Value::Null,
                })
            },
        )
        .unwrap()
}

/// A person's planner in workspace `workspace`, alive (its wrapper is this
/// test process) and idle.
pub(crate) fn idle_person_planner(queue: &SqliteQueue, db: &Path, workspace: &str) {
    let planner = queue.open_planner(PlannerOrigin::Person, None).unwrap();
    queue
        .planner_workspace_created(planner.id, workspace)
        .unwrap();
    queue
        .register_planner_wrapper(planner.id, std::process::id())
        .unwrap();
    queue
        .register_planner_agent(planner.id, std::process::id(), std::process::id())
        .unwrap();
    let dir = planners_dir(db).join(planner.id.to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::write(planner_idle_marker(&dir), "{}").unwrap();
}

/// `prompt_bytes` of the event that ends a plan review adds up and
/// matches the prompt the job was given (task 1561, ADR-t1566-1 decision 6).
pub(crate) fn assert_prompt_bytes(recorded: &Value, prompt: &str) {
    let bytes = &recorded["prompt_bytes"];
    assert_eq!(bytes["total"], json!(prompt.len()), "{recorded}");
    assert_eq!(bytes["limit"], 400_000);
    let sections = bytes["sections"].as_object().unwrap();
    let sum: u64 = sections.values().map(|n| n.as_u64().unwrap()).sum();
    assert_eq!(json!(sum), bytes["total"]);
    assert!(sections["tasks"].as_u64().unwrap() > 0);
    assert_eq!(bytes["over_limit"], Value::Null);
}

/// The plan review's wiring end to end: the queue's candidate (its
/// `interrupt` read in SQL), the job started in the checkout as its actor
/// with the prompt kept, the verdict's actions applied in one transaction
/// (a dependency, a lower priority and a cancel as a duplicate, recorded as
/// `cancel --duplicate-of` does), the predictions recorded per task, and
/// the prompt's bytes on `plan_review_finished`.
#[test]
fn a_passing_plan_review_readies_the_proposal_with_its_actions() {
    use dagq::application::PlanReviewStore;
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let two = add(&mut queue, "two", &[blocker], Priority::Normal);
    let three = add(&mut queue, "three", &[blocker], Priority::Interrupt);
    let copy = add(&mut queue, "copy", &[blocker], Priority::Normal);
    let proposal = submit(&mut queue, &[two, three, copy], None);
    let candidates = queue.plan_review_candidates().unwrap();
    assert_eq!(candidates.len(), 1, "{candidates:?}");
    assert_eq!(candidates[0].proposal_id, proposal);
    assert!(candidates[0].interrupt, "{candidates:?}");
    // Landings conflicted in a file main has and in one it no longer has:
    // the prompt lists the first as a hotspot (goal 31).
    let conn = Connection::open(&fx.db).unwrap();
    conn.execute(
        "INSERT INTO run_events(task_id, kind, payload) VALUES (?1, 'conflict_precheck', ?2)",
        rusqlite::params![
            blocker.as_i64(),
            json!({"main": "m", "conflicts": ["seed.txt", "gone.txt"]}).to_string()
        ],
    )
    .unwrap();
    let predict = |task: TaskId, tokens: u64| {
        json!({"task_id": task, "size": "M", "nature": "implementation", "uncertainty": 0.4,
               "expected_output_tokens": tokens, "rework_probability": 0.2, "reason": "a module"})
    };
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "pass", "reasons": [], "summary": "sound",
        "actions": [
            {"action": "add_dependency", "task_id": three, "depends_on": two},
            {"action": "lower_priority", "task_id": two, "priority": "low"},
            {"action": "cancel_duplicate", "task_id": copy, "duplicate_of": blocker}
        ],
        "predictions": [predict(two, 40_000), predict(three, 9_000), predict(copy, 1_000)]
    })]);
    let backend = PlanWorkspace::default();
    let outcome = supervise(&fx, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(status(&mut queue, two), TaskStatus::Ready);
    assert_eq!(status(&mut queue, three), TaskStatus::Ready);
    assert_eq!(
        queue.show_proposal(proposal).unwrap().status(),
        ProposalStatus::Accepted
    );
    assert_eq!(queue.show(three).unwrap().dependencies, [blocker, two]);
    // The task's record says a person made it (ADR-t1975-1 decision 7;
    // no request link): the lower_priority is left unapplied with why,
    // and the rest of the verdict goes on.
    assert_eq!(queue.show(two).unwrap().task.priority(), Priority::Normal);
    assert!(events(&mut queue, two, "task_priority_changed").is_empty());
    // The same record as `cancel --duplicate-of` (ADR-0046 decision 5),
    // marked as the plan review's.
    assert_eq!(status(&mut queue, copy), TaskStatus::Canceled);
    let changed = events(&mut queue, copy, "task_status_changed");
    let canceled = changed.last().unwrap();
    assert_eq!(canceled["to"], "canceled");
    assert_eq!(canceled["duplicate_of"], json!(blocker));
    assert_eq!(canceled["by"], "plan_review");
    assert!(events(&mut queue, copy, "task_canceled_as_duplicate").is_empty());
    // The events of the proposal are on its first task.
    let started = events(&mut queue, two, "plan_review_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["proposal_id"], json!(proposal));
    // The job runs in the repository's checkout, and its Claude session's
    // span has that cwd (ADR-0048).
    assert_eq!(
        started[0]["cwd"],
        fx.repo.canonicalize().unwrap().to_str().unwrap()
    );
    let opened = events(&mut queue, two, "session_opened");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0]["kind"], "plan_review");
    assert_eq!(opened[0]["cwd"], started[0]["cwd"]);
    let finished = events(&mut queue, two, "plan_review_finished");
    assert_eq!(finished[0]["decision"], "pass");
    assert_eq!(finished[0]["summary"], "sound");
    assert_eq!(
        queue.show(two).unwrap().task.origin().origin.as_str(),
        "human"
    );
    assert_eq!(finished[0].get("origin"), None);
    assert_eq!(
        finished[0]["actions_skipped"],
        json!([{
            "action": {"action": "lower_priority", "task_id": two, "priority": "low"},
            "reason": format!("task {two} is a person's (origin human): plan review does not change its priority; a doubt is a concern"),
        }])
    );
    assert_eq!(finished[0]["priorities_inherited"], json!([]));
    // The weight of each task, recorded per task (ADR-0079 decision 2).
    assert_eq!(finished[0]["prediction_error"], Value::Null);
    let recorded = events(&mut queue, two, "task_weight_predicted");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["proposal_id"], json!(proposal));
    assert_eq!(recorded[0]["plan_review_id"], started[0]["plan_review_id"]);
    assert_eq!(
        recorded[0]["prediction"],
        json!({"size": "M", "nature": "implementation", "uncertainty": 0.4,
               "expected_output_tokens": 40_000, "rework_probability": 0.2,
               "reason": "a module"})
    );
    // The stub's session wrote no transcript naming a model.
    assert_eq!(recorded[0]["model"], Value::Null);
    assert_eq!(recorded[0]["effort"], Value::Null);
    assert_eq!(
        events(&mut queue, three, "task_weight_predicted")[0]["prediction"]["expected_output_tokens"],
        9_000
    );
    // The job ran as the plan review job of the proposal (ADR-t728-1).
    assert_eq!(
        job_actors(&fx.db),
        [format!("plan-review-job plan-review-job:{proposal}:1")]
    );
    // The prompt, kept in the job's directory, points the job at the
    // repository's rules and carries the tasks, the lint result and the
    // checks, the acceptance one included.
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 1);
    let dir = plan_reviews_dir(&fx.db).join(started[0]["plan_review_id"].to_string());
    assert_eq!(
        fs::read_to_string(dir.join("prompt.txt")).unwrap(),
        prompts[0]
    );
    for expected in [
        "You are the plan review of dagq proposal 1",
        "its instructions (AGENTS.md and CLAUDE.md), the documents and rules they name",
        "Where the repository has no AGENTS.md, judge a task's verification, paths and evidence in this order: CLAUDE.md; then what the README, the CI configuration and the build configuration show",
        "\"title\":\"two\"",
        "depends_on_draft",
        "an acceptance criterion that contradicts the task's own description or a sibling task's acceptance",
        "cancel_duplicate (only an obvious duplicate; a doubtful one, or a change that looks already made, is a concern)",
        "Files the landings conflicted in most lately",
        "\"path\":\"seed.txt\"",
        "the runtime lets you run the dagq commands that read",
        "`dagq events --full --task ID`",
        "`--run ID`, `--goal ID`, `--kind KIND` (repeatable), `--since TIME` and `--until TIME`",
        "`dagq timeline RUN`",
        "estimate the weight of each submitted task of the proposal (tasks 2, 3, 4)",
        "\"priority_by\":\"human\"",
        "\"origin\":\"human\",\"origin_kind\":\"person\"",
        "- missing_dependency: ",
    ] {
        assert!(
            prompts[0].contains(expected),
            "{expected:?} not in {}",
            prompts[0]
        );
    }
    assert!(!prompts[0].contains("gone.txt"), "{}", prompts[0]);
    assert_prompt_bytes(&finished[0], &prompts[0]);
    // Reviewed once: a second pass finds nothing to review.
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 1);
}

/// ADR-t1433-2 decision 5: the proposal of a person's planner opened before
/// `dagq plan` was abolished, alive and idle in its workspace, is sent back.
/// The planner's row is closed without cmux (`person_retired`), nothing is
/// typed into its workspace, and the revise, with the precedents, goes to a
/// new planner of the runtime's, the way a revise whose planner closed goes
/// (ADR-0047 decision 12); past the timeout, the inbox is told.
#[test]
fn a_revise_of_a_persons_planner_goes_to_a_new_planner_with_the_precedents_and_times_out_to_the_inbox()
 {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    // A person answered the same kind of mismatch before.
    let earlier = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::Blocked,
            task_id: None,
            run_id: None,
            question: "task 9 changes a type but its acceptance says tests/cli.rs is not changed"
                .into(),
            options: Vec::new(),
            asked_by: "observer".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask
        .id;
    queue
        .answer(
            earlier,
            "drop that acceptance line: the tests follow the type",
        )
        .unwrap();
    idle_person_planner(&queue, &fx.db, "PW");
    let task = add(&mut queue, "retype", &[blocker], Priority::Normal);
    let proposal = submit(&mut queue, &[task], Some("PW"));
    let reason = format!(
        "task {task} changes the type of Foo, yet its acceptance says tests/cli.rs is not changed"
    );
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "revise", "reasons": [reason], "summary": "acceptance contradicts the description",
        "precedents": [earlier]
    })]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    assert!(
        reviewer.prompts()[0].contains(&format!(
            "precedent: ask {earlier} (blocked) asked: task 9 changes a type"
        )),
        "{}",
        reviewer.prompts()[0]
    );
    let revising = queue.show_proposal(proposal).unwrap();
    assert_eq!(revising.status(), ProposalStatus::Revising);
    assert_eq!(revising.revise_count(), 1);
    assert_eq!(status(&mut queue, task), TaskStatus::Draft);
    // The person's planner's row is closed without cmux: nothing typed into
    // its workspace, nothing closed.
    assert!(backend.closed().is_empty(), "{:?}", backend.closed());
    let closes: Vec<Value> = queue
        .latest_events_of("planner_closed", 10)
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .collect();
    assert_eq!(closes.len(), 1, "{closes:?}");
    assert_eq!(closes[0]["origin"], "person");
    assert_eq!(closes[0]["code"], "person_retired");
    assert_eq!(closes[0]["workspace_id"], "PW");
    assert_eq!(closes[0]["workspace_closed"], false);
    // A new planner of the runtime's got the reasons and the precedent.
    let launched = backend.launched();
    assert_eq!(launched.len(), 1, "{launched:?}");
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    assert_eq!(planners[0].origin, PlannerOrigin::Runtime);
    assert_eq!(planners[0].proposal_id, Some(proposal));
    let handle = planners[0].workspace_id.clone().unwrap();
    assert_eq!(launched[0], handle);
    let prompt = planner_prompt(&fx.db, planners[0].id);
    for expected in [
        reason.clone(),
        format!("precedent: ask {earlier}"),
        "a person answered: drop that acceptance line".to_owned(),
    ] {
        assert!(prompt.contains(&expected), "{expected:?} not in {prompt}");
    }
    let sent = events(&mut queue, task, "plan_revise_sent");
    assert_eq!(sent[0]["opened"], true);
    assert_eq!(sent[0]["workspace_id"], handle.as_str());

    // Past the planner timeout without a resubmission, the inbox is told
    // once, and it shows as the planner's attention: the timeout of 0 is
    // past from the second after the revise was sent.
    crate::runtime_support::await_second_after(
        proposal_column(&fx.db, proposal, "revise_sent_at")
            .as_i64()
            .unwrap(),
    );
    let quick = options(1, Duration::ZERO);
    supervise_with(&fx, &backend, &reviewer, &quick);
    supervise_with(&fx, &backend, &reviewer, &quick);
    let unresponsive = events(&mut queue, task, "planner_unresponsive");
    assert_eq!(unresponsive.len(), 1);
    let status_now = runtime::status(&fx.db).unwrap();
    let attention = status_now["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "planner_unresponsive")
        .cloned()
        .unwrap_or_else(|| panic!("{status_now}"));
    assert_eq!(attention["next"], "check the planner");
    assert_eq!(attention["task_id"], json!(task));
    let watched = dagq::compose::events(&fx.db, dagq::domain::EventId::new(0), 100, false).unwrap();
    assert!(
        watched["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "planner_unresponsive" && e["next"] == "check the planner"),
        "{watched}"
    );
    assert_eq!(backend.launched().len(), 1, "the revise is not sent twice");
    assert_eq!(
        reviewer.prompts().len(),
        1,
        "a revising proposal is not reviewed"
    );

    // Submitting it again clears the revise; the next review passes.
    queue
        .submit(Submission {
            tasks: Vec::new(),
            goals: Vec::new(),
            proposal: Some(proposal),
            owner: PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: Some(handle.clone()),
            },
        })
        .unwrap();
    assert_eq!(
        proposal_column(&fx.db, proposal, "unresponsive_at"),
        Value::Null
    );
    let passing = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&fx, &backend, &passing);
    assert_eq!(status(&mut queue, task), TaskStatus::Ready);
}

#[test]
fn a_revise_without_a_live_planner_opens_planners_within_the_limit() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let first = add(&mut queue, "first", &[blocker], Priority::Normal);
    let second = add(&mut queue, "second", &[blocker], Priority::Normal);
    let one = submit(&mut queue, &[first], None);
    let two = submit(&mut queue, &[second], Some("GONE"));
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "revise", "reasons": ["split it"], "summary": "too big"
    })]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 2);
    for proposal in [one, two] {
        assert_eq!(
            queue.show_proposal(proposal).unwrap().status(),
            ProposalStatus::Revising
        );
    }
    // One runtime planner at a time: the older proposal got it, the other
    // waits. It runs in the background, without a workspace.
    let launched = backend.launched();
    assert_eq!(launched.len(), 1, "{launched:?}");
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1);
    assert_eq!(planners[0].origin, PlannerOrigin::Runtime);
    assert_eq!(planners[0].proposal_id, Some(one));
    let prompt = fs::read_to_string(
        planners_dir(&fx.db)
            .join(planners[0].id.to_string())
            .join("prompt.txt"),
    )
    .unwrap();
    assert!(prompt.contains("- split it"), "{prompt}");
    assert!(
        prompt.contains(&format!(
            "dagq events --full --task {first} --kind plan_review_finished"
        )),
        "{prompt}"
    );
    assert!(!events(&mut queue, first, "plan_review_finished").is_empty());
    let sent = &events(&mut queue, first, "plan_revise_sent")[0];
    assert_eq!(sent["opened"], true);
    // Opened for the revise: one effort step above the default, and why
    // (ADR-0079 decision 7 (c)); the plan review itself started as before.
    assert_eq!(sent["effort_raised"], true);
    assert_eq!(
        sent["launch"],
        json!({"role": "runtime_planner", "provider": "claude", "model": "claude-opus-5-5", "effort": "high",
               "source": "revise_escalation", "escalated_from": "medium",
               "escalation_reason": "plan_review_revise"})
    );
    assert_eq!(reviewer.models(), []);
    assert_eq!(
        events(&mut queue, first, "plan_review_started")[0]["launch"]["source"],
        "default"
    );
    assert!(events(&mut queue, second, "plan_revise_sent").is_empty());
    assert_eq!(proposal_column(&fx.db, two, "revise_sent_at"), Value::Null);

    // The planner's session ends without submitting: its background
    // wrapper is stopped and the revise of proposal one goes to a new
    // planner first; the limit still holds for proposal two.
    let handle = planners[0].workspace_id.clone().unwrap();
    assert_eq!(launched[0], handle);
    queue.register_planner_wrapper(planners[0].id, 1).unwrap();
    queue.register_planner_agent(planners[0].id, 1, 1).unwrap();
    queue.planner_exited(planners[0].id, 1, 0).unwrap();
    supervise(&fx, &backend, &reviewer);
    assert!(backend.closed().contains(&handle), "{:?}", backend.closed());
    assert!(queue.planner(planners[0].id).unwrap().closed_at.is_some());
    // The close is recorded once, with why (ADR-t1300-1).
    let closes: Vec<Value> = queue
        .latest_events_of("planner_closed", 10)
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .collect();
    assert_eq!(closes.len(), 1, "{closes:?}");
    assert_eq!(closes[0]["planner_id"], planners[0].id.as_i64());
    assert_eq!(closes[0]["origin"], "runtime");
    assert_eq!(closes[0]["code"], "runtime_exited");
    assert_eq!(closes[0]["workspace_id"], handle.as_str());
    assert_eq!(closes[0]["workspace_closed"], true);
    assert_eq!(closes[0]["exit_code"], 0);
    assert_eq!(events(&mut queue, first, "plan_revise_lost").len(), 1);
    let open = queue.planners(false).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].proposal_id, Some(one));
    assert_eq!(backend.launched().len(), 2);
    assert_eq!(events(&mut queue, first, "plan_revise_sent").len(), 2);
    assert!(events(&mut queue, second, "plan_revise_sent").is_empty());
    assert!(proposal_column(&fx.db, two, "revised_at").is_i64());
}

/// A concern opens an `approve_plan` ask the inbox is told of and holds
/// its proposal; the supervisor applies the answers in the queue
/// (ADR-0041 decision 11) and records what each made of the reasons' codes
/// (`plan_review_outcome`, ADR-t947-1). Withdrawing a held proposal closes
/// its ask, so the answer never reaches the proposal its task joins next,
/// and frees its draft; an answer closed unapplied (its proposal withdrawn
/// or no longer held for it) records `ask_closed` (task 568).
#[test]
fn a_concern_asks_the_inbox_and_the_supervisor_applies_the_answers() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let [kept, dropped, returned, withdrawn, late, stale] =
        ["kept", "dropped", "returned", "withdrawn", "late", "stale"]
            .map(|title| add(&mut queue, title, &[blocker], Priority::Normal));
    queue
        .record_draft_origin(
            withdrawn,
            DraftOrigin::FollowUp,
            // Without a source goal it needs no membership judgement.
            &json!({"run": "r", "source_goal_state": "none"}),
        )
        .unwrap();
    let proposals = [kept, dropped, returned, withdrawn, late, stale]
        .map(|task| submit(&mut queue, &[task], None));
    // A submitted draft no longer waits for a planner.
    assert!(
        queue
            .planner_drafts()
            .unwrap()
            .iter()
            .all(|d| d.task.id() != withdrawn)
    );
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "concern",
        "reasons": [
            {"text": "looks already implemented", "codes": ["task_overlap"]},
            {"text": "odd", "codes": ["someday_code"]},
            "a note in the old form",
        ],
        "summary": "maybe done"
    })]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 6);
    let ask_of = |task: TaskId| {
        asks.iter()
            .find(|ask| ask.task_id == Some(task))
            .unwrap_or_else(|| panic!("no ask of task {task}: {asks:?}"))
            .clone()
    };
    for task in [kept, dropped, returned, withdrawn, late, stale] {
        let ask = ask_of(task);
        assert_eq!(ask.kind, AskKind::ApprovePlan);
        assert_eq!(ask.options, ["ready", "send_back", "cancel"]);
        assert!(
            ask.question.contains("looks already implemented"),
            "{}",
            ask.question
        );
        assert_eq!(status(&mut queue, task), TaskStatus::Submitted);
        let finished = &events(&mut queue, task, "plan_review_finished")[0];
        assert_eq!(
            finished["reasons"],
            json!(["looks already implemented", "odd", "a note in the old form"])
        );
        // A code outside the list is kept as printed.
        assert_eq!(
            finished["reason_codes"],
            json!([["task_overlap"], ["someday_code"], ["unlabeled"]])
        );
        assert_eq!(finished["primary_code"], "task_overlap");
    }
    // The supervisor notifies nobody: the inbox's watch tells of each ask.
    // Held for the person: not reviewed again.
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 6);

    // Withdrawn while held: the runtime closes its ask, and its draft waits
    // for a planner of the runtime's again.
    queue.withdraw_proposal(proposals[3]).unwrap();
    let closed = queue.read_ask(ask_of(withdrawn).id).unwrap();
    assert_eq!(closed.answer.as_deref(), Some("withdrawn"));
    assert!(closed.closed_at.is_some());
    assert_eq!(closed.answered_by.as_deref(), Some("runtime"));
    let answered = &events(&mut queue, withdrawn, "ask_answered")[0];
    assert_eq!(answered["runtime_closed"], true);
    assert_eq!(answered["answered_by"], "runtime");
    assert_eq!(status(&mut queue, withdrawn), TaskStatus::Draft);
    assert!(
        queue
            .planner_drafts()
            .unwrap()
            .iter()
            .any(|d| d.task.id() == withdrawn)
    );
    // Submitted again, it is reviewed again and gets a new ask.
    let again = submit(&mut queue, &[withdrawn], None);
    assert_ne!(again, proposals[3]);
    // Answered, then withdrawn before the supervisor applies the answer.
    queue.answer(ask_of(late).id, "cancel").unwrap();
    queue.withdraw_proposal(proposals[4]).unwrap();
    assert!(queue.read_ask(ask_of(late).id).unwrap().closed_at.is_some());
    assert_eq!(
        events(&mut queue, late, "ask_closed"),
        [json!({"ask_id": ask_of(late).id, "kind": "approve_plan"})]
    );
    // Answered, but the concern no longer holds its proposal.
    queue.answer(ask_of(stale).id, "ready").unwrap();
    Connection::open(&fx.db)
        .unwrap()
        .execute(
            "UPDATE proposals SET review_hold=NULL WHERE id=?1",
            [proposals[5].as_i64()],
        )
        .unwrap();

    queue.answer(ask_of(kept).id, "ready").unwrap();
    queue.answer(ask_of(dropped).id, "cancel").unwrap();
    queue
        .answer(ask_of(returned).id, "send_back: split the parser out first")
        .unwrap();
    // The runtime applies them: none is the inbox's to act on.
    let status_now = runtime::status(&fx.db).unwrap();
    for task in [kept, dropped, returned] {
        let ask = ask_of(task);
        let entry = status_now["attention"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["ask_id"] == json!(ask.id))
            .cloned()
            .unwrap();
        assert_eq!(
            entry["next"],
            format!("applying the answer of ask {} (runtime)", ask.id)
        );
    }
    supervise(&fx, &backend, &reviewer);
    assert_eq!(status(&mut queue, kept), TaskStatus::Ready);
    assert_eq!(
        queue.show_proposal(proposals[0]).unwrap().status(),
        ProposalStatus::Accepted
    );
    assert_eq!(status(&mut queue, dropped), TaskStatus::Canceled);
    assert_eq!(
        queue.show_proposal(proposals[1]).unwrap().status(),
        ProposalStatus::Canceled
    );
    assert_eq!(status(&mut queue, returned), TaskStatus::Draft);
    let reasons = proposal_column(&fx.db, proposals[2], "revise_reasons");
    let reasons: Vec<String> = serde_json::from_str(reasons.as_str().unwrap()).unwrap();
    assert_eq!(
        reasons,
        [
            format!(
                "a person sent the proposal back in ask {}: split the parser out first",
                ask_of(returned).id
            ),
            "looks already implemented".to_owned(),
            "odd".to_owned(),
            "a note in the old form".to_owned(),
        ]
    );
    assert_eq!(
        events(&mut queue, kept, "plan_decided")[0]["answer"],
        "ready"
    );
    for (task, outcome) in [
        (kept, "deviation_accepted"),
        (dropped, "canceled"),
        (returned, "deviation_rejected"),
    ] {
        let outcomes = events(&mut queue, task, "plan_review_outcome");
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        assert_eq!(outcomes[0]["outcome"], outcome);
        assert_eq!(outcomes[0]["primary_code"], "task_overlap");
        assert_eq!(
            outcomes[0]["reason_codes"],
            json!([["task_overlap"], ["someday_code"], ["unlabeled"]])
        );
        assert_eq!(
            outcomes[0]["plan_review_id"],
            events(&mut queue, task, "plan_review_finished")[0]["plan_review_id"]
        );
    }
    let stats = runtime::stats(&fx.db, &Default::default()).unwrap();
    assert_eq!(
        stats["review_reasons"]["plan_review"]["by_code"]["task_overlap"]["outcomes"],
        json!({"deviation_accepted": 1, "deviation_rejected": 1, "canceled": 1}),
        "{}",
        stats["review_reasons"]
    );
    // `kpi` reads the codes in the day's window: concerns, not revises.
    let kpi = common::cli::ok(&fx.db, &["kpi", "--last", "1"]);
    let today = kpi["periods"].as_array().unwrap().last().unwrap().clone();
    let by_code = &today["kpis"]["plan.revise_rate"]["code=task_overlap"];
    assert_eq!(by_code["value"], 0.0, "{today}");
    assert!(by_code["n"].as_i64().unwrap() >= 6, "{today}");
    assert_eq!(
        today["kpis"]["review.sendback_rate"]["all"]["value"],
        Value::Null
    );
    // The answer that no longer applies is closed unapplied.
    assert!(
        queue
            .read_ask(ask_of(stale).id)
            .unwrap()
            .closed_at
            .is_some()
    );
    assert!(events(&mut queue, stale, "plan_decided").is_empty());
    assert_eq!(
        events(&mut queue, stale, "ask_closed"),
        [json!({"ask_id": ask_of(stale).id, "kind": "approve_plan"})]
    );
    assert!(events(&mut queue, late, "plan_decided").is_empty());
    // The withdrawn task, submitted again, has a new ask; nothing else
    // answered is asked again.
    let open = queue.asks(Default::default()).unwrap();
    assert!(
        open.iter()
            .any(|ask| ask.task_id == Some(withdrawn) && ask.id != closed.id),
        "{open:?}"
    );
    for task in [kept, dropped, returned, late] {
        assert!(open.iter().all(|ask| ask.task_id != Some(task)), "{open:?}");
    }
    // The one sent back went to a planner of the runtime's.
    assert_eq!(backend.launched().len(), 1);

    // Withdrawn while revising, it drops the revise: no planner is sent
    // it again, and its draft joins a new proposal.
    queue.withdraw_proposal(proposals[2]).unwrap();
    assert_eq!(
        proposal_column(&fx.db, proposals[2], "revise_reasons"),
        Value::Null
    );
    supervise(&fx, &backend, &reviewer);
    assert_eq!(backend.launched().len(), 1);
    assert_eq!(status(&mut queue, returned), TaskStatus::Draft);
    let again = submit(&mut queue, &[returned], None);
    assert_ne!(again, proposals[2]);
    assert_eq!(status(&mut queue, returned), TaskStatus::Submitted);
}

/// A plan review stopped at the usage limit is no `plan review by hand`
/// (task 438): the job joins the queue's usage-limit `cost` ask, listed in
/// its `affected`, and `usage_limited` is recorded on the queue. While the
/// ask is open no plan review starts; `done` submits the proposal again.
#[test]
fn a_plan_review_at_the_usage_limit_joins_the_cost_ask_and_starts_again_after_done() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "limited", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::limited_then(
        &json!({"verdict": "pass", "reasons": [], "summary": "fits the goal"}),
    );
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(events(&mut queue, task, "plan_review_failed").len(), 1);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = &asks[0];
    assert_eq!(ask.kind, AskKind::QueueHold);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Cost);
    assert_eq!(ask.subject.as_deref(), Some("usage_limit"));
    let entry = format!("plan_review job of proposal {proposal}");
    assert_eq!(ask.affected, std::slice::from_ref(&entry));
    assert!(
        ask.question.ends_with(&format!("\n\nAffected: {entry}")),
        "{}",
        ask.question
    );
    let limited: Vec<_> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "usage_limited")
        .collect();
    assert_eq!(limited.len(), 1, "{limited:?}");
    assert_eq!(limited[0].task_id, None);
    assert_eq!(limited[0].payload["job"], "plan_review");
    assert_eq!(limited[0].payload["ask_id"], json!(ask.id));
    // The ask is the attention, not the failed plan review.
    let status_now = runtime::status(&fx.db).unwrap();
    let kinds: Vec<&Value> = status_now["attention"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| &a["kind"])
        .collect();
    assert!(
        !kinds.contains(&&json!("plan_review_failed")),
        "{status_now}"
    );
    assert!(kinds.contains(&&json!("ask_opened")), "{status_now}");
    // Held: no plan review starts.
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 1);
    queue.answer(ask.id, "done").unwrap();
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 2);
    assert_eq!(status(&mut queue, task), TaskStatus::Ready);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let applied: Vec<_> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "queue_hold_applied")
        .collect();
    assert_eq!(applied[0].payload["jobs"], json!([entry]));
    assert_eq!(
        applied[0].payload["restarted"],
        json!([{"job": "plan_review", "proposal_id": proposal}])
    );
}

#[test]
fn a_failed_plan_review_waits_for_a_person_and_is_not_retried() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "unlucky", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::failing();
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(status(&mut queue, task), TaskStatus::Submitted);
    assert_eq!(proposal_column(&fx.db, proposal, "review_hold"), "failed");
    let failed = events(&mut queue, task, "plan_review_failed");
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("model unavailable"),
        "{failed:?}"
    );
    assert_prompt_bytes(&failed[0], &reviewer.prompts()[0]);
    let status_now = runtime::status(&fx.db).unwrap();
    let attention = status_now["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "plan_review_failed")
        .cloned()
        .unwrap_or_else(|| panic!("{status_now}"));
    assert_eq!(attention["next"], "plan review by hand");
    assert_eq!(attention["task_id"], json!(task));
    // A verdict the runtime cannot apply fails the same way.
    let other = add(
        &mut queue,
        "self-dependent",
        &[TaskId::new(1)],
        Priority::Normal,
    );
    submit(&mut queue, &[other], None);
    let raising = StubReviewer::new(&[json!({
        "verdict": "pass", "reasons": [], "summary": "ok",
        "actions": [{"action": "add_dependency", "task_id": other, "depends_on": other}]
    })]);
    supervise(&fx, &backend, &raising);
    assert_eq!(status(&mut queue, other), TaskStatus::Submitted);
    let failed = events(&mut queue, other, "plan_review_failed");
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("a task cannot depend on itself"),
        "{failed:?}"
    );

    // A person has the first one reviewed again: it goes as it is and
    // passes; the other one is readied with the bypass, which ends its
    // proposal and its attention.
    let again = queue
        .submit(Submission {
            tasks: Vec::new(),
            goals: Vec::new(),
            proposal: Some(proposal),
            owner: PlannerOwner {
                origin: PlannerOrigin::Person,
                workspace_id: None,
            },
        })
        .unwrap();
    assert_eq!(again.status(), ProposalStatus::Submitted);
    assert_eq!(events(&mut queue, task, "proposal_resubmitted").len(), 1);
    queue.transition(other, TaskAction::BypassReview).unwrap();
    let passing = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&fx, &backend, &passing);
    assert_eq!(passing.prompts().len(), 1);
    assert_eq!(status(&mut queue, task), TaskStatus::Ready);
    let settled = events(&mut queue, other, "proposal_settled");
    assert_eq!(settled[0]["status"], "accepted");
    let status_now = runtime::status(&fx.db).unwrap();
    assert!(
        !status_now["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "plan_review_failed"),
        "{status_now}"
    );
    // A proposal not held is not submitted again as it is.
    assert!(
        queue
            .submit(Submission {
                tasks: Vec::new(),
                goals: Vec::new(),
                proposal: Some(proposal),
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: None,
                },
            })
            .is_err()
    );
}

#[test]
fn a_ready_task_the_review_reopens_leaves_the_claim_for_a_planner() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let ready = add(&mut queue, "ready one", &[blocker], Priority::Normal);
    queue.transition(ready, TaskAction::BypassReview).unwrap();
    let running = add(&mut queue, "new one", &[blocker], Priority::Normal);
    // A ready task still in a proposal under review stays where it is.
    let held = add(&mut queue, "held one", &[blocker], Priority::Normal);
    let pending = add(&mut queue, "pending one", &[blocker], Priority::Normal);
    let proposal = submit(&mut queue, &[running], None);
    let active = submit(&mut queue, &[held, pending], None);
    queue.transition(held, TaskAction::BypassReview).unwrap();
    let reviewer = StubReviewer::new(&[
        json!({
            "verdict": "pass", "reasons": [], "summary": "ok",
            "reopen": [{"task_id": ready, "reason": "it must use the new API"},
                       {"task_id": blocker, "reason": "not ready"},
                       {"task_id": held, "reason": "in review"}]
        }),
        json!({"verdict": "pass", "reasons": [], "summary": "ok"}),
    ]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    assert_eq!(status(&mut queue, running), TaskStatus::Ready);
    // Out of the claim, in a proposal of its own, with a planner of the
    // runtime's that has the reason.
    assert_eq!(status(&mut queue, ready), TaskStatus::Submitted);
    let reopened = events(&mut queue, ready, "task_reopened");
    let own = ProposalId::new(reopened[0]["proposal_id"].as_i64().unwrap());
    assert_ne!(own, proposal);
    assert_eq!(
        queue.show_proposal(own).unwrap().status(),
        ProposalStatus::Revising
    );
    let finished = events(&mut queue, running, "plan_review_finished");
    assert_eq!(finished[0]["reopened"][0]["task_id"], json!(ready));
    assert!(
        finished[0]["reopen_skipped"][0]
            .as_str()
            .unwrap()
            .contains("not ready"),
        "{finished:?}"
    );
    assert!(
        finished[0]["reopen_skipped"][1]
            .as_str()
            .unwrap()
            .contains(&format!("is in proposal {active}")),
        "{finished:?}"
    );
    assert_eq!(status(&mut queue, held), TaskStatus::Ready);
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners[0].proposal_id, Some(own));
    let prompt = fs::read_to_string(
        planners_dir(&fx.db)
            .join(planners[0].id.to_string())
            .join("prompt.txt"),
    )
    .unwrap();
    assert!(prompt.contains("it must use the new API"), "{prompt}");
    assert!(
        prompt.contains(&format!(
            "dagq events --full --task {running} --kind plan_review_finished"
        )),
        "{prompt}"
    );
    assert!(!prompt.contains(&format!(
        "dagq events --full --task {ready} --kind plan_review_finished"
    )));
    queue.remove_dependency(ready, blocker).unwrap();
    assert!(queue.candidates().unwrap().is_empty());

    // Its planner submits it again as it is: the submitted task goes back
    // to plan review, and a pass readies it.
    queue
        .submit(Submission {
            tasks: Vec::new(),
            goals: Vec::new(),
            proposal: Some(own),
            owner: PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: planners[0].workspace_id.clone(),
            },
        })
        .unwrap();
    let passing = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    queue.add_dependency(ready, blocker).unwrap();
    supervise(&fx, &backend, &passing);
    assert_eq!(status(&mut queue, ready), TaskStatus::Ready);
    // Its planner, idle with nothing left to do, is asked to exit.
    let dir = planners_dir(&fx.db).join(planners[0].id.to_string());
    queue
        .register_planner_wrapper(planners[0].id, std::process::id())
        .unwrap();
    queue
        .register_planner_agent(planners[0].id, std::process::id(), std::process::id())
        .unwrap();
    fs::write(planner_idle_marker(&dir), "{}").unwrap();
    supervise(&fx, &backend, &passing);
    assert!(exit_requested(&fx.db, planners[0].id));
}

/// A reopened task whose proposal is withdrawn goes back to `draft` with
/// origin `reopened`, and a planner of the runtime's takes it up with the
/// reopen's reason; the proposal's other member only goes back to `draft`
/// (task 418).
#[test]
fn a_withdrawn_reopen_gets_a_planner_of_the_runtimes_with_the_reason() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let ready = add(&mut queue, "ready one", &[blocker], Priority::Normal);
    queue.transition(ready, TaskAction::BypassReview).unwrap();
    let running = add(&mut queue, "new one", &[blocker], Priority::Normal);
    let reviewed = submit(&mut queue, &[running], None);
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "pass", "reasons": [], "summary": "ok",
        "reopen": [{"task_id": ready, "reason": "it must use the new API"}]
    })]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let own = ProposalId::new(
        events(&mut queue, ready, "task_reopened")[0]["proposal_id"]
            .as_i64()
            .unwrap(),
    );
    // Its planner adds a draft of its own to the proposal, then withdraws it.
    let extra = add(&mut queue, "extra one", &[blocker], Priority::Normal);
    queue
        .submit(Submission {
            tasks: vec![extra],
            goals: Vec::new(),
            proposal: Some(own),
            owner: PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: None,
            },
        })
        .unwrap();
    queue.withdraw_proposal(own).unwrap();
    assert_eq!(status(&mut queue, ready), TaskStatus::Draft);
    assert_eq!(status(&mut queue, extra), TaskStatus::Draft);
    assert_eq!(
        queue.draft_origin(ready).unwrap(),
        Some((
            DraftOrigin::Reopened,
            json!({"reason": "it must use the new API", "proposal_id": own, "reviewed_proposal_id": reviewed})
        ))
    );
    assert_eq!(queue.draft_origin(extra).unwrap(), None);

    // While the planner of the withdrawn proposal is open, no other is
    // opened for its task; once it ends, one is.
    let options = options(3, Duration::from_secs(3600));
    supervise_with(&fx, &backend, &reviewer, &options);
    let own_planner = queue.planners(false).unwrap();
    assert_eq!(own_planner.len(), 1, "{own_planner:?}");
    assert_eq!(own_planner[0].proposal_id, Some(own));
    queue
        .register_planner_wrapper(own_planner[0].id, 1)
        .unwrap();
    queue
        .register_planner_agent(own_planner[0].id, 1, 1)
        .unwrap();
    queue.planner_exited(own_planner[0].id, 1, 0).unwrap();
    supervise_with(&fx, &backend, &reviewer, &options);
    supervise_with(&fx, &backend, &reviewer, &options);
    let planners = queue.planners(false).unwrap();
    let planner = planners
        .iter()
        .find(|p| p.draft_task_id == Some(ready))
        .unwrap_or_else(|| panic!("{planners:?}"));
    assert!(planners.iter().all(|p| p.draft_task_id != Some(extra)));
    assert_eq!(
        events(&mut queue, ready, "draft_planner_opened")[0]["origin"],
        "reopened"
    );
    let prompt = planner_prompt(&fx.db, planner.id);
    for expected in [
        format!("draft task {ready}"),
        "## Where it came from: reopened".to_owned(),
        "it must use the new API".to_owned(),
        format!("The plan review of proposal {reviewed} found"),
        format!("reopened it into proposal {own}"),
        format!("dagq edit {ready}"),
        format!("dagq submit {ready}"),
        format!("dagq cancel {ready}"),
        format!("dagq ask --task {ready} --kind planner_question --because scope"),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    // Nothing made it ready: only a submit and plan review do.
    assert_eq!(status(&mut queue, ready), TaskStatus::Draft);
}

pub(crate) use crate::common::queue::runtime_draft;

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

use crate::runtime_support::planner_turns::{exit_requested, idle, turn_requests};

pub(crate) fn planner_prompt(db: &Path, planner: dagq::domain::PlannerId) -> String {
    fs::read_to_string(
        planners_dir(db)
            .join(planner.to_string())
            .join("prompt.txt"),
    )
    .unwrap()
}

#[test]
fn drafts_of_the_runtime_get_planners_within_the_limit_and_a_persons_draft_none() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let source = add(&mut queue, "source", &[], Priority::Normal);
    let follow_up = runtime_draft(
        &mut queue,
        "follow",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": source.as_i64(), "source_run_id": null, "index": 0,
               "category": "flaky_test"}),
    );
    let gap = runtime_draft(
        &mut queue,
        "gap",
        Some(goal),
        DraftOrigin::GoalGap,
        json!({"findings": ["the acceptance names a check nobody runs"]}),
    );
    // A person's draft (the fixture's blocker, and this one) gets none.
    let mine = add(&mut queue, "mine", &[], Priority::Normal);
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();

    // One runtime planner at a time: the older draft first.
    supervise(&fx, &backend, &reviewer);
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    assert_eq!(planners[0].origin, PlannerOrigin::Runtime);
    assert_eq!(planners[0].draft_task_id, Some(follow_up));
    assert_eq!(
        backend.launched(),
        [planners[0].workspace_id.clone().unwrap()]
    );
    let prompt = planner_prompt(&fx.db, planners[0].id);
    for expected in [
        format!("draft task {follow_up}"),
        "## Where it came from: follow_up".to_owned(),
        // The worker's category (ADR-t947-3), with what it means.
        "Category (the worker's; keep it as it is, and judge the draft on its merits): flaky_test: an existing test fails".to_owned(),
        "`dagq events --full --task ID`".to_owned(),
        "`dagq timeline RUN`".to_owned(),
        "### Source task".to_owned(),
        "source: change the type of Foo".to_owned(),
        "## Goal".to_owned(),
        "every draft is decided".to_owned(),
        "no new tables".to_owned(),
        format!("- task {gap} (draft): gap"),
        format!("dagq submit {follow_up}"),
        format!("dagq cancel {follow_up}"),
        format!("dagq cancel {follow_up} --duplicate-of <that task>"),
        format!("dagq ask --task {follow_up} --kind planner_question --because scope"),
        "`follow-up draft (proposed by the receipt of run ".to_owned(),
        "in this order: its AGENTS.md; without one, its CLAUDE.md; without either, what its README, CI configuration and build configuration show; when none of them settles it, decide them yourself from the source, the decisions the repository records and a person's precedents, and ask a person with a `planner_question` ask as below only when that material cannot settle them".to_owned(),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    let opened_events = events(&mut queue, follow_up, "draft_planner_opened");
    assert_eq!(opened_events.len(), 1);
    assert_eq!(opened_events[0]["attempt"], 1);
    assert_eq!(opened_events[0]["origin"], "follow_up");
    // The limit holds while that planner is at work.
    supervise(&fx, &backend, &reviewer);
    assert_eq!(queue.planners(false).unwrap().len(), 1);

    // A higher limit opens one for the goal's gap too, never for a
    // person's draft.
    supervise_with(
        &fx,
        &backend,
        &reviewer,
        &options(3, Duration::from_secs(3600)),
    );
    let planners = queue.planners(false).unwrap();
    assert_eq!(
        planners.iter().map(|p| p.draft_task_id).collect::<Vec<_>>(),
        [Some(follow_up), Some(gap)]
    );
    let prompt = planner_prompt(&fx.db, planners[1].id);
    assert!(
        prompt.contains("## Where it came from: goal_gap")
            && prompt.contains("the acceptance names a check nobody runs")
            && !prompt.contains("Category (the worker's"),
        "{prompt}"
    );
    for draft in [TaskId::new(1), mine] {
        assert!(events(&mut queue, draft, "draft_planner_opened").is_empty());
    }

    // The follow_up's planner drops it (cancels it) and goes idle: the
    // runtime asks it to exit, and no planner is opened for it again.
    queue.transition(follow_up, TaskAction::Cancel).unwrap();
    idle(&queue, &fx.db, planners[0].id);
    supervise(&fx, &backend, &reviewer);
    assert!(exit_requested(&fx.db, planners[0].id));
    queue
        .planner_exited(planners[0].id, std::process::id(), 0)
        .unwrap();
    supervise_with(
        &fx,
        &backend,
        &reviewer,
        &options(3, Duration::from_secs(3600)),
    );
    assert!(queue.planner(planners[0].id).unwrap().closed_at.is_some());
    assert_eq!(
        events(&mut queue, follow_up, "draft_planner_opened").len(),
        1
    );
    // Nothing of this took a plan review.
    assert!(reviewer.prompts().is_empty());
}

#[test]
fn a_planner_question_answer_goes_to_its_planner_as_a_turn_or_is_carried_by_a_new_one() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let first = runtime_draft(
        &mut queue,
        "first",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": null, "index": 0}),
    );
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let planner = queue.planners(false).unwrap()[0].clone();
    assert_eq!(planner.draft_task_id, Some(first));
    let handle = planner.workspace_id.clone().unwrap();

    // The planner cannot decide: it asks and stops.
    let asked = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: Some(first),
            run_id: None,
            question: "is this in the goal?".into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    idle(&queue, &fx.db, planner.id);
    // Answered while it is still there (before a pass ended it for the
    // wait, ADR-t1704-1 decision 2).
    assert!(turn_requests(&fx.db, planner.id).is_empty());
    queue.answer(asked.id, "cancel").unwrap();
    supervise(&fx, &backend, &reviewer);
    assert!(!exit_requested(&fx.db, planner.id));
    // The answer is its next turn's request; nothing is typed.
    let requests = turn_requests(&fx.db, planner.id);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0]["what"], format!("answer of ask {}", asked.id));
    assert_eq!(
        requests[0]["prompt"],
        format!("answer to ask {}: cancel", asked.id)
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert_eq!(events(&mut queue, first, "ask_delivered").len(), 1);
    assert_eq!(
        events(&mut queue, first, "planner_answer_claimed"),
        [json!({
            "ask_id": asked.id,
            "planner_id": planner.id,
            "workspace_id": handle,
            "claimed_at": events(&mut queue, first, "planner_answer_claimed")[0]["claimed_at"],
        })]
    );
    // It is at work on the answer: not asked to exit yet.
    supervise(&fx, &backend, &reviewer);
    assert!(!exit_requested(&fx.db, planner.id));

    // A draft whose planner is gone before the answer: a new planner
    // carries it.
    let second = runtime_draft(
        &mut queue,
        "second",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": null, "index": 1}),
    );
    let asked = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: Some(second),
            run_id: None,
            question: "split it?".into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    // The open question keeps the draft from a planner of its own.
    queue.transition(first, TaskAction::Cancel).unwrap();
    queue
        .planner_exited(planner.id, std::process::id(), 0)
        .unwrap();
    supervise(&fx, &backend, &reviewer);
    let left = queue.planners(false).unwrap();
    assert!(left.is_empty(), "{left:?}");
    queue.answer(asked.id, "adopt").unwrap();
    supervise(&fx, &backend, &reviewer);
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1);
    assert_eq!(planners[0].draft_task_id, Some(second));
    let prompt = planner_prompt(&fx.db, planners[0].id);
    assert!(
        prompt.contains(&format!("answer to ask {}: adopt", asked.id))
            && prompt.contains("split it?"),
        "{prompt}"
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());

    // keep_draft leaves the draft as it is until the inbox records a
    // planning request for it: no runtime planner is opened for it again.
    let asked = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: Some(second),
            run_id: None,
            question: "keep it?".into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(asked.id, "keep_draft").unwrap();
    queue.register_planner_wrapper(planners[0].id, 1).unwrap();
    queue.register_planner_agent(planners[0].id, 1, 1).unwrap();
    queue.planner_exited(planners[0].id, 1, 0).unwrap();
    for _ in 0..3 {
        supervise(&fx, &backend, &reviewer);
    }
    assert!(queue.planners(false).unwrap().is_empty());
    let opened = events(&mut queue, second, "draft_planner_opened");
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert_eq!(opened[0]["ask_id"], json!(asked.id.as_i64() - 1));
    assert_eq!(events(&mut queue, second, "planner_answer_closed").len(), 1);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
}

#[test]
fn a_planner_question_answer_another_supervisor_claimed_is_not_sent_again() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let draft = runtime_draft(
        &mut queue,
        "draft",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": null, "index": 0}),
    );
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let planner = queue.planners(false).unwrap()[0].clone();
    let handle = planner.workspace_id.clone().unwrap();
    let asked = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: Some(draft),
            run_id: None,
            question: "is this in the goal?".into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    // An open ask is nobody's to type yet.
    assert!(
        !queue
            .claim_planner_answer(asked.id, planner.id, &handle)
            .unwrap()
    );
    idle(&queue, &fx.db, planner.id);
    queue.answer(asked.id, "cancel").unwrap();
    // Not to a planner the answer does not go to.
    assert!(
        !queue
            .claim_planner_answer(asked.id, dagq::domain::PlannerId::new(99), &handle)
            .unwrap()
    );
    // The other supervisor of a handoff claims it first, on its own
    // connection; a second claim is refused.
    let mut other = SqliteQueue::open(&fx.db).unwrap();
    assert!(
        other
            .claim_planner_answer(asked.id, planner.id, &handle)
            .unwrap()
    );
    assert!(
        !queue
            .claim_planner_answer(asked.id, planner.id, &handle)
            .unwrap()
    );
    // This supervisor then leaves the delivery to the claimer.
    supervise(&fx, &backend, &reviewer);
    assert!(
        turn_requests(&fx.db, planner.id).is_empty(),
        "{:?}",
        turn_requests(&fx.db, planner.id)
    );
    assert_eq!(events(&mut queue, draft, "planner_answer_claimed").len(), 1);
    assert!(events(&mut queue, draft, "ask_delivered").is_empty());

    // The claimer ended before the delivery: once its claim is older than
    // the lease, the next pass takes it over and sends the answer once.
    Connection::open(&fx.db)
        .unwrap()
        .execute(
            "UPDATE run_events SET payload=json_set(payload,'$.claimed_at',
             json_extract(payload,'$.claimed_at') - ?1) WHERE kind='planner_answer_claimed'",
            [dagq::infrastructure::draft_planners::PLANNER_ANSWER_CLAIM_SECS],
        )
        .unwrap();
    supervise(&fx, &backend, &reviewer);
    let requests = turn_requests(&fx.db, planner.id);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0]["prompt"],
        format!("answer to ask {}: cancel", asked.id)
    );
    assert_eq!(events(&mut queue, draft, "planner_answer_claimed").len(), 2);
    assert_eq!(events(&mut queue, draft, "ask_delivered").len(), 1);
    supervise(&fx, &backend, &reviewer);
    assert_eq!(turn_requests(&fx.db, planner.id).len(), 1);
}

/// Edits during a job are told from the others by the event ids of the
/// queue (ADR-0041 decision 9): the verdict on a task edited during its
/// review is not applied, with why, and the review runs again on the new
/// contents; an edit before a review, or of another proposal's task, leaves
/// its verdict applied. A verdict whose predictions do not hold is applied
/// all the same, with why they are not recorded.
#[test]
fn a_verdict_on_a_task_edited_during_its_review_is_not_applied_and_the_review_runs_again() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let two = add(&mut queue, "two", &[blocker], Priority::Normal);
    let three = add(&mut queue, "three", &[blocker], Priority::Normal);
    let other = add(&mut queue, "other", &[blocker], Priority::Normal);
    let proposal = submit(&mut queue, &[two, three], None);
    let later = submit(&mut queue, &[other], None);
    // The first job (of `proposal`) edits its own task `three` and the task
    // of `later`, whose review starts after it.
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "concern", "reasons": ["maybe done"], "summary": "doubtful"}),
        json!({"verdict": "pass", "reasons": [], "summary": "fine now", "actions": [],
               "predictions": [{"task_id": two, "size": "XL"}]}),
    ])
    .editing(&fx.db, &[three, other]);
    let backend = PlanWorkspace::default();
    let outcome = supervise(&fx, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // The first verdict is dropped with why; the second job reads the
    // edited task and its pass is applied.
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 3);
    assert!(!prompts[0].contains("edited while its review ran"));
    for prompt in &prompts[1..] {
        assert!(prompt.contains("edited while its review ran"), "{prompt}");
    }
    let discarded = events(&mut queue, two, "plan_review_discarded");
    assert_eq!(discarded.len(), 1);
    assert_eq!(discarded[0]["verdict"], "concern");
    assert_eq!(discarded[0]["edited"], json!([three]));
    let finished = events(&mut queue, two, "plan_review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["summary"], "fine now");
    assert_eq!(finished[0]["attempt"], 1);
    assert!(
        finished[0]["prediction_error"]
            .as_str()
            .unwrap()
            .starts_with("the predictions are malformed"),
        "{}",
        finished[0]
    );
    assert!(events(&mut queue, two, "task_weight_predicted").is_empty());
    for (task, proposal) in [(two, proposal), (three, proposal), (other, later)] {
        assert_eq!(status(&mut queue, task), TaskStatus::Ready);
        assert_eq!(
            queue.show_proposal(proposal).unwrap().status(),
            ProposalStatus::Accepted
        );
    }
    assert!(events(&mut queue, other, "plan_review_discarded").is_empty());
    assert!(queue.asks(Default::default()).unwrap().is_empty());

    // A job that edits its own task and then fails is not held for a
    // person either: its end is discarded and the review runs again.
    let five = add(&mut queue, "five", &[blocker], Priority::Normal);
    let failed = submit(&mut queue, &[five], None);
    let failing = StubReviewer::failing().editing(&fx.db, &[five]);
    failing.verdicts.lock().unwrap().push(
        json!({"verdict": "pass", "reasons": [], "summary": "fine now", "actions": []}).to_string(),
    );
    supervise(&fx, &backend, &failing);
    assert_eq!(failing.prompts().len(), 2);
    assert!(events(&mut queue, five, "plan_review_failed").is_empty());
    let discarded = events(&mut queue, five, "plan_review_discarded");
    assert_eq!(discarded.len(), 1);
    assert_eq!(discarded[0]["verdict"], Value::Null);
    assert_eq!(discarded[0]["edited"], json!([five]));
    let error = discarded[0]["error"].as_str().unwrap();
    assert!(error.contains("model unavailable"), "{error}");
    assert!(error.contains(&format!("task {five}")), "{error}");
    assert_eq!(proposal_column(&fx.db, failed, "review_hold"), Value::Null);
    assert_eq!(status(&mut queue, five), TaskStatus::Ready);
    let (outcome, error): (String, String) = Connection::open(&fx.db)
        .unwrap()
        .query_row(
            "SELECT outcome, error FROM plan_reviews ORDER BY id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(outcome, "interrupted");
    assert!(error.contains(&format!("task {three}")), "{error}");
}

/// A task of `title`, `description` and `acceptance`, waiting for the
/// blocker.
pub(crate) fn add_text(
    queue: &mut SqliteQueue,
    title: &str,
    description: &str,
    acceptance: &str,
) -> TaskId {
    queue
        .add(NewTask {
            change: None,
            title: title.into(),
            description: description.into(),
            acceptance: acceptance.into(),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Some(Priority::Normal),
            dependencies: vec![TaskId::new(1)],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Interactive),
            wait_for_build: false,
        })
        .unwrap()
        .id()
}

#[test]
fn the_prompt_lists_each_tasks_duplicate_candidates_but_not_the_proposals_own() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    // Landed: completed with a commit on main; and one canceled.
    let done = add_text(
        &mut queue,
        "runtime: search index for the plan review",
        "adds src/infrastructure/search_index.rs",
        "it finds tasks",
    );
    let dropped = add_text(
        &mut queue,
        "plan review candidates, first try",
        "abandoned",
        "none",
    );
    queue.transition(dropped, TaskAction::Cancel).unwrap();
    let raw = Connection::open(&fx.db).unwrap();
    raw.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    raw.execute(
        "UPDATE tasks SET status = 'completed' WHERE id = ?1",
        [done.as_i64()],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO landed_commits (run_id, task_id, commit_sha, message, landed_at)
         VALUES ('run-landed', ?1, 'abc123', 'runtime: search index in src/infrastructure/search_index.rs', 'now')",
        [done.as_i64()],
    )
    .unwrap();
    let asked = add_text(
        &mut queue,
        "runtime: search index candidates in the plan review",
        "change src/infrastructure/search_index.rs",
        "the candidates are listed",
    );
    let twin = add_text(
        &mut queue,
        "runtime: search index candidates twin",
        "also src/infrastructure/search_index.rs",
        "twin",
    );
    let lone = add_text(&mut queue, "qwertyuiop", "zxcvbnm", "asdfghjkl");
    submit(&mut queue, &[asked, twin, lone], None);
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "pass", "reasons": [], "summary": "sound", "actions": []}),
    ]);
    let outcome = supervise(&fx, &PlanWorkspace::default(), &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let prompt = reviewer.prompts().remove(0);
    assert!(
        prompt.contains("Candidates of duplicates and of changes already made"),
        "{prompt}"
    );
    let candidates: Vec<Value> = prompt
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line.get("related").is_some())
        .collect();
    assert_eq!(
        candidates
            .iter()
            .map(|c| c["task_id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        [asked.as_i64(), twin.as_i64(), lone.as_i64()]
    );
    let own = [asked.as_i64(), twin.as_i64(), lone.as_i64()];
    for entry in &candidates {
        for related in entry["related"].as_array().unwrap() {
            assert!(!own.contains(&related["id"].as_i64().unwrap()), "{entry}");
        }
        for hit in entry["search"].as_array().unwrap() {
            let task = hit["task_id"].as_i64().or(hit["id"].as_i64()).unwrap();
            assert!(!own.contains(&task), "{entry}");
        }
    }
    // The related task that landed, with its clues and status.
    let first = &candidates[0];
    let related = first["related"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == json!(done))
        .unwrap_or_else(|| panic!("{first}"));
    assert_eq!(related["status"], "completed");
    assert!(
        related["clues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["value"] == "src/infrastructure/search_index.rs"),
        "{related}"
    );
    // Search finds the landed task, its commit and the canceled task.
    let search = first["search"].as_array().unwrap();
    let find = |kind: &str, id: Value| {
        search
            .iter()
            .find(|hit| hit["kind"] == kind && hit["id"] == id)
            .unwrap_or_else(|| panic!("{kind} {id} not in {first}"))
    };
    assert_eq!(find("task", json!(done))["status"], "completed");
    assert_eq!(find("task", json!(dropped))["status"], "canceled");
    let commit = find("commit", json!("abc123"));
    assert_eq!(commit["task_id"], json!(done));
    assert_eq!(commit["status"], "completed");
    assert!(search.len() <= 5 && first["related"].as_array().unwrap().len() <= 5);
    // A task nothing resembles has empty lists.
    assert_eq!(candidates[2]["related"], json!([]), "{}", candidates[2]);
    assert_eq!(candidates[2]["search"], json!([]), "{}", candidates[2]);
}

#[test]
fn the_search_candidates_of_a_japanese_title_include_similar_japanese_tasks() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let done = add_text(
        &mut queue,
        "計画の重複を検出する",
        "提案の中から重複を探す",
        "重複が見つかる",
    );
    let raw = Connection::open(&fx.db).unwrap();
    raw.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    raw.execute(
        "UPDATE tasks SET status = 'completed' WHERE id = ?1",
        [done.as_i64()],
    )
    .unwrap();
    // No space and no ASCII: the title is one phrase found nowhere else.
    let asked = add_text(
        &mut queue,
        "重複した計画の候補を一覧にする",
        "候補を並べる",
        "一覧が出る",
    );
    submit(&mut queue, &[asked], None);
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "pass", "reasons": [], "summary": "sound", "actions": []}),
    ]);
    let outcome = supervise(&fx, &PlanWorkspace::default(), &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let prompt = reviewer.prompts().remove(0);
    let entry = prompt
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| line.get("search").is_some() && line["task_id"] == json!(asked))
        .unwrap_or_else(|| panic!("{prompt}"));
    let hit = entry["search"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["kind"] == "task" && hit["id"] == json!(done))
        .unwrap_or_else(|| panic!("{entry}"));
    assert_eq!(hit["status"], "completed");
}

/// A task that declares `paths`, waiting for the draft blocker.
pub(crate) fn add_paths(queue: &mut SqliteQueue, title: &str, paths: &[&str]) -> TaskId {
    queue
        .add(NewTask {
            change: None,
            title: title.into(),
            description: format!("{title}: the long description"),
            acceptance: format!("{title} works"),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            priority: Some(Priority::Normal),
            dependencies: vec![TaskId::new(1)],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Interactive),
            wait_for_build: false,
        })
        .unwrap()
        .id()
}

/// Plan review is an actor of its own, the plan-review-job (ADR-t728-1):
/// the supervisor applies its verdict as data and records the events as
/// its own, at the job's request. An output the runtime cannot read, or
/// one with a field or an action it does not know, readies nothing: the
/// proposal waits for a person (`plan review by hand`).
#[test]
fn a_plan_review_verdict_is_applied_at_its_jobs_request_and_a_broken_one_fails_closed() {
    let fx = fixture();
    let backend = PlanWorkspace::default();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let supervisor = format!("supervisor:{}", std::process::id());
    for (title, broken, expected) in [
        (
            "unreadable",
            json!("no verdict here"),
            "printed no verdict JSON",
        ),
        (
            "unknown field",
            json!({"verdict": "pass", "reasons": [], "summary": "ok", "ready": true}),
            "unknown field `ready`",
        ),
        (
            "unknown action",
            json!({"verdict": "pass", "reasons": [], "summary": "ok",
                   "actions": [{"action": "land", "task_id": 1}]}),
            "unknown variant `land`",
        ),
    ] {
        let task = add(&mut queue, title, &[TaskId::new(1)], Priority::Normal);
        submit(&mut queue, &[task], None);
        supervise(&fx, &backend, &StubReviewer::new(&[broken]));
        assert_eq!(status(&mut queue, task), TaskStatus::Submitted, "{title}");
        let kinds: Vec<String> = queue
            .show(task)
            .unwrap()
            .events
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(
            !kinds.iter().any(|k| k == "plan_review_finished"),
            "{title}: {kinds:?}"
        );
        let failed = events(&mut queue, task, "plan_review_failed");
        assert_eq!(failed.len(), 1, "{title}");
        let error = failed[0]["error"].as_str().unwrap();
        assert!(error.contains(expected), "{title}: {error}");
        let attention = runtime::status(&fx.db).unwrap()["attention"].clone();
        assert!(
            attention
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["kind"] == "plan_review_failed"
                    && a["task_id"] == json!(task)
                    && a["next"] == "plan review by hand"),
            "{title}: {attention}"
        );
    }

    // A readable concern is applied: the verdict and the ask it opens are
    // the supervisor's, requested by the job; the ask's asker stays.
    let task = add(&mut queue, "doubtful", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], None);
    let concern = StubReviewer::new(&[json!({
        "verdict": "concern", "reasons": ["needs a person"], "summary": "doubtful"
    })]);
    supervise(&fx, &backend, &concern);
    assert_eq!(status(&mut queue, task), TaskStatus::Submitted);
    let job = format!("plan-review-job:{proposal}:1");
    let detail = queue.show(task).unwrap();
    for kind in ["plan_review_finished", "ask_opened"] {
        let event = detail
            .events
            .iter()
            .find(|e| e.kind == kind)
            .unwrap_or_else(|| panic!("no {kind}"));
        let actor = event.actor.clone().expect("an actor");
        assert_eq!(
            (actor.role.as_str(), actor.id.as_str(), actor.requested_by),
            ("supervisor", supervisor.as_str(), Some(job.clone())),
            "{kind}"
        );
    }
    let asks = queue.asks(Default::default()).unwrap();
    let ask = asks.iter().find(|a| a.task_id == Some(task)).unwrap();
    assert_eq!(ask.kind, AskKind::ApprovePlan);
    assert_eq!(ask.asked_by, "plan_review");
}
