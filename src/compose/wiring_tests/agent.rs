//! The agent of a wiring test: Claude's headless worker and reviewer as the
//! supervisor and the wrapper start them, whose processes end at once
//! having done what the test's script says. A worker's turn writes the
//! receipt the script gives and Claude's result line; a review's job
//! answers the script's verdict. The turn's output is read by Claude's own
//! adapter ([`ClaudeCode`]'s reader and signals), as a real turn's is.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
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
        CommitSha, RunId, TaskRun,
        headless_job::JobAccess,
        headless_job::JobFailure,
        transcript::{TRANSCRIPT_MISSING, Transcript, Unreadable},
        turn::TurnSession,
    },
    infrastructure::{adapters::ClaudeCode, sqlite::SqliteQueue},
};

/// What a worker's turn writes as its receipt, given its run and the head
/// of its worktree.
pub(super) type Receipt = fn(&RunId, &CommitSha) -> Value;

/// The receipt of a worker that found its base meets the task: no commit,
/// `already_satisfied` on the base with `test -f seed.txt` run there.
pub(super) fn already_satisfied(run: &RunId, head: &CommitSha) -> Value {
    json!({
        "run_id": run,
        "result": "already_satisfied",
        "commit": head,
        "tests": {"status": "passed", "evidence_or_reason": "test -f seed.txt on the base"},
        "e2e": {"status": "not_applicable", "evidence_or_reason": "the runtime runs it"},
        "subagent_review": {"status": "not_applicable", "evidence_or_reason": "no change to review"},
        "summary": "the base meets it",
        "base_checks": [{"command": "test -f seed.txt", "result": "passed"}],
    })
}

/// A review's verdict `decision` with `summary`.
pub(super) fn verdict(decision: &str, summary: &str) -> Value {
    json!({"verdict": decision, "reasons": [], "summary": summary})
}

/// What the agent is told to do: the receipt of each worker's turn in
/// order (a turn past them writes none), and the verdict of each review
/// job in order (the last one again for any after it).
pub(super) struct Script {
    pub(super) receipts: Vec<Receipt>,
    pub(super) verdicts: Vec<Value>,
}

/// The programs of the commands this agent makes, which its spawner tells
/// apart.
const TURN: &str = "wiring-worker-turn";
const REVIEW: &str = "wiring-review";
const AGENT_JOB: &str = "wiring-agent-job";

/// The agent: its script, and what it started, by program, in order.
pub(super) struct FakeAgent {
    db: PathBuf,
    files: Arc<MemoryFiles>,
    git: FakeGit,
    claude: ClaudeCode,
    receipts: Mutex<VecDeque<Receipt>>,
    verdicts: Mutex<VecDeque<Value>>,
    started: Mutex<Vec<String>>,
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
        }
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
    /// `session`: the script's next receipt at the run's receipt path, and
    /// the line of Claude's result as its output.
    fn turn(&self, run_dir: &Path, session: &str) -> Result<String> {
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
        if let Some(receipt) = receipt {
            let worktree = run.worktree_path().context("the run has no worktree")?;
            let head = crate::application::Repository::head(&self.git, Path::new(worktree))?;
            let path = run
                .receipt_path()
                .map_or_else(|| run_dir.join("receipt.json"), PathBuf::from);
            self.files
                .write(&path, receipt(&id, &head).to_string().as_bytes())?;
        }
        Ok(format!(
            "{}\n",
            json!({
                "type": "result", "subtype": "success", "is_error": false,
                "num_turns": 1, "duration_ms": 5, "total_cost_usd": 0.01,
                "session_id": session, "result": "done",
                "usage": {"input_tokens": 7, "output_tokens": 3},
                "permission_denials": [],
            })
        ))
    }

    /// A review agent's job answering `prompt`: a built-in one passes, as
    /// it finds nothing in a change of no file.
    fn agent_job(prompt: &str) -> String {
        let agent = prompt
            .lines()
            .find_map(|line| line.strip_prefix("You are the review agent `"))
            .and_then(|rest| rest.split('`').next())
            .unwrap_or("unnamed");
        format!(
            "{}\n",
            json!({"agent": agent, "status": "completed", "verdict": "pass",
                   "reasons": [], "summary": format!("{agent} looked")})
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
        _: &str,
        _: JobAccess,
    ) -> Result<CommandSpec> {
        let mut command = CommandSpec::new(REVIEW);
        command.current_dir(tree);
        Ok(command)
    }

    fn agent_job_command(&self, job: &AgentJobLaunch) -> Result<CommandSpec> {
        let mut command = CommandSpec::new(AGENT_JOB);
        command.current_dir(&job.cwd).stdin(job.prompt.as_str());
        Ok(command)
    }

    fn turn_command(
        &self,
        target: &TurnTarget<'_>,
        prompt: &str,
        session: TurnSession<'_>,
    ) -> Result<CommandSpec> {
        let name = match session {
            TurnSession::New(name) | TurnSession::Resume(name) => name,
        };
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
        let stdout = match program.as_str() {
            TURN => {
                let [run_dir, session] = args.as_slice() else {
                    bail!("a worker's turn names its run directory and session");
                };
                self.turn(run_dir, &session.to_string_lossy())?
            }
            REVIEW => format!("{}\n", self.next_verdict()?),
            AGENT_JOB => Self::agent_job(command.get_stdin().unwrap_or_default()),
            other => bail!("the wiring test's agent starts no {other}"),
        };
        match streams {
            Streams::Files {
                stdout: out,
                stderr,
            } => {
                self.files.write(out, stdout.as_bytes())?;
                self.files.write(stderr, b"")?;
            }
            Streams::Log(log) => self.files.write(log, stdout.as_bytes())?,
            Streams::Inherit | Streams::Null => {}
        }
        let mut started = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        started.push(program);
        Ok(Box::new(Ended {
            // Far from the host's pids and from the test's processes'.
            pid: 5_000_000 + u32::try_from(started.len()).unwrap_or(u32::MAX - 5_000_000),
            exit: exit(0),
        }))
    }
}
