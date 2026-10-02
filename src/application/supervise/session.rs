//! A claimed run's worker session: its provisioning, the [`SessionWatch`]
//! of its wrapper, receipt, idle marker and dialogs, and the answers to
//! its `worker_question` asks.

use super::*;
use crate::domain::EventKind;
use crate::domain::e2e_quarantine;
use crate::domain::exit::CAUSE_EXIT_TIMEOUT;
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
        let command = shell_join(&[
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
        ]);
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
                    group: self.workspace_group(),
                    run_env,
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
            exit_timed_out: false,
            exit_retry: Box::default(),
            first_commit_seen: false,
            agent_seen: None,
            prompt_checked: None,
            prompt_hash: None,
            exit_asked: false,
            silent: false,
            exit_for_silence: false,
            answer_start: None,
            stall: StallWatch::default(),
            stale: None,
            recovery: RecoveryWatch::default(),
            input_at: None,
            answered_at: None,
            asks_from: 0,
            stage: Stage::Session,
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
    /// `exit_request_timed_out` is recorded once per run; the lease is kept.
    pub(super) exit_timed_out: bool,
    /// The retries of the `/exit` after its timeout (ADR-0047 decision 25),
    /// boxed to keep the phase small.
    pub(super) exit_retry: Box<ExitRetry>,
    /// `first_commit_observed` is recorded (also by a previous supervisor).
    pub(super) first_commit_seen: bool,
    /// When this supervisor first saw the agent registered.
    pub(super) agent_seen: Option<Instant>,
    /// When the screen was last read for a dialog.
    pub(super) prompt_checked: Option<Instant>,
    /// `screen_hash` of the dialog last recorded as `prompt_waiting` and not
    /// cleared since.
    pub(super) prompt_hash: Option<Box<str>>,
    /// The `stuck_exit` ask of the exit timeout is registered (also by a
    /// previous supervisor).
    pub(super) exit_asked: bool,
    /// The wrapper went silent while its process lived on
    /// (`wrapper_heartbeat_expired` is recorded); cleared when its
    /// heartbeat comes back before any `/exit` (task 606).
    pub(super) silent: bool,
    /// The `/exit` was sent because of that silence.
    pub(super) exit_for_silence: bool,
    /// Whether the session took the last answer delivered or the nudge
    /// (task 285).
    pub(super) answer_start: Option<StartCheck>,
    /// Idle without a receipt: the nudge and the `stalled` ask (ADR-0043
    /// decision 1).
    pub(super) stall: StallWatch,
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
    /// When [`SessionWatch::poll`] last typed an answer of a
    /// `worker_question` into a worker's own session: only an idle marker
    /// newer than it counts for [`SessionWatch::marked_idle`], so the
    /// screen is read for a dialog after the answer (task 870). Kept apart
    /// from `input_at`, whose `None` marks a worker's own session (the park
    /// of its recovery job, the move to headless Codex). A watch of a
    /// revise or a resume types answers into `input_at` instead.
    pub(super) answered_at: Option<SystemTime>,
    /// The `worker_question`s this watch follows (their answers typed, the
    /// session waiting on them) are those created at or after this (unix
    /// seconds): for a revise or a conflict request, when it was sent; the
    /// asks from before it are the inbox's to deliver by hand (task 582).
    /// 0 follows every ask of the run.
    pub(super) asks_from: i64,
    /// The stage the session is watched in, as its `idle_inferred` names
    /// it.
    pub(super) stage: Stage,
}

/// The stage a [`SessionWatch`] watches its session in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    Session,
    Resume,
    Revise,
}

impl Stage {
    /// Its `phase`, as the stage's own events name it.
    pub(super) const fn phase(self) -> &'static str {
        match self {
            Self::Session => SESSION_PHASE,
            Self::Resume => RESUME_PHASE,
            Self::Revise => REVISE_PHASE,
        }
    }
}

/// Whether the idle marker at `idle_marker` exists and, when `at` (the last
/// input typed) is known, was written after it, in a later millisecond
/// (task 1050).
fn marker_after(files: &dyn RunFiles, idle_marker: &Path, at: Option<SystemTime>) -> bool {
    match at {
        Some(at) => files
            .modified(idle_marker)
            .is_ok_and(|modified| super::file_time::written_after(modified, at)),
        None => files.exists(idle_marker),
    }
}

