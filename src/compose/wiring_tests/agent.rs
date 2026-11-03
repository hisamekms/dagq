//! The agent of a wiring test: Claude's headless worker and reviewer as the
//! supervisor and the wrapper start them, whose processes end at once
//! having done what the test's script says. A worker's turn writes the
//! receipt the script gives and Claude's result line; a review's job
//! answers the script's verdict. The turn's output is read by Claude's own
//! adapter ([`ClaudeCode`]'s reader and signals), as a real turn's is.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{git::FakeGit, host::exit};
use crate::{
    application::{
        AgentJobLaunch, AgentProvider, AgentSignals, CommandSpec, Exit, IdleHook, RunFiles,
        RunReads, Spawned, Spawner, StepReader, Streams, TranscriptSource, Transcripts, TurnReader,
        TurnTarget, memory_files::MemoryFiles,
    },
    domain::{
        RunId, TaskRun,
        headless_job::JobAccess,
        headless_job::JobFailure,
        transcript::{TRANSCRIPT_MISSING, Transcript, Unreadable},
        turn::TurnSession,
    },
    infrastructure::{adapters::ClaudeCode, sqlite::SqliteQueue},
};

/// What a worker's turn does in its run's worktree (`worktree` of `git`)
/// and writes as its receipt.
pub(super) type Receipt = fn(&RunId, &Path, &FakeGit) -> Result<Value>;

/// The receipt of a worker that found its base meets the task: no commit,
/// `already_satisfied` on the base with `test -f seed.txt` run there.
pub(super) fn already_satisfied(run: &RunId, worktree: &Path, git: &FakeGit) -> Result<Value> {
    let head = crate::application::Repository::head(git, worktree)?;
    Ok(json!({
        "run_id": run,
        "result": "already_satisfied",
        "commit": head,
        "tests": {"status": "passed", "evidence_or_reason": "test -f seed.txt on the base"},
        "e2e": {"status": "not_applicable", "evidence_or_reason": "the runtime runs it"},
        "subagent_review": {"status": "not_applicable", "evidence_or_reason": "no change to review"},
        "summary": "the base meets it",
        "base_checks": [{"command": "test -f seed.txt", "result": "passed"}],
    }))
}

/// The receipt of a worker that made the task's change: one commit on its
/// worktree, `succeeded` with it.
pub(super) fn succeeded(run: &RunId, worktree: &Path, git: &FakeGit) -> Result<Value> {
    let commit = git.commit_in(worktree)?;
    Ok(json!({
        "run_id": run,
        "result": "succeeded",
        "commit": commit,
        "tests": {"status": "passed", "evidence_or_reason": "the change's tests"},
        "e2e": {"status": "not_applicable", "evidence_or_reason": "the runtime runs it"},
        "subagent_review": {"status": "not_applicable", "evidence_or_reason": "a small change"},
        "summary": "made the change",
    }))
}

/// A worker's turn that ends with no receipt and no commit, as one that
/// stops short of its task: what it gives ([`Value::Null`]) is written
/// nowhere.
pub(super) fn no_receipt(_: &RunId, _: &Path, _: &FakeGit) -> Result<Value> {
    Ok(Value::Null)
}

/// A worker's turn that makes one commit on its worktree and ends with no
/// receipt, as [`no_receipt`].
pub(super) fn committed_no_receipt(_: &RunId, worktree: &Path, git: &FakeGit) -> Result<Value> {
    git.commit_in(worktree)?;
    Ok(Value::Null)
}

/// What a worker's turn does whose session dies before its receipt: one
/// commit on its worktree, then its process ends with 7 on Claude's error
/// result, writing no receipt, so that its session ends.
pub(super) fn committed_and_died(_: &RunId, worktree: &Path, git: &FakeGit) -> Result<Value> {
    git.commit_in(worktree)?;
    Ok(died())
}

/// What a worker's turn does whose session dies before it did anything:
/// its process ends with 7 on Claude's error result, writing no receipt.
pub(super) fn died_at_once(_: &RunId, _: &Path, _: &FakeGit) -> Result<Value> {
    Ok(died())
}

/// The receipt that stands for a turn's process dying ([`committed_and_died`]).
fn died() -> Value {
    json!({"wiring_session_dies": 7})
}

