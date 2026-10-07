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
    /// Whether its reviews can run the review's subagents (ADR-t1453-1).
    pub runs_subagents: bool,
    /// The subagents each review that required them was handed, by name
    /// and description, in order.
    pub handed: Mutex<Vec<Vec<(String, String)>>>,
    /// The queue and the backend's stands: as each recovery job starts, the
    /// run it recovers gets a session still running (a process standing for
    /// a wrapper that did not end with its agent), which the backend stops
    /// on its close, so the round's end has a wrapper to stop.
    pub leave_running: Option<(PathBuf, Stands)>,
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
            runs_subagents: true,
            handed: Mutex::new(Vec::new()),
            leave_running: None,
        }
    }
    /// See `leave_running`.
    pub fn leaving_sessions_running(mut self, db: &Path, backend: &TestWorkspace) -> Self {
        self.leave_running = Some((db.to_owned(), backend.stands.clone()));
        self
    }
    /// The subagents each review was handed (see `handed`).
    pub fn handed(&self) -> Vec<Vec<(String, String)>> {
        self.handed.lock().unwrap().clone()
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
        run: &TaskRun,
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
        command
            .current_dir(run.worktree_path().unwrap())
            .arg("-c")
            .arg(script);
        Ok(command)
    }
    fn runs_review_subagents(&self) -> bool {
        self.runs_subagents
    }
    fn review_subagents(
        &self,
        _: &mut CommandSpec,
        agents: &[dagq::domain::review_subagents::AgentDefinition],
    ) -> Result<()> {
        ensure!(self.runs_subagents, "the test reviewer runs no subagents");
        self.handed.lock().unwrap().push(
            agents
                .iter()
                .map(|a| (a.name.clone(), a.description.clone()))
                .collect(),
        );
        Ok(())
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

/// A reviewer script that prints the verdict JSON.
pub fn verdict(decision: &str, reasons: &[&str], summary: &str) -> String {
    let json = json!({"verdict": decision, "reasons": reasons, "summary": summary});
    format!("printf '%s\\n' '{json}'")
}