impl SessionWatch {
    /// The watch of a live session asked at `input_at` to fix what its
    /// review or a conflict named ([`ReviseWatch`]), or what parked its run
    /// ([`ResumeWatch`]): only the answers of its `worker_question`s and its
    /// dialogs are followed (task 238, ADR-0071 decision 17).
    pub(super) fn fixing(
        run: &TaskRun,
        workspace: &str,
        input_at: SystemTime,
        stage: Stage,
    ) -> Result<Self> {
        Ok(SessionWatch {
            workspace: workspace.to_owned(),
            run_dir: PathBuf::from(run.run_dir().context("missing run directory")?),
            receipt_path: PathBuf::from(run.receipt_path().context("missing receipt path")?),
            idle_marker: run.idle_marker_path()?,
            startup: Instant::now(),
            receipt_seen: false,
            receipt_seen_at: None,
            exit_requested: None,
            exit_timed_out: false,
            exit_retry: Box::default(),
            first_commit_seen: true,
            agent_seen: None,
            prompt_checked: None,
            prompt_hash: None,
            exit_asked: false,
            silent: false,
            exit_for_silence: false,
            answer_start: None,
            stall: StallWatch::default(),
            stale: None,
            recovery: RecoveryWatch::default(),
            input_at: Some(input_at),
            answered_at: None,
            asks_from: 0,
            stage,
        })
    }

    /// Carry over into the watch of a revise the supervisor adopted what the
    /// previous one recorded for its session, as an adopted worker's own
    /// session does: the dialog recorded as `prompt_waiting` after `anchor`
    /// (the event that started the revise) and not cleared since, so the
    /// same screen is not recorded again and the revise's end clears it
    /// (closing its `answer_prompt` ask), and the recovery watch rebuilt
    /// from its events ([`RecoveryWatch::adopt`]) (task 581). A dialog
    /// recorded before the anchor is not carried over: some paths end one
    /// without `prompt_cleared` (task 742).
    pub(super) fn adopt(
        &mut self,
        queue: &dyn Queue,
        run: &TaskRun,
        anchor: EventId,
    ) -> Result<()> {
        let events = queue.run_events(run.id())?;
        self.prompt_hash = RunHistory::from_events(&events)
            .waiting_prompt_hash_after(anchor)
            .map(Box::from);
        self.recovery = RecoveryWatch::adopt(queue, run)?;
        Ok(())
    }

    /// Whether the receipt ended a recorded dialog whose `answer_prompt` ask
    /// the next [`SessionWatch::poll`] closes: set only between the adoption
    /// of a run with its receipt observed and that first poll (task 239).
    pub(super) fn receipt_ends_dialog(&self) -> bool {
        self.receipt_seen && self.prompt_hash.is_some()
    }

    /// Whether the session waits for the answer of a `worker_question` this
    /// watch follows ([`SessionWatch::asks_from`]).
    pub(super) fn waits_for_question(&self, sv: &Supervisor<'_>, run: &TaskRun) -> Result<bool> {
        sv.queue
            .has_unclosed_worker_question_since(run.id(), self.asks_from)
    }

    /// Whether the session is idle by its idle marker: one exists, written
    /// after the last input typed when one is known (the stage's input, or
    /// the last answer typed into a worker's own session).
    fn marked_idle(&self, sv: &Supervisor<'_>) -> bool {
        marker_after(
            &*sv.files,
            &self.idle_marker,
            self.input_at.max(self.answered_at),
        )
    }

    /// Whether, without such a marker, its screen shows it idle
    /// (ADR-t803-1). A screen that cannot be read, or a marker that
    /// cannot be read, shows nothing.
    fn screen_shows_idle(&self, sv: &Supervisor<'_>, run: &TaskRun) -> bool {
        self.screen_idle(sv, run, self.last_input())
            .is_ok_and(|idle| idle.is_some())
    }