/// The receipt of a worker that made no commit in its turn and wrote
/// `succeeded` naming its worktree's head: on the base, validation fails
/// it as `commit_mismatch`; after an earlier turn's commit, it names that.
pub(super) fn succeeded_at_the_head(run: &RunId, worktree: &Path, git: &FakeGit) -> Result<Value> {
    let head = crate::application::Repository::head(git, worktree)?;
    Ok(json!({
        "run_id": run,
        "result": "succeeded",
        "commit": head,
        "tests": {"status": "passed", "evidence_or_reason": "test -f seed.txt"},
        "e2e": {"status": "not_applicable", "evidence_or_reason": "the runtime runs it"},
        "subagent_review": {"status": "not_applicable", "evidence_or_reason": "no change"},
        "summary": "the base meets it",
    }))
}

/// What a review job meets when its provider wants a login: it prints
/// Claude's login message and ends with 1, giving no verdict. A test's
/// script names it among the verdicts.
pub(super) fn login_wall() -> Value {
    json!({"wiring_login_wall": true})
}

/// A review's verdict `decision` with `summary`.
pub(super) fn verdict(decision: &str, summary: &str) -> Value {
    json!({"verdict": decision, "reasons": [], "summary": summary})
}

/// A recovery job's verdict `repair` of high confidence with the one
/// `action` and `diagnosis`.
pub(super) fn repair(action: Value, diagnosis: &str) -> Value {
    json!({"verdict": "repair", "confidence": "high",
           "diagnosis": diagnosis, "actions": [action]})
}

/// What the agent is told to do: the receipt of each worker's turn in
/// order (a turn past them writes none), and the verdict of each review
/// job and recovery job in the order they start (the last one again for
/// any after it).
pub(super) struct Script {
    pub(super) receipts: Vec<Receipt>,
    pub(super) verdicts: Vec<Value>,
}

/// The programs of the commands this agent makes, which its spawner tells
/// apart.
const TURN: &str = "wiring-worker-turn";
const REVIEW: &str = "wiring-review";
const AGENT_JOB: &str = "wiring-agent-job";
const JOB: &str = "wiring-job";
const RECOVERY: &str = "wiring-recovery-job";

/// The agent: its script, and what it started, by program, in order.
pub(super) struct FakeAgent {
    db: PathBuf,
    files: Arc<MemoryFiles>,
    git: FakeGit,
    claude: ClaudeCode,
    receipts: Mutex<VecDeque<Receipt>>,
    verdicts: Mutex<VecDeque<Value>>,
    started: Mutex<Vec<String>>,
    /// The arguments of each review agent's job it started, in order.
    agent_jobs: Mutex<Vec<Vec<String>>>,
    /// The verdicts of the jobs of a review agent, by its name, one per job
    /// in order (the last one again for any after it); an agent without
    /// any passes.
    agent_verdicts: Mutex<std::collections::HashMap<String, VecDeque<Value>>>,
    /// The directory each review job was started in, in order.
    reviewed_in: Mutex<Vec<PathBuf>>,
    /// The prompt of each review job, in order.
    review_prompts: Mutex<Vec<String>>,
    /// What runs as the next review job starts, before it answers, once.
    on_review: Mutex<Option<super::host::Hook>>,
    /// The session of each worker's turn, in order: `start <name>` for a
    /// new one, `resume <name>` for one gone on.
    turn_sessions: Mutex<Vec<String>>,
    /// The model and effort each worker's turn was started with, in order.
    turn_models: Mutex<Vec<(String, String)>>,
    /// The prompt each worker's turn was given, in order.
    turn_prompts: Mutex<Vec<String>>,
    /// What lets the next worker's turn end, once: the turn runs, its run
    /// holding its slot, until it says so.
    held_turn: Mutex<Option<mpsc::Receiver<()>>>,
    /// The verdict of each recovery job in order, before the script's;
    /// without one left, a job answers the script's next verdict.
    recoveries: Mutex<VecDeque<Value>>,
    /// The prompt of each recovery job and the directory it ran in, in
    /// order.
    recovery_prompts: Mutex<Vec<(String, PathBuf)>>,
    /// What lets each next recovery job end, in order: its process runs
    /// until it says so.
    held_recovery: Mutex<VecDeque<mpsc::Receiver<()>>>,
}

