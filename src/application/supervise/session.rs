//! A claimed run's worker session: its provisioning, the [`SessionWatch`]
//! of its wrapper, receipt and idle marker, and the answers to
//! its `worker_question` asks.

use super::*;
use crate::domain::EventKind;
use crate::domain::e2e_quarantine;
use crate::domain::language::with_instruction;

impl Supervisor<'_> {
    /// Start the validation of `run` on a thread (see [`spawn_validation`]).
    pub(super) fn validate(&self, run: TaskRun) -> thread::JoinHandle<Result<Validation>> {
        spawn_validation(
            self.queues.clone(),
            self.repository.clone(),
            self.files.clone(),
            run,
            self.e2e_paths(),
        )
    }
    /// `[e2e] paths` of `dagq.toml` (ADR-t963-1 decision 2). A file that
    /// cannot be read leaves `e2e` to the task, as without the table, with
    /// a warning: the gate before the fixed binary is replaced still runs
    /// every e2e.
    pub(super) fn e2e_paths(&self) -> Vec<String> {
        self.verifier.e2e_paths().unwrap_or_else(|error| {
            warn!("[e2e] paths of dagq.toml could not be read; e2e is required by the task only: {error:#}");
            Vec::new()
        })
    }
    /// The marks of `.config/e2e-quarantine.toml` in the landing branch's
    /// committed tree, read afresh for each e2e of a run (ADR-t1233-2
    /// decision 5, ADR-t1165-1 decision 6): neither a worker's edits nor
    /// uncommitted main checkout edits may grant an exception (task 1198).
    /// A file that cannot be read holds no mark.
    pub(super) fn main_quarantine(&self, run: &TaskRun) -> e2e_quarantine::QuarantineFile {
        let text = self
            .repository
            .main_head()
            .and_then(|head| self.repository.file_in(head.as_str(), e2e_quarantine::FILE));
        match text {
            Ok(Some(text)) => e2e_quarantine::QuarantineFile::of(&text),
            Ok(None) => e2e_quarantine::QuarantineFile::Absent,
            Err(error) => {
                warn!(run_id = %run.id(), "{} in the landing branch's committed tree could not be read, so no e2e mark holds for the run: {error:#}", e2e_quarantine::FILE);
                e2e_quarantine::QuarantineFile::Unreadable(format!("{error:#}"))
            }
        }
    }
    /// The executor every AI actor the supervisor starts goes through: its
    /// workspaces through cmux, its agents through the review provider and
    /// the spawner, on the queue's environment.
    pub(super) fn actors(&self) -> HostActorExecutor<'_> {
        self.actors_on(self.reviewer)
    }
    /// [`Self::actors`] with its agents made by `agent`: a headless job
    /// whose role runs on another provider (ADR-t1063-1).
    pub(super) fn actors_on<'s>(&'s self, agent: &'s dyn AgentProvider) -> HostActorExecutor<'s> {
        HostActorExecutor::new(&self.layout.db)
            .with_no_claude(self.no_claude)
            .with_workspaces(self.cmux)
            .with_provider(agent)
            .with_spawner(self.spawner)
            .with_queue_service(self.service_access)
    }
    /// Plan paths, create the run directory, worktree and workspace. Any
    /// error leaves what was created for inspection.
    /// The queue's workspace group, asked for with every run workspace:
    /// the call is idempotent by external ID, and cmux removes a group whose
    /// last workspace closes, so a handle kept from an earlier run could
    /// name a group that is gone. A group cmux cannot make is a warning in
    /// the log, and the run opens outside it.
    pub(super) fn workspace_group(&self) -> Option<String> {
        let name = workspace_group_name(&self.layout.repo_root);
        match self.cmux.ensure_group(&self.layout.queue_hash, &name) {
            Ok(group) => Some(group),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "warning: cmux workspace group {name:?} (external ID {}) could not be made, \
so the run workspace opens outside it: {error:#}", self.layout.queue_hash);
                None
            }
        }
    }
    /// What `run` inherits from an earlier run of its task that was retried
    /// with its branch carried over (ADR-0047 decision 24): the latest such
    /// run, so that a run after it that failed early (and was retried by
    /// the triage) does not lose the carried-over work. Its own commits are
    /// counted from their merge base with `run`'s base, the current main.
    fn inheritance(&mut self, run: &TaskRun) -> Result<Option<Inheritance>> {
        let runs = self.queue.show(run.task_id())?.runs;
        for previous in runs
            .iter()
            .take_while(|other| other.id() != run.id())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let events = self.queue.run_events(previous.id())?;
            if let Some(mut inherited) = Inheritance::of(&*self.files, previous, &events) {
                if let Ok(Some(base)) = self
                    .repository
                    .merge_base(run.base_commit().as_str(), &inherited.head)
                {
                    inherited.base = base;
                }
                return Ok(Some(inherited));
            }
        }
        Ok(None)
    }
    /// Write `run`'s prompt (`prompt.txt` in `run_dir`) for its task, as
    /// its worker's route and provider take it; the branch it inherits, if
    /// any. Written again when the run moves to the other provider
    /// (ADR-t813-2), whose worker is told otherwise.
    pub(super) fn write_prompt(
        &mut self,
        task: &crate::domain::Task,
        run: &TaskRun,
        run_dir: &Path,
    ) -> Result<Option<Inheritance>> {
        let predecessors: Vec<PredecessorSummary> = self
            .queue
            .predecessors(task.id())?
            .iter()
            .map(|predecessor| PredecessorSummary::from_predecessor(&*self.files, predecessor))
            .collect();
        let goal_predecessors: Vec<GoalPredecessorSummary> = self
            .queue
            .goal_predecessors(task.id())?
            .iter()
            .map(|goal| GoalPredecessorSummary::from_goal_predecessor(&*self.files, goal))
            .collect();
        let goal = match task.goal_id() {
            Some(goal_id) => Some(self.queue.show_goal(goal_id)?.goal),
            None => None,
        };
        let siblings = siblings_in_progress(task, self.queue.tasks_in_progress()?);
        let inherited = self.inheritance(run)?;
        let mut text = prompt(
            task,
            run,
            goal.as_ref(),
            &predecessors,
            &goal_predecessors,
            &siblings,
            inherited.as_ref(),
            &self.e2e_paths(),
        )?;
        // A Claude worker the supervisor gave the broker's tools is told of
        // them (ADR-t827-4 decision 1).
        if run.actual_provider() == Provider::Claude
            && self
                .files
                .exists(&crate::application::broker_run::mcp_config_path(run_dir))
        {
            text.push_str(crate::application::prompt::BROKER_TOOLS);
        }
        self.files.write(
            &run_dir.join("prompt.txt"),
            with_instruction(text, self.verifier.language().as_ref()).as_bytes(),
        )?;
        Ok(inherited)
    }
    pub(super) fn provision(&mut self, claimed: &TaskRun) -> Result<SessionWatch> {
        let state_dir = &self.layout.runs_dir;
        let paths = RunPaths::new(state_dir, claimed.id());
        let run_dir = paths.run_dir.clone();
        let plan = RunPlan {
            repo_path: path_text(&self.layout.repo_root)?,
            run_dir: path_text(&run_dir)?,
            branch: format!("dagq/{}", claimed.id()),
            worktree_path: path_text(&paths.worktree)?,
            receipt_path: path_text(&paths.receipt)?,
            log_path: path_text(&paths.log)?,
        };
        // Save intended paths before any external resource is created.
        self.queue.plan_run(claimed.id(), &self.token, &plan)?;
        self.files.create_dir_all(state_dir)?;
        self.files
            .create_new_dir(&run_dir)
            .context("run directory must be new")?;
        let run_env = self.verifier.run_env(&run_dir)?;
        let run = self.queue.run(claimed.id())?;
        // A Codex worker's turns run in Codex's sandbox: the server they
        // build through is the supervisor's (ADR-t1215-1).
        if run.actual_provider() == crate::domain::Provider::Codex {
            self.ensure_sccache(crate::domain::sccache::CheckReason::BeforeWorker);
        }
        let task = self.queue.show(run.task_id())?.task;
        let inherited = self.write_prompt(&task, &run, &run_dir)?;
        if let Some(inherited) = &inherited {
            self.queue.record_runtime_event(
                run.id(),
                EventKind::RunInherited,
                json!({"inherit_from_run": inherited.run_id, "head": inherited.head, "branch": inherited.branch}),
            )?;
            info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} carries run {}'s work over from {}", run.id(), run.task_id(), inherited.run_id, inherited.head);
        }
        // A running wrapper must not change when the development binary is rebuilt.
        self.files
            .copy(&self.layout.runner, &run_dir.join(RUN_RUNNER_FILE))
            .context("snapshot runtime binary")?;
        self.prepare_turns(&run, &run_dir)?;
        let git_output = self.repository.create_worktree(&run)?;
        self.files
            .write(&run_dir.join("worktree-create.txt"), git_output.as_bytes())?;
        self.queue.record_runtime_event(
            run.id(),
            EventKind::WorktreeCreated,
            json!({"path": plan.worktree_path, "branch": plan.branch}),
        )?;
        // The broker's tools (`preferred`): the worker is told of them in
        // its prompt, written again with them.
        let granted = self.broker_grant(&run);
        if granted {
            self.write_prompt(&task, &run, &run_dir)?;
        }
        let background = self.background_log(&run, &run_dir, None, false);
        let command = background::wrapper_command(
            vec![
                path_text(&run_dir.join(RUN_RUNNER_FILE))?,
                "--db".into(),
                path_text(&self.layout.db)?,
                "session".into(),
                "--run".into(),
                run.id().to_string(),
                "--lease".into(),
                self.token.to_string(),
                "--claude".into(),
                path_text(&self.layout.claude)?,
                "--codex".into(),
                path_text(&self.layout.codex)?,
            ],
            background.as_deref(),
        );
        let workspace = self
            .actors()
            .spawn(ActorExecutionSpec::new(
                ActorContext::worker(run.id(), run.task_id()),
                WorkspaceAccess::Write(paths.worktree.clone()),
                ActorProgram::RunWorkspace {
                    task: &task,
                    run: &run,
                    wrapper: command,
                    resume: false,
                    description: workspace_description(
                        SessionRole::Worker,
                        &self.layout.queue_hash,
                        Some(run.id()),
                        Some(run.task_id()),
                    ),
                    group: self.session_group(background.as_deref()),
                    run_env,
                    background: background.as_deref(),
                },
            ))?
            .workspace()?;
        if let Err(error) = self
            .queue
            .workspace_created(run.id(), &self.token, &workspace)
        {
            // Unrecorded, the workspace would be left open with nothing to
            // find it by, and its wrapper is refused (task 806).
            return Err(match self.cmux.close(&workspace) {
                Ok(()) => error.context(format!(
                    "the workspace {workspace} of run {} could not be recorded and was closed",
                    run.id()
                )),
                Err(close) => error.context(format!(
                    "the workspace {workspace} of run {} could not be recorded, and closing it failed: {close:#}",
                    run.id()
                )),
            });
        }
        self.record_launch(&run, &workspace, background.as_deref())?;
        info!(task_id = %run.task_id(), run_id = %run.id(), "task {} running in workspace {}; run {}", run.task_id(), workspace, run.id());
        Ok(SessionWatch {
            workspace,
            run_dir,
            receipt_path: PathBuf::from(plan.receipt_path),
            idle_marker: run.idle_marker_path()?,
            startup: Instant::now(),
            receipt_seen: false,
            receipt_seen_at: None,
            exit_requested: None,

            first_commit_seen: false,

            silent: false,
            exit_for_silence: false,
            answer_start: None,
            stall: Box::default(),
            stale: None,
            recovery: RecoveryWatch::default(),
            input_at: None,
            answered_at: None,
            asks_from: 0,
        })
    }
}

