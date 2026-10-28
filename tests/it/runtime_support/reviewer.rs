//! The stand-in for the headless reviewer of the runtime tests.
use super::*;

/// Stands in for the headless reviewer (ADR-0027): each review runs the
/// next script with `/bin/sh -c` in the worktree (the last one repeats) and
/// records its prompt; `timeout` is the review timeout.
pub struct TestReviewer {
    pub scripts: Mutex<Vec<String>>,
    pub prompts: Mutex<Vec<String>>,
    pub timeout: Duration,
    /// Scripts of the headless recovery jobs, one per job in order; without
    /// one left, a job cannot start.
    pub triages: Mutex<Vec<String>>,
    /// The recovery job prompts and the directories they ran in.
    pub triage_prompts: Mutex<Vec<(String, PathBuf)>>,
    /// The model and effort each job was given (ADR-0079 decision 7), in
    /// order; a job started as before gives none.
    pub models: Mutex<Vec<(String, String)>>,
    /// The queue and the backend's stands: as each recovery job starts, the
    /// run it recovers gets a session still running (a process standing for
    /// a wrapper that did not end with its agent), which the backend stops
    /// on its close, so the round's end has a wrapper to stop.
    pub leave_running: Option<(PathBuf, Stands)>,
    /// Scripts of the agent jobs of the eval (ADR-t1728-1), each run with
    /// `/bin/sh -c` in the case's tree (the last one repeats); without
    /// one, an agent job cannot start.
    pub agent_jobs: Mutex<Vec<String>>,
    /// Scripts of the agent jobs of runs' reviews (ADR-t1895-1), by the
    /// agent their prompt names, each run with `/bin/sh -c` in the
    /// worktree (an agent's last one repeats); an agent without them takes
    /// `agent_jobs`', and a built-in agent (`builtin:<name>`, which runs
    /// by default on a change of code) without them passes.
    pub review_agents: Mutex<Vec<(String, Vec<String>)>>,
    /// Each agent job's launch as it started, the eval's and the reviews'.
    pub launches: Mutex<Vec<dagq::application::AgentJobLaunch>>,
}

impl TestReviewer {
    pub fn new(scripts: &[String]) -> Self {
        Self {
            scripts: Mutex::new(scripts.to_vec()),
            prompts: Mutex::new(Vec::new()),
            timeout: Duration::from_secs(60),
            triages: Mutex::new(Vec::new()),
            triage_prompts: Mutex::new(Vec::new()),
            models: Mutex::new(Vec::new()),
            leave_running: None,
            agent_jobs: Mutex::new(Vec::new()),
            review_agents: Mutex::new(Vec::new()),
            launches: Mutex::new(Vec::new()),
        }
    }
    /// See `review_agents`.
    pub fn with_review_agents(self, scripts: &[(&str, Vec<String>)]) -> Self {
        *self.review_agents.lock().unwrap() = scripts
            .iter()
            .map(|(agent, scripts)| ((*agent).to_owned(), scripts.clone()))
            .collect();
        self
    }
    /// See `launches`.
    pub fn launches(&self) -> Vec<dagq::application::AgentJobLaunch> {
        self.launches.lock().unwrap().clone()
    }
    /// See `agent_jobs`.
    pub fn with_agent_jobs(self, scripts: &[String]) -> Self {
        *self.agent_jobs.lock().unwrap() = scripts.to_vec();
        self
    }
    /// See `leave_running`.
    pub fn leaving_sessions_running(mut self, db: &Path, backend: &TestWorkspace) -> Self {
        self.leave_running = Some((db.to_owned(), backend.stands.clone()));
        self
    }
    pub fn models(&self) -> Vec<(String, String)> {
        self.models.lock().unwrap().clone()
    }
    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
    pub fn with_triages(self, scripts: &[String]) -> Self {
        *self.triages.lock().unwrap() = scripts.to_vec();
        self
    }
    pub fn triage_prompts(&self) -> Vec<(String, PathBuf)> {
        self.triage_prompts.lock().unwrap().clone()
    }
}