impl FakeAgent {
    /// The agent of the queue at `db` whose turns write `files` in `git`'s
    /// worktrees, doing what `script` says.
    pub(super) fn new(db: &Path, files: Arc<MemoryFiles>, git: FakeGit, script: Script) -> Self {
        Self {
            db: db.to_owned(),
            files,
            git,
            claude: ClaudeCode {
                executable: PathBuf::from("wiring-claude"),
            },
            receipts: Mutex::new(script.receipts.into()),
            verdicts: Mutex::new(script.verdicts.into()),
            started: Mutex::default(),
            agent_jobs: Mutex::default(),
            agent_verdicts: Mutex::default(),
            reviewed_in: Mutex::default(),
            review_prompts: Mutex::default(),
            on_review: Mutex::default(),
            turn_sessions: Mutex::default(),
            turn_models: Mutex::default(),
            turn_prompts: Mutex::default(),
            held_turn: Mutex::default(),
            recoveries: Mutex::default(),
            recovery_prompts: Mutex::default(),
            held_recovery: Mutex::default(),
        }
    }

    /// Hold the next recovery job not held yet: its process starts and
    /// runs until `release` says so or is dropped (by the test's deadline
    /// at the latest); only then does it print its verdict and end. A
    /// signal ends it at once.
    pub(super) fn hold_next_recovery(&self, release: mpsc::Receiver<()>) {
        self.held_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(release);
    }

    /// The agent whose next recovery jobs answer `verdicts`, one per job
    /// in order, before the script's verdicts.
    pub(super) fn recovering(&self, verdicts: Vec<Value>) {
        *self
            .recoveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = verdicts.into();
    }