/// Watches one session: wrapper registration and heartbeat, receipt and idle
/// marker, the single exit request, and the wrapper's exit.
pub(super) struct SessionWatch {
    pub(super) workspace: String,
    pub(super) run_dir: PathBuf,
    pub(super) receipt_path: PathBuf,
    pub(super) idle_marker: PathBuf,
    pub(super) startup: Instant,
    pub(super) receipt_seen: bool,
    /// When this supervisor first saw the receipt, for the wait on
    /// background work the session left running after it.
    pub(super) receipt_seen_at: Option<Instant>,
    pub(super) exit_requested: Option<Instant>,
    /// `first_commit_observed` is recorded (also by a previous supervisor).
    pub(super) first_commit_seen: bool,
    /// The wrapper went silent while its process lived on
    /// (`wrapper_heartbeat_expired` is recorded); cleared when its
    /// heartbeat comes back before any `/exit` (task 606).
    pub(super) silent: bool,
    /// The `/exit` was sent because of that silence.
    pub(super) exit_for_silence: bool,
    /// Whether the session took the last answer delivered or the nudge
    /// (task 285).
    pub(super) answer_start: Option<SystemTime>,
    /// Idle without a receipt: the nudge and the `stalled` ask (ADR-0043
    /// decision 1).
    pub(super) stall: Box<StallWatch>,
    /// Idle with a receipt for an older commit: the one request to rewrite
    /// it (task 357).
    pub(super) stale: Option<StaleNudge>,
    /// Background work past its threshold: the recovery job (ADR-0047
    /// decision 39).
    pub(super) recovery: RecoveryWatch,
    /// The last input typed into a session that had gone idle before (a
    /// revise request, or an answer typed during it): an idle marker no
    /// newer than this is from before it, so the session works (task 238).
    /// `None` for a worker's own session, where any idle marker counts
    /// but for one from before the last answer typed (`answered_at`).
    pub(super) input_at: Option<SystemTime>,
    pub(super) answered_at: Option<SystemTime>,
    /// The `worker_question`s this watch follows (their answers typed, the
    /// session waiting on them) are those created at or after this (unix
    /// seconds): for a revise or a conflict request, when it was sent; the
    /// asks from before it are the inbox's to deliver by hand (task 582).
    /// 0 follows every ask of the run.
    pub(super) asks_from: i64,
}