    /// The last input the supervisor knows the session was given: the
    /// stage's request and the last text it typed.
    pub(super) fn last_input(&self) -> SystemTime {
        [self.input_at, self.stall.last_send()]
            .into_iter()
            .flatten()
            .fold(UNIX_EPOCH, SystemTime::max)
    }

    /// The idle the session's screen shows after `after` while its idle
    /// marker is missing or older than its last input
    /// ([`Supervisor::session_idle`], recorded as `idle_inferred` with the
    /// watch's phase); `None` when the marker tells, or the screen does not
    /// look idle.
    pub(super) fn screen_idle(
        &self,
        sv: &Supervisor<'_>,
        run: &TaskRun,
        after: SystemTime,
    ) -> Result<Option<IdleMarker>> {
        Ok(sv
            .session_idle(
                run,
                &self.workspace,
                &self.idle_marker,
                after,
                self.stage.phase(),
            )?
            .filter(IdleMarker::is_inferred))
    }

    /// The session's idle marker once it has its receipt, or, without a
    /// marker newer than the receipt, the last text typed into it and its
    /// input marker, the idle its screen shows (ADR-t803-1).
    fn idle_after_receipt_or_screen(
        &self,
        sv: &Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<IdleMarker>> {
        let after = [
            sv.files.modified(&self.receipt_path).ok(),
            self.input_at,
            self.stale.map(|nudge| nudge.at),
        ]
        .into_iter()
        .flatten()
        .fold(UNIX_EPOCH, SystemTime::max);
        sv.session_idle(
            run,
            &self.workspace,
            &self.idle_marker,
            after,
            SESSION_PHASE,
        )
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
        // A headless worker's asks wait in its run directory (ADR-t813-3
        // decision 3): opened before its turn's end is read.
        take_ask_requests(sv, run);
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
            self.watch_sends(sv, run)?;
        }
        // `receipt_observed` ends a recorded dialog by itself, also one an
        // adopted run recorded before its receipt, and before the session
        // goes on to validation (task 239).
        if self.receipt_ends_dialog() {
            self.prompt_hash = None;
            self.recovery.stop_for(
                sv,
                run,
                Some(RecoveryAlert::PromptWaiting),
                "dialog_cleared",
            );
            close_answer_prompt_asks(sv, run, PROMPT_RECEIPT_CLOSED)?;
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
        // Past it the run goes on, and a /exit held back by the dialog
        // becomes a stuck_exit ask.
        let waited_out = self
            .receipt_seen_at
            .is_some_and(|at| at.elapsed() >= sv.cmux.resume_timeout());
        if self.receipt_seen
            && self.exit_requested.is_none()
            && !session_ended
            && agent_registered
            && let Some(evidence) = match self.idle_after_receipt_or_screen(sv, run)? {
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
        if let Some(wrapper) = wrapper {
            if wrapper.exited_at.is_some() {
                match sv.cmux.capture(&self.workspace) {
                    Ok(screen) => sv
                        .files
                        .write(&self.run_dir.join("terminal-final.txt"), screen.as_bytes())?,
                    Err(error) => sv.queue.record_runtime_event(
                        run.id(),
                        EventKind::ScreenCaptureFailed,
                        reason_of_error(&error, ReasonCode::BackendFailed)
                            .on(json!({"error": format!("{error:#}")})),
                    )?,
                }
                // Nobody needs to send /exit to a session that exited, nor
                // answer its dialog.
                let workspace = self.workspace.clone();
                self.exit_retry.exited(sv, run, &workspace);
                for ask in sv
                    .queue
                    .close_stuck_exit_asks(run.id(), STUCK_EXIT_CLOSED)?
                {
                    info!(run_id = %run.id(), ask_id = %ask.id, "session of {} exited; closed its stuck_exit ask {}", run.id(), ask.id);
                }
                close_answer_prompt_asks(sv, run, PROMPT_EXITED_CLOSED)?;
                self.stall.ended(sv, run)?;
                self.end_sends(sv, run)?;
                if let Some(nudge) = &mut self.stale {
                    nudge.settle(sv, run, SESSION_PHASE, None, "run_ended")?;
                }
                self.recovery.stop(sv, run);
                return sv.queue.finish_supervision(run.id(), &sv.token).map(Some);
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
                    for alert in [
                        RecoveryAlert::LongBackground,
                        RecoveryAlert::IdleProcess,
                        RecoveryAlert::Stalled,
                    ] {
                        self.recovery
                            .stop_for(sv, run, Some(alert), "exit_requested");
                    }
                }
                WrapperPulse::Silent => (),
                WrapperPulse::Exited => return Ok(None),
                WrapperPulse::Fresh => {
                    if self.exit_requested.is_none() {
                        // A silence that ended before any /exit is over:
                        // the session may wait again, and a later silence
                        // is recorded again (task 606).
                        self.silent = false;
                        if let Some(typed) = self.deliver_answers(sv, run)? {
                            self.answered_at = Some(typed);
                        }
                        // A request to rewrite a stale receipt is typed
                        // after the receipt.
                        let rewrite_asked = self.stale.is_some_and(|n| !n.settled);
                        if (!self.receipt_seen || rewrite_asked)
                            && let Some(start) = &mut self.answer_start
                        {
                            start.poll(sv, run, &self.workspace, &self.idle_marker)?;
                        }
                        // A send it did not take before its receipt goes to
                        // its recovery job (ADR-0047 decision 31); a request
                        // to rewrite a stale receipt ends at its own timeout.
                        if !self.receipt_seen
                            && let Some((_, start)) = self.watch_sends(sv, run)?
                        {
                            self.answer_start = Some(start);
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
                        // An interactive session at a wall whose run moved
                        // to headless Codex (ADR-t813-2 decision 5).
                        if let Some(switch) = self.stall.take_switch() {
                            return self.park_for_switch(sv, run, &switch).map(Some);
                        }
                        self.watch_background(sv, run)?;
                        self.watch_idle_processes(sv, run)?;
                    }
                    if let Some(agent) = processes.iter().find(|p| p.role == "agent") {
                        self.watch_prompt(sv, run, agent)?;
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
        if let Some(requested) = self.exit_requested
            && !self.exit_timed_out
        {
            let timeout = sv.cmux.exit_timeout();
            let workspace = self.workspace.clone();
            if requested.elapsed() >= timeout && answer_exit_dialog(sv, run, &workspace, true)? {
                // A known dialog answered by rule gets the exit timeout
                // again (ADR-0047 decision 29).
                self.exit_requested = Some(Instant::now());
            } else if requested.elapsed() >= timeout {
                // Something in the session (for example a dialog) held the
                // /exit back. Keep the lease and keep watching: the run
                // proceeds to validation once the session exits. /exit is
                // retried only where the screen shows it safe (ADR-0047
                // decision 25); past the retries the recovery job looks at
                // it (decision 39).
                sv.queue.record_runtime_event(
                    run.id(),
                    EventKind::ExitRequestTimedOut,
                    json!({"code": ReasonCode::ExitTimeout, "workspace_id": self.workspace, "timeout_secs": timeout.as_secs()}),
                )?;
                warn!(run_id = %run.id(), "session for {} did not exit within {}s of the exit request in workspace {}; keeping the run and retrying its /exit", run.id(), timeout.as_secs(), self.workspace);
                self.exit_timed_out = true;
                self.exit_retry.start(CAUSE_EXIT_TIMEOUT);
            }
        }
        if self.exit_timed_out && !self.exit_asked {
            // A run that has not been reviewed is not closed to go on
            // (ADR-0047 decision 25): past its retries, its recovery job.
            let workspace = self.workspace.clone();
            let typed = self.exit_requested.is_some();
            if self.exit_retry.poll(sv, run, &workspace, typed)? != RetryStep::Waiting {
                self.recover_stuck_exit(sv, run)?;
            }
        }
        Ok(None)
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

    /// Read the screen of a session that has run for `prompt_wait` with
    /// neither a receipt nor an idle marker, its wrapper and agent alive, and
    /// record a dialog found there as `prompt_waiting` (once per screen) and
    /// its disappearance as `prompt_cleared`. A known dialog whose
    /// conditions hold is answered by rule instead (ADR-0047 decision 29);
    /// no other dialog gets a key. A dialog is raised to the inbox as an `answer_prompt` ask with the
    /// screen's excerpt (ADR-0024's Consequences), which the runtime closes
    /// once the dialog is gone, the receipt arrives or the session exits.
    pub(super) fn watch_prompt(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        agent: &RunProcess,
    ) -> Result<()> {
        // A headless session has no dialog to wait at (ADR-t813-1).
        if headless(run) {
            return Ok(());
        }
        let started = *self.agent_seen.get_or_insert_with(Instant::now);
        let wait = sv.cmux.prompt_wait();
        if self.receipt_seen {
            // `receipt_observed` ended the dialog ([`SessionWatch::poll`]).
            return Ok(());
        }
        // The screen is read for the idle last: a session that waits for an
        // answer is not captured.
        if self.marked_idle(sv)
            || !sv.processes.alive(agent.pid)
            || self.waits_for_question(sv, run)?
            || self.screen_shows_idle(sv, run)
        {
            // The agent finished a response, is gone, or stopped at an ask
            // that waits for its answer: no dialog holds it now, and a
            // recorded one must not stay an attention.
            return self.clear_prompt(sv, run);
        }
        // A recorded dialog (also one adopted from the previous supervisor)
        // is rechecked without waiting again, so an answer clears it soon.
        if (self.prompt_hash.is_none() && started.elapsed() < wait)
            || self
                .prompt_checked
                .is_some_and(|at| at.elapsed() < wait.min(PROMPT_CHECK_INTERVAL))
        {
            return Ok(());
        }
        self.prompt_checked = Some(Instant::now());
        let screen = match sv.cmux.capture(&self.workspace) {
            Ok(screen) => screen,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "screen of {} could not be read for a dialog: {error:#}", run.id());
                return Ok(());
            }
        };
        // A login that ran out, or the usage limit, is no dialog to answer:
        // only a person moves it, once for every session it stopped
        // (ADR-0047 decision 42).
        let workspace = self.workspace.clone();
        if let Some(wall) = sv.signals.screen_wall(&screen) {
            // A worker's own interactive Claude session moves to headless
            // Codex when it can (ADR-t813-2 decision 5); the watch parks it.
            if self.stall.parking() {
                return self.clear_prompt(sv, run);
            }
            if self.input_at.is_none()
                && !self.receipt_seen
                && let Some(switch) = interactive_switch(sv, run, wall)?
            {
                self.stall.request_switch(switch);
                return self.clear_prompt(sv, run);
            }
            if raise_wall(sv, run, &workspace, &screen, wall)? {
                return self.clear_prompt(sv, run);
            }
        }
        // A known dialog is answered by rule once its conditions hold
        // (ADR-0047 decision 29); otherwise, or once answered in vain, it is
        // raised like any other.
        if answer_known_dialog(sv, run, &workspace, &screen, false, None)? {
            return Ok(());
        }
        match sv.signals.detect_prompt(&screen) {
            Some(kind) => {
                let excerpt = sv.signals.screen_excerpt(&screen);
                let hash = format!("{:x}", Sha256::digest(excerpt.as_bytes()));
                if self.prompt_hash.as_deref() != Some(hash.as_str()) {
                    sv.queue.record_runtime_event(
                        run.id(),
                        EventKind::PromptWaiting,
                        json!({
                            "workspace_id": self.workspace,
                            "excerpt": excerpt,
                            "screen_hash": hash,
                            "prompt": kind,
                        }),
                    )?;
                    info!(run_id = %run.id(), "run {} waits at a {} dialog in workspace {}; its recovery job looks at it", run.id(), kind, self.workspace);
                    self.prompt_hash = Some(hash.into());
                }
                // A changed screen under an open ask keeps that ask, so a
                // ticking line cannot flood the inbox.
                self.recover_prompt(sv, run, kind, &excerpt)?;
            }
            None => self.clear_prompt(sv, run)?,
        }
        Ok(())
    }

    /// Type the answer of each answered `worker_question` of the run into
    /// the worker's terminal, prefixed `answer to ask <id>:`, once the worker
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
        let between = headless(run) && between_turns(sv, &self.idle_marker, &events);
        // Background work does not hold an answer back: typing into the
        // prompt opens no dialog, only /exit does.
        let idle_at = match sv.files.modified(&self.idle_marker) {
            Ok(_) if between => Some(i64::MAX),
            Ok(modified) => Some(unix_seconds(modified)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("inspect idle marker"),
        };
        let latest = answers.iter().map(|ask| ask.created_at).max().unwrap_or(0);
        // Without a marker as new as the latest ask, the screen stands in
        // for it (ADR-t803-1): idle since after the ask and the last input
        // the supervisor gave.
        let idle_at = match idle_at {
            Some(at) if at >= latest => at,
            marker => {
                let asked = UNIX_EPOCH + Duration::from_secs(u64::try_from(latest).unwrap_or(0));
                match self.screen_idle(sv, run, self.last_input().max(asked))? {
                    Some(idle) => unix_seconds(idle.modified()),
                    None => match marker {
                        Some(at) => at,
                        None => return Ok(None),
                    },
                }
            }
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
                Ok(submission) => {
                    typed = Some(sent_at);
                    self.stall.input_sent(sent_at, Some(&text));
                    self.answer_start = Some(StartCheck::new(&what, &text, sent_at, &submission));
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

    /// Record `prompt_cleared` if a dialog is recorded and not cleared yet;
    /// its recovery job, if one runs, has nothing left to do.
    pub(super) fn clear_prompt(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        if self.prompt_hash.take().is_some() {
            self.recovery.stop_for(
                sv,
                run,
                Some(RecoveryAlert::PromptWaiting),
                "dialog_cleared",
            );
            sv.queue.record_runtime_event(
                run.id(),
                EventKind::PromptCleared,
                json!({"workspace_id": self.workspace}),
            )?;
            info!(run_id = %run.id(), "dialog of {} is gone", run.id());
            close_answer_prompt_asks(sv, run, PROMPT_CLEARED_CLOSED)?;
        }
        Ok(())
    }
}

/// Raise a dialog a worker's session stopped at as an `answer_prompt` ask
/// to the inbox once its recovery job escalated (ADR-0047 decision 40):
/// the question names the run, the workspace and the kind of dialog, the
/// job's `note`, and carries the screen's excerpt; the job's options are
/// the ask's and its reason category the ask's. An open ask of the run is
/// not registered twice. The runtime sends no key: the person answers the
/// dialog, and the ask closes itself once the dialog is gone.
pub(super) fn ask_answer_prompt(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    prompt: &str,
    excerpt: &str,
    note: Option<&Note>,
) -> Result<AskId> {
    let recovery = note.map_or_else(String::new, |note| {
        format!(
            "\n\nIts recovery job looked first, and {}.\n{}",
            note.why, note.text
        )
    });
    let question = format!(
        "The session of run {run_id} (task {task_id}) waits at a {prompt} dialog in workspace {workspace}. Answer with the choice to send to it (or what to do instead); the dialog is answered in that workspace, and this ask closes itself once the dialog is gone.{recovery}\n\nLast lines of the screen:\n{excerpt}",
        run_id = run.id(),
        task_id = run.task_id(),
    );
    let outcome = ask::ask(
        &mut *sv.queue,
        &sv.layout.main_checkout,
        NewAsk {
            recommendation: None,
            confidence: None,
            kind: AskKind::AnswerPrompt,
            task_id: Some(run.task_id()),
            run_id: Some(run.id().clone()),
            question,
            options: note.map(|note| note.options.clone()).unwrap_or_default(),
            asked_by: SessionRole::Supervisor.as_str().into(),
            reason_category: note.map_or(AskReason::RecoveryFailed, |note| note.category),
            topics: Vec::new(),
            finding_id: None,
        },
        sv.cmux,
    )?;
    info!(ask_id = %outcome["id"], run_id = %run.id(), "answer_prompt ask {} for {} (notified: {})", outcome["id"], run.id(), outcome["notified"]);
    Ok(AskId::new(
        outcome["id"].as_i64().context("ask returned no id")?,
    ))
}

/// Raise a worker's session stopped at a wall only a person moves
/// (ADR-0047 decision 42): a login that ran out, or the usage limit (task
/// 438). The run joins the queue's open `authentication` ask (or `cost`
/// ask of `subject: usage_limit`), or opens it, and a run that joined
/// records `auth_required` (or `usage_limited`) with the screen's excerpt
/// and its hash. However many sessions stop at it, the inbox gets one ask
/// and one notification, with the runs it holds listed. The error stays
/// on the screen after a person moved the wall and answered the ask, so a
/// screen the run already raised under an ask that is answered now is not
/// raised again: returns `false`, and the caller goes on as if nothing
/// held the session (the stall nudge then tells it to go on).
pub(super) fn raise_wall(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    screen: &str,
    wall: Wall,
) -> Result<bool> {
    let excerpt = sv.signals.screen_excerpt(screen);
    let hash = format!("{:x}", Sha256::digest(excerpt.as_bytes()));
    let last = sv
        .queue
        .run_events(run.id())?
        .into_iter()
        .rev()
        .find(|e| e.kind == wall.event_kind() && e.payload.get("job").is_none());
    if let Some(last) = last
        && last.payload.get("screen_hash").and_then(Value::as_str) == Some(hash.as_str())
        && let Some(id) = last.payload.get("ask_id").and_then(Value::as_i64)
        && !sv.queue.read_ask(AskId::new(id))?.is_open()
    {
        return Ok(false);
    }
    let (outcome, value) = ask::hold(
        &mut *sv.queue,
        &sv.layout.main_checkout,
        NewHold::wall(wall, Some(run.id().clone()), None),
        sv.cmux,
    )?;
    if outcome.joined {
        sv.queue.record_runtime_event(
            run.id(),
            wall.event_kind(),
            json!({
                "workspace_id": workspace,
                "excerpt": excerpt,
                "screen_hash": hash,
                "ask_id": outcome.ask.id,
            }),
        )?;
        warn!(ask_id = %outcome.ask.id, run_id = %run.id(), "run {} stopped at the {} wall in workspace {workspace}; ask {} holds {} run(s) and job(s) (notified: {})", run.id(), wall.as_str(), outcome.ask.id, outcome.ask.affected.len(), value["notified"]);
    }
    Ok(true)
}

/// Close the run's `answer_prompt` asks nobody closed, noting each.
pub(super) fn close_answer_prompt_asks(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    answer: &str,
) -> Result<()> {
    for ask in sv.queue.close_answer_prompt_asks(run.id(), answer)? {
        info!(ask_id = %ask.id, run_id = %run.id(), "closed the answer_prompt ask {} of {}: {answer}", ask.id, run.id());
    }
    Ok(())
}

/// The answers the runtime writes into an open `answer_prompt` ask it closes.
pub(super) const PROMPT_CLEARED_CLOSED: &str = "the dialog is gone; closed by the runtime";

pub(super) const PROMPT_RECEIPT_CLOSED: &str = "the receipt arrived; closed by the runtime";

pub(super) const PROMPT_EXITED_CLOSED: &str = "the session exited; closed by the runtime";

pub(super) const INPUT_READY_CLOSED: &str =
    "the input box got ready and the request was sent; closed by the runtime";

/// A session's screen is read for a dialog at most this often.
pub(super) const PROMPT_CHECK_INTERVAL: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::super::file_time::at_ns;
    use super::*;
    use crate::application::memory_files::MemoryFiles;

    /// Task 1050: for `marked_idle`, an idle marker of the millisecond of
    /// the last input typed (an adopter's, from an event) is not idle after
    /// it; without a known input any marker is.
    #[test]
    fn a_marker_of_the_inputs_millisecond_is_not_idle_after_it() {
        let files = MemoryFiles::default();
        let marker = Path::new("/run/idle.json");
        let input = Some(at_ns(250, 0));
        assert!(!marker_after(&files, marker, input));
        assert!(!marker_after(&files, marker, None));
        files.put(marker, at_ns(250, 700_000), "{}");
        assert!(!marker_after(&files, marker, input));
        assert!(marker_after(&files, marker, None));
        files.put(marker, at_ns(251, 0), "{}");
        assert!(marker_after(&files, marker, input));
    }
}