impl AgentProvider for TestReviewer {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    // A run that fails under these tests is recovered by this provider too:
    // with no script left, a live session's recovery job escalates, and one
    // for a run that ended cannot start (it waits to be recovered by hand).
    // A goal whose tasks all landed is not reviewed by this provider: its
    // goal review cannot start, and the goal stays open (tests/it/goal_review.rs
    // plays the goal review).
    fn headless_command(&self, cwd: &Path, prompt: &str, access: JobAccess) -> Result<CommandSpec> {
        ensure!(
            !prompt.starts_with("You are the goal review"),
            "the test reviewer runs no goal review"
        );
        assert_eq!(access, runtime::TRIAGE_ACCESS);
        let mut triages = self.triages.lock().unwrap();
        if triages.is_empty() && prompt.contains(LIVE_RECOVERY) {
            triages.push(format!("printf '%s\\n' '{ESCALATE}'"));
        }
        ensure!(
            !triages.is_empty(),
            "the test reviewer has no recovery job left"
        );
        if triages[0] == UNSTARTABLE_JOB {
            triages.remove(0);
            anyhow::bail!("the test reviewer cannot start this recovery job");
        }
        if let Some((db, stands)) = &self.leave_running {
            let stand = Stand::start()?;
            let updated = Connection::open(db)?.execute(
                "UPDATE task_runs SET workspace_id=?2, workspace_closed_at=NULL WHERE run_dir=?1",
                [cwd.to_str().unwrap(), &stand.handle],
            )?;
            assert_eq!(updated, 1, "the run of {}", cwd.display());
            stands.lock().unwrap().push((stand.handle.clone(), stand));
        }
        self.triage_prompts
            .lock()
            .unwrap()
            .push((prompt.into(), cwd.into()));
        let mut command = CommandSpec::new("/bin/sh");
        command.current_dir(cwd).arg("-c").arg(triages.remove(0));
        Ok(command)
    }
    fn review_command(
        &self,
        _: &TaskRun,
        tree: &Path,
        prompt: &str,
        access: JobAccess,
    ) -> Result<CommandSpec> {
        assert_eq!(access, runtime::REVIEW_ACCESS);
        self.prompts.lock().unwrap().push(prompt.into());
        let mut scripts = self.scripts.lock().unwrap();
        let script = if scripts.len() > 1 {
            scripts.remove(0)
        } else {
            scripts[0].clone()
        };
        ensure!(
            script != UNSTARTABLE_REVIEW,
            "the test reviewer cannot start this review"
        );
        let mut command = CommandSpec::new("/bin/sh");
        command.current_dir(tree).arg("-c").arg(script);
        Ok(command)
    }
    fn agent_job_command(&self, job: &dagq::application::AgentJobLaunch) -> Result<CommandSpec> {
        self.launches.lock().unwrap().push(job.clone());
        let named = job
            .prompt
            .lines()
            .find_map(|line| line.strip_prefix("You are the review agent `"))
            .and_then(|rest| rest.split('`').next())
            .map(str::to_owned);
        let mut by_agent = self.review_agents.lock().unwrap();
        let mine = by_agent
            .iter_mut()
            .find(|(agent, _)| Some(agent) == named.as_ref())
            .map(|(_, scripts)| scripts);
        let mut eval = self.agent_jobs.lock().unwrap();
        let mut builtin_passes = Vec::new();
        let scripts = match (mine, &named) {
            (Some(scripts), _) => scripts,
            (None, Some(agent)) if agent.starts_with("builtin:") => {
                builtin_passes.push(agent_says(agent, "pass", &[]));
                &mut builtin_passes
            }
            (None, _) => &mut *eval,
        };
        ensure!(!scripts.is_empty(), "the test reviewer runs no agent job");
        let script = if scripts.len() > 1 {
            scripts.remove(0)
        } else {
            scripts[0].clone()
        };
        let mut command = CommandSpec::new("/bin/sh");
        command
            .current_dir(&job.cwd)
            .arg("-c")
            .arg(script)
            .stdin(job.prompt.as_str());
        Ok(command)
    }
    fn review_timeout(&self) -> Duration {
        self.timeout
    }
    fn select_model(&self, _: &mut CommandSpec, model: &str, effort: &str) {
        self.models
            .lock()
            .unwrap()
            .push((model.into(), effort.into()));
    }
}

/// A reviewer script whose review cannot start: `review_command` fails, so
/// no job runs and writes `review-N.out` / `.err`.
pub const UNSTARTABLE_REVIEW: &str = "<unstartable review>";

/// A recovery job's script whose job cannot start: `headless_command`
/// fails, so no provider's process starts.
pub const UNSTARTABLE_JOB: &str = "<unstartable job>";

/// A script of an agent job of a run's review: `agent`'s one completed
/// result of `decision` for `reasons`.
pub fn agent_says(agent: &str, decision: &str, reasons: &[&str]) -> String {
    agent_answers(
        json!({"agent": agent, "status": "completed", "verdict": decision,
                         "reasons": reasons, "summary": format!("{agent} looked")}),
    )
}

/// A script of an agent job that prints `result` as it is.
pub fn agent_answers(result: Value) -> String {
    format!("printf '%s\\n' '{result}'")
}

/// A reviewer script that prints the verdict JSON.
pub fn verdict(decision: &str, reasons: &[&str], summary: &str) -> String {
    let json = json!({"verdict": decision, "reasons": reasons, "summary": summary});
    format!("printf '%s\\n' '{json}'")
}