impl SessionWatch {
    /// The watch of a live session asked at `input_at` to fix what its
    /// review or a conflict named ([`ReviseWatch`]), or what parked its run
    /// ([`ResumeWatch`]): only the answers of its `worker_question`s and its
    /// recovery are followed (task 238, ADR-0071 decision 17).
    pub(super) fn fixing(run: &TaskRun, workspace: &str, input_at: SystemTime) -> Result<Self> {
        Ok(SessionWatch {
            workspace: workspace.to_owned(),
            run_dir: PathBuf::from(run.run_dir().context("missing run directory")?),
            receipt_path: PathBuf::from(run.receipt_path().context("missing receipt path")?),
            idle_marker: run.idle_marker_path()?,
            startup: Instant::now(),
            receipt_seen: false,
            receipt_seen_at: None,
            exit_requested: None,

            first_commit_seen: true,

            silent: false,
            exit_for_silence: false,
            answer_start: None,
            stall: Box::default(),
            stale: None,
            recovery: RecoveryWatch::default(),
            input_at: Some(input_at),
            answered_at: None,
            asks_from: 0,
        })
    }

    pub(super) fn adopt(
        &mut self,
        queue: &dyn Queue,
        run: &TaskRun,
        _anchor: EventId,
    ) -> Result<()> {
        self.recovery = RecoveryWatch::adopt(queue, run)?;
        Ok(())
    }