    /// The prompt of each recovery job and the directory it ran in, in
    /// order.
    pub(super) fn recovery_prompts(&self) -> Vec<(String, PathBuf)> {
        self.recovery_prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The jobs of the review agent `agent` answer `verdicts` (each its
    /// decision and reasons), one per job in order, the last one again for
    /// any after it.
    pub(super) fn review_agent_says(&self, agent: &str, verdicts: &[(&str, &[&str])]) {
        let verdicts = verdicts
            .iter()
            .map(|(decision, reasons)| json!({"verdict": decision, "reasons": reasons}))
            .collect();
        self.agent_verdicts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(agent.to_owned(), verdicts);
    }

    /// The directory each review job was started in, in order.
    pub(super) fn reviewed_in(&self) -> Vec<PathBuf> {
        self.reviewed_in
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The prompt of each review job, in order.
    pub(super) fn review_prompts(&self) -> Vec<String> {
        self.review_prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Run `hook` as the next review job starts, before it answers.
    pub(super) fn on_review(&self, hook: super::host::Hook) {
        *self
            .on_review
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }

    /// Hold the next worker's turn: its process starts, as a turn's does,
    /// and runs until `release` says so or is dropped (by the test's
    /// deadline at the latest); only then does it write its receipt and
    /// output and end.
    pub(super) fn hold_next_turn(&self, release: mpsc::Receiver<()>) {
        *self
            .held_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(release);
    }

    /// The session of each worker's turn, in order: `start <name>` for a
    /// new one, `resume <name>` for one gone on.
    pub(super) fn turn_sessions(&self) -> Vec<String> {
        self.turn_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The model and effort each worker's turn was started with, in order.
    pub(super) fn turn_models(&self) -> Vec<(String, String)> {
        self.turn_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The prompt each worker's turn was given, in order.
    pub(super) fn turn_prompts(&self) -> Vec<String> {
        self.turn_prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The arguments of each review agent's job started, in order.
    pub(super) fn agent_jobs(&self) -> Vec<Vec<String>> {
        self.agent_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// What this agent started, by program, in order.
    pub(super) fn started(&self) -> Vec<String> {
        self.started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn next_verdict(&self) -> Result<Value> {
        let mut verdicts = self
            .verdicts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match verdicts.len() {
            0 => bail!("the wiring test's script has no verdict"),
            1 => Ok(verdicts[0].clone()),
            _ => Ok(verdicts.pop_front().expect("a verdict")),
        }
    }

    /// A worker's turn in the run directory `run_dir` of session
    /// `session`: the script's next receipt, which it writes as it ends.
    fn turn(&self, run_dir: &Path, session: &str) -> Result<Turn> {
        let id = RunId::new(
            run_dir
                .file_name()
                .and_then(|name| name.to_str())
                .context("the turn's run directory")?,
        )?;
        let run: TaskRun = RunReads::run(&SqliteQueue::open(&self.db)?, &id)?;
        let receipt = self
            .receipts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        let receipt = match receipt {
            Some(receipt) => Some((
                receipt,
                PathBuf::from(run.worktree_path().context("the run has no worktree")?),
                run.receipt_path()
                    .map_or_else(|| run_dir.join("receipt.json"), PathBuf::from),
            )),
            None => None,
        };
        Ok(Turn {
            run: id,
            session: session.to_owned(),
            receipt,
            git: self.git.clone(),
            files: self.files.clone(),
        })
    }

    /// A review agent's job answering `prompt`: the agent's next verdict
    /// the test gave ([`FakeAgent::review_agent_says`]), or else a pass, as
    /// a built-in one finds nothing in a change of no file.
    fn agent_job(&self, prompt: &str) -> String {
        let agent = prompt
            .lines()
            .find_map(|line| line.strip_prefix("You are the review agent `"))
            .and_then(|rest| rest.split('`').next())
            .unwrap_or("unnamed");
        let said = {
            let mut verdicts = self
                .agent_verdicts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            verdicts.get_mut(agent).and_then(|said| {
                if said.len() > 1 {
                    said.pop_front()
                } else {
                    said.front().cloned()
                }
            })
        };
        let said = said.unwrap_or_else(|| json!({"verdict": "pass", "reasons": []}));
        format!(
            "{}\n",
            json!({"agent": agent, "status": "completed", "verdict": said["verdict"],
                   "reasons": said["reasons"], "summary": format!("{agent} looked")})
        )
    }
}

impl AgentProvider for FakeAgent {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }

    /// A short wait between the looks of a wrapper and of a job's watch:
    /// the test's own.
    fn wait_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(10)
    }

    fn review_command(
        &self,
        _: &TaskRun,
        tree: &Path,
        prompt: &str,
        _: JobAccess,
    ) -> Result<CommandSpec> {
        self.review_prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(prompt.to_owned());
        let mut command = CommandSpec::new(REVIEW);
        command.current_dir(tree);
        Ok(command)
    }

    /// A headless job's (the recovery job's): it answers the next verdict
    /// the test gave [`FakeAgent::recovering`], or else the script's next
    /// verdict, as a review does; its prompt and directory are recorded.
    fn headless_command(&self, cwd: &Path, prompt: &str, _: JobAccess) -> Result<CommandSpec> {
        self.recovery_prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((prompt.to_owned(), cwd.to_owned()));
        let recovery = self
            .recoveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        let mut command = CommandSpec::new(if recovery.is_some() { RECOVERY } else { JOB });
        command.current_dir(cwd);
        if let Some(verdict) = recovery {
            command.arg(verdict.to_string());
        }
        Ok(command)
    }

    fn agent_job_command(&self, job: &AgentJobLaunch) -> Result<CommandSpec> {
        let mut command = CommandSpec::new(AGENT_JOB);
        command.current_dir(&job.cwd).stdin(job.prompt.as_str());
        Ok(command)
    }

    /// Claude's own arguments of a launch's model and effort, on a review
    /// agent's job only: the other programs take no option; a worker's
    /// turn's model and effort are recorded.
    fn select_model(&self, command: &mut CommandSpec, model: &str, effort: &str) {
        if command.get_program() == AGENT_JOB {
            self.claude.select_model(command, model, effort);
        }
        if command.get_program() == TURN {
            self.turn_models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((model.to_owned(), effort.to_owned()));
        }
    }

    fn turn_command(
        &self,
        target: &TurnTarget<'_>,
        prompt: &str,
        session: TurnSession<'_>,
    ) -> Result<CommandSpec> {
        let (name, how) = match session {
            TurnSession::New(name) => (name, "start"),
            TurnSession::Resume(name) => (name, "resume"),
        };
        self.turn_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("{how} {name}"));
        let mut command = CommandSpec::new(TURN);
        command
            .current_dir(target.cwd)
            .arg(target.dir)
            .arg(name)
            .stdin(prompt);
        Ok(command)
    }

    fn turn_reader(
        &self,
        toolchains: &crate::domain::toolchain::Toolchains,
    ) -> Result<Box<dyn TurnReader>> {
        self.claude.turn_reader(toolchains)
    }

    fn turn_permission_mode(&self) -> Option<&'static str> {
        self.claude.turn_permission_mode()
    }

    fn step_reader(
        &self,
        model: Option<&str>,
        known: crate::domain::command_shape::Subcommands,
    ) -> Box<dyn StepReader> {
        self.claude.step_reader(model, known)
    }
}

impl AgentSignals for FakeAgent {
    fn job_failure(&self, output: &str) -> JobFailure {
        AgentSignals::job_failure(&self.claude, output)
    }

    fn idle_hook(&self, content: &[u8]) -> IdleHook {
        self.claude.idle_hook(content)
    }
}

/// No transcript: Claude keeps none of a test's turns.
impl Transcripts for FakeAgent {
    fn read(
        &self,
        source: &TranscriptSource,
        _: &crate::domain::toolchain::Toolchains,
    ) -> std::result::Result<Transcript, Unreadable> {
        Err(Unreadable {
            code: TRANSCRIPT_MISSING,
            version: None,
            detail: format!("no transcript of {:?} in a wiring test", source.session_id),
        })
    }
}

/// A process of this agent's, ended as it started.
struct Ended {
    pid: u32,
    exit: Exit,
}

impl Spawned for Ended {
    fn id(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> Result<Option<Exit>> {
        Ok(Some(self.exit.clone()))
    }

    fn kill(&mut self) -> Result<()> {
        Ok(())
    }

    fn wait(&mut self) -> Result<Exit> {
        Ok(self.exit.clone())
    }
}

/// Starts what this agent's commands name: each does its work at once,
/// writes its output where `streams` sends it and ends with 0; any other
/// program is refused.
impl Spawner for FakeAgent {
    fn spawn(&self, command: &CommandSpec, streams: Streams<'_>) -> Result<Box<dyn Spawned>> {
        let program = command.get_program().to_string_lossy().into_owned();
        let args: Vec<PathBuf> = command.get_args().map(PathBuf::from).collect();
        let mut code = 0;
        let stdout = match program.as_str() {
            TURN => {
                let [run_dir, session] = args.as_slice() else {
                    bail!("a worker's turn names its run directory and session");
                };
                self.turn_prompts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(command.get_stdin().unwrap_or_default().to_owned());
                let turn = self.turn(run_dir, &session.to_string_lossy())?;
                let held = self
                    .held_turn
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(release) = held {
                    let outputs = Outputs::of(&streams);
                    let files = self.files.clone();
                    return Ok(Box::new(Held {
                        pid: self.record_start(program),
                        release,
                        end: Some(Box::new(move || outputs.write(&files, &turn.end()?.0))),
                        ended: false,
                    }));
                }
                let (stdout, ended) = turn.end()?;
                code = ended;
                stdout
            }
            REVIEW => {
                if let Some(cwd) = command.get_current_dir() {
                    self.reviewed_in
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(cwd.to_owned());
                }
                let hook = self
                    .on_review
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(hook) = hook {
                    hook();
                }
                let verdict = self.next_verdict()?;
                if verdict == login_wall() {
                    code = 1;
                    "Invalid API key · Please run /login\n".to_owned()
                } else {
                    format!("{verdict}\n")
                }
            }
            JOB => format!("{}\n", self.next_verdict()?),
            AGENT_JOB => {
                self.agent_jobs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(
                        args.iter()
                            .map(|arg| arg.to_string_lossy().into_owned())
                            .collect(),
                    );
                self.agent_job(command.get_stdin().unwrap_or_default())
            }
            RECOVERY => {
                let [verdict] = args.as_slice() else {
                    bail!("a recovery job names its verdict");
                };
                let stdout = format!("{}\n", verdict.to_string_lossy());
                let held = self
                    .held_recovery
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_front();
                if let Some(release) = held {
                    let outputs = Outputs::of(&streams);
                    let files = self.files.clone();
                    return Ok(Box::new(Held {
                        pid: self.record_start(program),
                        release,
                        end: Some(Box::new(move || outputs.write(&files, &stdout))),
                        ended: false,
                    }));
                }
                stdout
            }
            other => bail!("the wiring test's agent starts no {other}"),
        };
        Outputs::of(&streams).write(&self.files, &stdout)?;
        Ok(Box::new(Ended {
            pid: self.record_start(program),
            exit: exit(code),
        }))
    }
}

impl FakeAgent {
    /// `program` started: the pid of its process, far from the host's pids
    /// and from the test's processes'.
    fn record_start(&self, program: String) -> u32 {
        let mut started = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        started.push(program);
        5_000_000 + u32::try_from(started.len()).unwrap_or(u32::MAX - 5_000_000)
    }
}

/// A worker's turn of a run, as its process ends: the receipt its script
/// gives, written at the run's receipt path (`worktree` its run's).
struct Turn {
    run: RunId,
    session: String,
    receipt: Option<(Receipt, PathBuf, PathBuf)>,
    git: FakeGit,
    files: Arc<MemoryFiles>,
}

impl Turn {
    /// The turn's receipt written, if the script gives one other than
    /// [`no_receipt`]'s; its output (the model's answer in the turn's
    /// session, then the line of Claude's result) and the code its process
    /// ends with (Claude's error result and 7 when the script's turn dies).
    fn end(self) -> Result<(String, i32)> {
        if let Some((receipt, worktree, path)) = self.receipt {
            let receipt = receipt(&self.run, &worktree, &self.git)?;
            if let Some(code) = receipt["wiring_session_dies"].as_i64() {
                let result = json!({
                    "type": "result", "subtype": "error_during_execution", "is_error": true,
                    "num_turns": 1, "duration_ms": 5, "session_id": self.session,
                    "usage": {"input_tokens": 7, "output_tokens": 3},
                });
                return Ok((format!("{result}\n"), i32::try_from(code)?));
            }
            if !receipt.is_null() {
                self.files.write(&path, receipt.to_string().as_bytes())?;
            }
        }
        Ok((
            format!(
                "{}\n{}\n",
                json!({
                    "type": "assistant", "session_id": self.session,
                    "message": {"model": "wiring-model", "content": [{"type": "text", "text": "done"}]},
                }),
                json!({
                    "type": "result", "subtype": "success", "is_error": false,
                    "num_turns": 1, "duration_ms": 5, "total_cost_usd": 0.01,
                    "session_id": self.session, "result": "done",
                    "usage": {"input_tokens": 7, "output_tokens": 3},
                    "permission_denials": [],
                })
            ),
            0,
        ))
    }
}

/// Where a process's output goes: its output and its errors, each at a
/// path or nowhere.
struct Outputs {
    stdout: Option<PathBuf>,
    stderr: Option<PathBuf>,
}

impl Outputs {
    fn of(streams: &Streams<'_>) -> Self {
        match streams {
            Streams::Files { stdout, stderr } => Self {
                stdout: Some((*stdout).to_owned()),
                stderr: Some((*stderr).to_owned()),
            },
            Streams::Log(log) => Self {
                stdout: Some((*log).to_owned()),
                stderr: None,
            },
            Streams::Inherit | Streams::Null => Self {
                stdout: None,
                stderr: None,
            },
        }
    }

    /// `stdout` written as the output, the errors empty.
    fn write(&self, files: &MemoryFiles, stdout: &str) -> Result<()> {
        if let Some(out) = &self.stdout {
            files.write(out, stdout.as_bytes())?;
        }
        if let Some(err) = &self.stderr {
            files.write(err, b"")?;
        }
        Ok(())
    }
}

/// A worker's turn or a recovery job held in its process
/// ([`FakeAgent::hold_next_turn`], [`FakeAgent::hold_next_recovery`]):
/// running until its release, then writing what it ends with; it ends at
/// the look after that, so that its wrapper reads its output first.
struct Held {
    pid: u32,
    release: mpsc::Receiver<()>,
    end: Option<Box<dyn FnOnce() -> Result<()> + Send>>,
    ended: bool,
}

impl Spawned for Held {
    fn id(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> Result<Option<Exit>> {
        if self.ended {
            return Ok(Some(exit(0)));
        }
        if matches!(self.release.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            return Ok(None);
        }
        if let Some(end) = self.end.take() {
            end()?;
        }
        self.ended = true;
        Ok(None)
    }

    /// A signal ends it at once, writing nothing.
    fn kill(&mut self) -> Result<()> {
        self.end = None;
        self.ended = true;
        Ok(())
    }

    fn wait(&mut self) -> Result<Exit> {
        if !self.ended {
            let _ = self.release.recv_timeout(super::harness::LIMIT);
            if let Some(end) = self.end.take() {
                end()?;
            }
            self.ended = true;
        }
        Ok(exit(0))
    }
}