    fn idle_after_receipt(&self, sv: &Supervisor<'_>) -> Result<Option<IdleMarker>> {
        sv.session_idle(&self.idle_marker)
    }

    /// One observation. `Some` once supervision finished (`validating` or
    /// `failed`): the wrapper exited, or the session went idle after its
    /// receipt and stays open for the review; an error means the run must
    /// be retained. An `/exit` an earlier supervisor already requested is
    /// waited out as before.
    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<TaskRun>> {
        let processes = sv.queue.processes(run.id())?;
        self.watch_first_commit(sv, run)?;
        if !self.receipt_seen && sv.files.is_file(&self.receipt_path) {
            self.receipt_seen = true;
            self.receipt_seen_at = Some(Instant::now());
            // The work interval ends here (`stats`' `work`); its load goes
            // with it (task 197).
            let mut payload = json!({"path": path_text(&self.receipt_path)?, "validated": false});
            if let Value::Object(load) = serde_json::to_value(sv.take_load(run.id()))? {
                payload.as_object_mut().expect("an object").extend(load);
            }
            sv.queue
                .record_runtime_event(run.id(), EventKind::ReceiptObserved, payload)?;
            info!(run_id = %run.id(), "receipt received for {}; waiting for the session to go idle (or a person's /exit)", run.id());
            // A send it had not taken ends with the receipt.
        }
        self.stall.settle(sv, run, self.receipt_seen)?;
        self.stop_idle_job(sv, run, true);
        let wrapper = processes.iter().find(|p| p.role == "wrapper");
        // A session that already ended (on its own, by a person's /exit,
        // or before this supervisor adopted the run) is not asked to exit.
        let session_ended = wrapper.is_some_and(|w| w.exited_at.is_some());
        // A fast session, or a wrapper slowed by load, can see the receipt and
        // the idle before the wrapper registers its agent: the run is still
        // `starting`, and taking it to validation would refuse that
        // registration and end the session (task 1274).
        let agent_registered = processes.iter().any(|p| p.role == "agent");
        // Background work the session left running after its receipt is
        // waited for up to the resume timeout, like a resumed session's:
        // work that never ends must not hold the run without an attention.
        // Past it validation can proceed; any later shutdown uses the
        // wrapper exit request file.
        let waited_out = self
            .receipt_seen_at
            .is_some_and(|at| at.elapsed() >= sv.cmux.resume_timeout());
        if self.receipt_seen
            && self.exit_requested.is_none()
            && !session_ended
            && agent_registered
            && let Some(evidence) = match self.idle_after_receipt(sv)? {
                // The session has not answered the request to rewrite
                // its receipt yet: waited for up to the resume timeout,
                // like background work after the receipt.
                Some(idle)
                    if self.stale.is_some_and(|n| {
                        !n.answered_by(idle.modified()) && !n.waited_out(&*sv.files, sv.cmux)
                    }) =>
                {
                    None
                }
                Some(idle) if waited_out => {
                    idle.stopped_after_receipt(&*sv.files, &self.receipt_path)?
                }
                Some(idle) => idle.idle_after_receipt(&*sv.files, &self.receipt_path)?,
                None => None,
            }
        {
            if self.stale_receipt(sv, run)? {
                return Ok(None);
            }
            sv.queue
                .record_runtime_event(run.id(), EventKind::SessionIdleObserved, evidence)?;
            // The session stays open through validation and review, and
            // is asked to exit only once the verdict is known (ADR-0027
            // decision 1).
            info!(run_id = %run.id(), "session of {} is idle after its receipt; validating with the session open", run.id());
            self.recovery.stop(sv, run);
            return sv
                .queue
                .finish_supervision_live(run.id(), &sv.token)
                .map(Some);
        }
        // A lost session no attempt could open again (task 1372) ends as
        // one that exited, with its lost wrapper's code: the attempts
        // forgot its processes.
        let lost_exit = wrapper
            .is_none()
            .then(|| sv.reopens.get(run.id()).and_then(|r| r.lost_exit()))
            .flatten();
        // The receipt is read once the session ended, whatever order its
        // receipt and its exit were seen in: one a turn wrote before it
        // failed goes to validation (ADR-t1594-1).
        if let Some(code) = lost_exit {
            sv.reopens.remove(run.id());
            self.end_exited(sv, run, false)?;
            let receipt = sv.files.is_file(&self.receipt_path);
            return sv
                .queue
                .finish_lost_session(run.id(), &sv.token, code, receipt)
                .map(Some);
        }
        if let Some(wrapper) = wrapper {
            if wrapper.exited_at.is_some() {
                sv.reopens.remove(run.id());
                self.end_exited(sv, run, true)?;
                let receipt = sv.files.is_file(&self.receipt_path);
                return sv
                    .queue
                    .finish_supervision(run.id(), &sv.token, receipt)
                    .map(Some);
            }
            let pulse = wrapper_pulse(
                sv,
                run,
                wrapper,
                &self.workspace,
                &mut self.silent,
                "wrapper heartbeat expired; session may still be alive",
            )?;
            match pulse {
                WrapperPulse::Silent if self.exit_requested.is_none() => {
                    // The same single /exit a finished session gets,
                    // recorded before it is sent.
                    let timeout = sv.cmux.exit_timeout();
                    sv.queue.record_runtime_event(
                        run.id(),
                        EventKind::ExitRequested,
                        json!({"workspace_id": self.workspace, "timeout_secs": timeout.as_secs()}),
                    )?;
                    let workspace = self.workspace.clone();
                    submit(sv, run, &workspace, Input::Exit, "/exit")?;
                    info!(run_id = %run.id(), "exit requested for {} after its wrapper went silent; waiting for session exit", run.id());
                    self.exit_requested = Some(Instant::now());
                    self.exit_for_silence = true;
                    // Background work, idle processes and a stall are
                    // followed only before the /exit.
                    for alert in [RecoveryAlert::IdleProcess, RecoveryAlert::Stalled] {
                        self.recovery
                            .stop_for(sv, run, Some(alert), "exit_requested");
                    }
                }
                WrapperPulse::Silent => (),
                WrapperPulse::Exited => return Ok(None),
                WrapperPulse::Fresh => {
                    // A reopen is over once the run's wrapper lives in its
                    // slot (task 1372).
                    if sv.reopens.get(run.id()).is_some_and(|r| r.settled()) {
                        sv.reopens.remove(run.id());
                    }
                    if self.exit_requested.is_none() {
                        // A silence that ended before any /exit is over:
                        // the session may wait again, and a later silence
                        // is recorded again (task 606).
                        self.silent = false;
                        if let Some(typed) = self.deliver_answers(sv, run)? {
                            self.answered_at = Some(typed);
                        }
                        // A login or usage limit a person fixed: the
                        // session is told to go on (task 437).
                        if let Some(typed) = self.continue_after_hold(sv, run)? {
                            self.input_at = self.input_at.map(|_| typed);
                        }
                        if !self.receipt_seen
                            && let Some(start) = self.watch_stall(sv, run)?
                        {
                            self.answer_start = Some(start);
                        }
                        // A recovery job's `resume` parks the run for a
                        // session of its own (task 442).
                        if let Some(instruction) = self.stall.take_park() {
                            return self.park_for_resume(sv, run, &instruction).map(Some);
                        }
                        self.watch_idle_processes(sv, run)?;
                    }
                }
            }
        } else {
            let timeout = sv.cmux.registration_timeout();
            ensure!(
                self.startup.elapsed() < timeout,
                "wrapper did not register within {} seconds",
                timeout.as_secs()
            );
        }
        Ok(None)
    }

    fn end_exited(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun, _capture: bool) -> Result<()> {
        self.stall.ended(sv, run)?;
        if let Some(nudge) = &mut self.stale {
            nudge.settle(sv, run, SESSION_PHASE, None, "run_ended")?;
        }
        self.recovery.stop(sv, run);
        Ok(())
    }

    /// The session went idle after its receipt: ask it once to rewrite a
    /// receipt that names an older commit than its clean HEAD (task 357),
    /// and `true` while that request waits for its answer. Once answered,
    /// how it ended is recorded and the run goes on either way.
    fn stale_receipt(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<bool> {
        if let Some(nudge) = &mut self.stale {
            let rewritten = nudge.rewritten(&*sv.files, &self.receipt_path);
            let outcome = if rewritten { "rewritten" } else { "unchanged" };
            nudge.settle(sv, run, SESSION_PHASE, None, outcome)?;
            return Ok(false);
        }
        let Some(stale) = stale_receipt(sv, run) else {
            return Ok(false);
        };
        let workspace = self.workspace.clone();
        let Some((nudge, start)) =
            nudge_stale_receipt(sv, run, &workspace, SESSION_PHASE, None, &stale)?
        else {
            return Ok(false);
        };
        self.stale = Some(nudge);
        self.answer_start = Some(start);
        Ok(true)
    }

    /// Record `first_commit_observed` once, the first time the worktree's
    /// HEAD is seen away from the run's base commit: with `agent_started` it
    /// measures how long a session takes to start working (`stats`'s
    /// `startup`). The time is when this poll saw it, at most a tick late.
    /// A HEAD that cannot be read is noted and checked again next poll.
    pub(super) fn watch_first_commit(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<()> {
        if self.first_commit_seen {
            return Ok(());
        }
        let Some(worktree) = run.worktree_path() else {
            return Ok(());
        };
        let head = match sv.repository.head(Path::new(worktree)) {
            Ok(head) => head,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "HEAD of {} could not be read for its first commit: {error:#}", run.id());
                return Ok(());
            }
        };
        if head != *run.base_commit() {
            sv.queue.record_runtime_event(
                run.id(),
                EventKind::FirstCommitObserved,
                json!({"commit": head, "base_commit": run.base_commit()}),
            )?;
            self.first_commit_seen = true;
        }
        Ok(())
    }

    /// Queue the answer of each answered `worker_question` as a turn,
    /// prefixed `answer to ask <id>:`, once the worker
    /// went idle after asking (its idle marker is no older than the ask, to
    /// the second), then close the ask and record `ask_delivered` (ADR-0022
    /// decision 2). Each answer is sent at most once: a failed send records
    /// `ask_delivery_failed` and leaves the ask unclosed for the inbox.
    /// Only the asks this watch follows are typed
    /// ([`SessionWatch::asks_from`]). Returns when the last answer sent was
    /// typed, if one was.
    pub(super) fn deliver_answers(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<SystemTime>> {
        let mut answers = sv.queue.undelivered_answers(run.id())?;
        answers.retain(|ask| ask.created_at >= self.asks_from);
        if answers.is_empty() {
            return Ok(None);
        }
        let events = sv.queue.run_events(run.id())?;
        let failed = RunHistory::from_events(&events).failed_deliveries();
        answers.retain(|ask| !failed.contains(&ask.id));
        if answers.is_empty() {
            return Ok(None);
        }
        // A headless session between turns takes the answer as its next
        // turn, whether the ask opened before or after its turn ended.
        let between = between_turns(sv, &self.idle_marker, &events);
        // Background work does not hold an answer back: typing into the
        // prompt opens no dialog, only /exit does.
        let idle_at = match sv.files.modified(&self.idle_marker) {
            Ok(_) if between => Some(i64::MAX),
            Ok(modified) => Some(unix_seconds(modified)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("inspect idle marker"),
        };
        let Some(idle_at) = idle_at else {
            return Ok(None);
        };
        let mut typed = None;
        for ask in answers {
            if idle_at < ask.created_at {
                continue;
            }
            let text = answer_text(run, ask.id, ask.answer.as_deref().unwrap_or_default());
            let what = format!("answer of ask {}", ask.id);
            let sent_at = sv.files.now();
            let workspace = self.workspace.clone();
            match submit(sv, run, &workspace, Input::Text(&text), &what) {
                // Sent: failing to record it must not cost the live run its
                // lease, so it is only noted (the ask then shows unclosed).
                Ok(_submission) => {
                    typed = Some(sent_at);
                    self.stall.input_sent(sent_at, Some(&text));
                    self.answer_start = Some(sent_at);
                    match sv.queue.ask_delivered(ask.id, &self.workspace) {
                        Ok(_) => {
                            info!(ask_id = %ask.id, run_id = %run.id(), "answer of ask {} sent to run {} in workspace {}", ask.id, run.id(), self.workspace)
                        }
                        Err(error) => {
                            warn!(ask_id = %ask.id, run_id = %run.id(), error = %format_args!("{error:#}"), "answer of ask {} was sent to run {} but could not be recorded: {error:#}", ask.id, run.id())
                        }
                    }
                }
                Err(error) => {
                    sv.queue.record_runtime_event(
                        run.id(),
                        EventKind::AskDeliveryFailed,
                        reason_of_error(&error, ReasonCode::BackendFailed).on(json!({
                            "ask_id": ask.id,
                            "workspace_id": self.workspace,
                            "error": format!("{error:#}"),
                        })),
                    )?;
                    warn!(ask_id = %ask.id, run_id = %run.id(), error = %format_args!("{error:#}"), "answer of ask {} could not be sent to run {} in workspace {}: {error:#}; it is left to the inbox", ask.id, run.id(), self.workspace);
                }
            }
        }
        Ok(typed)
    }
}
