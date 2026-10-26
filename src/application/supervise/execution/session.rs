//! A claimed run's worker session: its provisioning, the [`SessionWatch`]
//! of its wrapper, receipt and idle marker, and the answers to
//! its `worker_question` asks.

use super::*;
use crate::application::prompt::{HandoffMaterial, PromptBytes, handoff_text};
use crate::domain::EventKind;
use crate::domain::e2e_quarantine;
use crate::domain::fresh_session::{self, NextSession};
use crate::domain::local_checks::LocalChecks;

/// The file in a run's directory that holds the whole list of the tests
/// that fail already on the landing branch's CI its worker's prompt was
/// written with; a resolution request writes `<request>-known-failures.json`
/// and the review `review-<attempt>-known-failures.json` beside it.
pub(super) const KNOWN_FAILURES_FILE: &str = "known-failures.json";

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
    /// The tests that fail already on the landing branch's CI for a run of
    /// `task` (ADR-t1920-1 decision 5), written whole to `file` in its run
    /// directory, where the worker and the whole review read what their
    /// section leaves out; `None`, and no file, when the list holds none.
    /// A list that cannot be read or written leaves the section out with a
    /// warning: the claim, the request or the review goes on without it.
    pub(super) fn known_failures(
        &self,
        task: crate::domain::TaskId,
        file: &Path,
    ) -> Option<crate::application::prompt::KnownFailures> {
        let read = || -> Result<_> {
            let config = self.verifier.ci_watch_config()?;
            let view = crate::application::ci_watch::run_known_failures(
                &*self.queue,
                config.as_ref(),
                task,
            )?;
            let known =
                crate::application::prompt::KnownFailures::from_view(&view, &path_text(file)?);
            if known.is_some() {
                self.files
                    .write(file, serde_json::to_string_pretty(&view)?.as_bytes())?;
            }
            Ok(known)
        };
        read().unwrap_or_else(|error| {
            warn!(task_id = %task, "the tests that fail already on the landing branch's CI could not be read for task {task}; its prompt goes without them: {error:#}");
            None
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
    /// session wrappers, the runs' and the planners', in the background
    /// through [`SessionWrappers`](crate::application::SessionWrappers), its agents through the review provider and
    /// the spawner, on the queue's environment. A worker set to the
    /// container (goal 38) has its session wrapper started here, on the
    /// host, and its turns in their container; any other actor set to it is
    /// refused, never started on the host.
    pub(super) fn actors(&self) -> HostActorExecutor<'_> {
        self.actors_on(self.reviewer)
    }
    /// [`Self::actors`] with its agents made by `agent`: a headless job
    /// whose role runs on another provider (ADR-t1063-1).
    pub(super) fn actors_on<'s>(&'s self, agent: &'s dyn AgentProvider) -> HostActorExecutor<'s> {
        HostActorExecutor::new(&self.layout.db)
            .with_no_claude(self.no_claude)
            .with_sessions(self.sessions)
            .with_provider(agent)
            .with_spawner(self.spawner)
            .with_queue_service(self.service_access)
            .with_events(&*self.queue)
            .with_config(crate::application::execution::ExecutionConfig::of_runtimes(
                &self.landing.runtimes,
            ))
    }
    /// What `run` inherits from an earlier run of its task that was retried
    /// with its branch carried over (ADR-0047 decision 24): the one
    /// [`crate::domain::resume::inherited_from`] picks, the latest such run
    /// unless a release started afresh since. Its own commits are counted
    /// from their merge base with `run`'s base, the current main. Without
    /// one, what a child of an applied replacement takes of the replaced
    /// run's saved work ([`Self::replan_inheritance`]), handed over into
    /// `run_dir`; nothing at all after a release started the task afresh
    /// (`retry_fresh`), the snapshot left as it is and the end of a
    /// handover still going recorded by that release
    /// ([`crate::domain::task_hold::ReplanCarryEnded`]).
    fn inheritance(&mut self, run: &TaskRun, run_dir: &Path) -> Result<Option<Inheritance>> {
        use crate::domain::resume::{InheritedFrom, inherited_from};
        let runs = self.queue.show(run.task_id())?.runs;
        let previous: Vec<&TaskRun> = runs
            .iter()
            .take_while(|other| other.id() != run.id())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let events = previous
            .iter()
            .map(|previous| self.queue.run_events(previous.id()))
            .collect::<Result<Vec<_>>>()?;
        let retried = match inherited_from(events.iter().map(Vec::as_slice)) {
            InheritedFrom::Fresh => return Ok(None),
            InheritedFrom::Run(at) => self.retried_inheritance(run, previous[at], &events[at]),
            InheritedFrom::Nothing => None,
        };
        match retried {
            Some(inherited) => Ok(Some(inherited)),
            None => self.replan_inheritance(run, run_dir),
        }
    }
    /// What `run` inherits from `previous`, a retry that carried its branch
    /// over with `events`.
    fn retried_inheritance(
        &self,
        run: &TaskRun,
        previous: &TaskRun,
        events: &[RunEvent],
    ) -> Option<Inheritance> {
        let mut inherited = Inheritance::of(&*self.files, previous, events)?;
        if let Ok(Some(base)) = self
            .repository
            .merge_base(run.base_commit().as_str(), &inherited.head)
        {
            inherited.base = base;
        }
        Some(inherited)
    }
    /// What `run`, of a child of an applied replacement, takes of the
    /// saved work of the run the replacement ended (ADR-t1521-2 decision
    /// 5), handed over only as the plan said: the snapshot's head, checked
    /// to be what its ref still pins, or its uncommitted changes, each file
    /// checked against the manifest and copied into `run_dir`. Nothing for
    /// a task that takes none. A handover that fails records
    /// `replan_carry_failed` on the run and fails its provisioning
    /// ([`ReplanCarryFailed`]: the run is abandoned with the error for its
    /// recovery and the claims go on), with the snapshot and its ref left
    /// as they are; nothing of the saved work goes anywhere but a run's
    /// worktree, through that run's own receipt, review and landing.
    fn replan_inheritance(&mut self, run: &TaskRun, run_dir: &Path) -> Result<Option<Inheritance>> {
        let Some(carry) = self.queue.child_carry(run.task_id())? else {
            return Ok(None);
        };
        match self.hand_over(run, &carry, run_dir) {
            Ok(inherited) => Ok(Some(inherited)),
            Err(error) => {
                let why = format!(
                    "the work replan operation {} saved in snapshot {} could not be handed to task {}: {error:#}; the snapshot and its ref are kept",
                    carry.operation,
                    carry.snapshot_id,
                    run.task_id()
                );
                self.queue.record_runtime_event(
                    run.id(),
                    EventKind::ReplanCarryFailed,
                    json!({"operation_id": carry.operation, "snapshot_id": carry.snapshot_id,
                           "carry": carry.carry, "reason": why}),
                )?;
                Err(anyhow::Error::new(ReplanCarryFailed(why)))
            }
        }
    }
    /// [`Self::replan_inheritance`]'s handover of `carry` to `run`.
    fn hand_over(
        &mut self,
        run: &TaskRun,
        carry: &crate::domain::replan_apply::ChildCarry,
        run_dir: &Path,
    ) -> Result<Inheritance> {
        use crate::domain::replan_plan::CarrySource;
        use crate::domain::replan_snapshot::{self, MANIFEST, SavedKind, SnapshotManifest};
        let dir = replan_snapshot::snapshot_dir(&self.layout.replans_dir, &carry.snapshot_id);
        let manifest: SnapshotManifest =
            serde_json::from_slice(&self.files.read(&dir.join(MANIFEST))?)
                .context("read the snapshot's manifest")?;
        ensure!(
            manifest.operation == carry.operation && manifest.run_id == carry.from_run,
            "the snapshot is of replan operation {} and run {}",
            manifest.operation,
            manifest.run_id
        );
        manifest.check_paths()?;
        // Only the files this child takes are read and checked: the head
        // alone is its ref, the uncommitted changes are their diffs and
        // untracked files.
        let taken = SnapshotManifest {
            files: manifest
                .files
                .iter()
                .filter(|file| {
                    carry.carry == CarrySource::SnapshotDiff
                        && matches!(
                            file.kind,
                            SavedKind::Index | SavedKind::Worktree | SavedKind::Untracked
                        )
                })
                .cloned()
                .collect(),
            ..manifest.clone()
        };
        let mut contents = Vec::new();
        for file in &taken.files {
            if let Ok(bytes) = self.files.read(&dir.join(&file.name)) {
                contents.push((file.name.clone(), bytes));
            }
        }
        let saved: Vec<(String, u64, String)> = contents
            .iter()
            .map(|(name, bytes)| {
                (
                    name.clone(),
                    bytes.len() as u64,
                    replan_snapshot::sha256(bytes),
                )
            })
            .collect();
        replan_snapshot::check_saved(&taken, &saved)?;
        let mut files = Vec::new();
        match carry.carry {
            CarrySource::SnapshotHead => {
                let pinned = self
                    .repository
                    .is_ancestor(&manifest.head, &manifest.pinned_ref)?
                    && self
                        .repository
                        .is_ancestor(&manifest.pinned_ref, &manifest.head)?;
                ensure!(
                    pinned,
                    "{} does not name the saved head {}",
                    manifest.pinned_ref,
                    manifest.head
                );
            }
            CarrySource::SnapshotDiff => {
                let into = run_dir.join("replan-carry");
                self.files.create_dir_all(&into)?;
                for file in &taken.files {
                    let what = match file.kind {
                        SavedKind::Index => "staged diff".to_owned(),
                        SavedKind::Worktree => "unstaged diff".to_owned(),
                        SavedKind::Untracked if file.executable => {
                            format!("untracked executable file {}", file.source)
                        }
                        SavedKind::Untracked => format!("untracked file {}", file.source),
                        _ => continue,
                    };
                    let Some((_, bytes)) = contents.iter().find(|(name, _)| *name == file.name)
                    else {
                        continue;
                    };
                    let to = into.join(&file.name);
                    if let Some(parent) = to.parent() {
                        self.files.create_dir_all(parent)?;
                    }
                    self.files.write(&to, bytes)?;
                    // The mode the park saved with it, as a restore does.
                    if file.executable {
                        self.files.set_executable(&to, true)?;
                    }
                    files.push((path_text(&to)?, what));
                }
            }
            CarrySource::Base => {}
        }
        let mut base = CommitSha::try_from(manifest.base_commit.as_str())?;
        if let Ok(Some(merged)) = self
            .repository
            .merge_base(run.base_commit().as_str(), &manifest.head)
        {
            base = merged;
        }
        Ok(Inheritance {
            run_id: carry.from_run.clone(),
            base,
            head: manifest.head.clone(),
            branch: manifest.branch.clone(),
            receipt_path: None,
            summary: String::new(),
            by_hand: None,
            replan: Some(crate::application::prompt::ReplanCarried {
                operation: carry.operation,
                task: carry.from_task,
                carry: carry.carry,
                pinned_ref: manifest.pinned_ref.clone(),
                files,
            }),
        })
    }
    /// Keep the head `inherited` names, the head its run's record of the
    /// retry chose, under `refs/dagq/runs/<run-id>` of the run it is carried
    /// from, before the next run's clone or worktree is made
    /// (ADR-t3294-1): whatever a writer that raced the record, or a refused
    /// `ready --inherit` putting the ref back, left it holding, the ref
    /// holds the recorded head as the run starts. A refusal that arrives
    /// later may still put it back; nothing writes it again then, and the
    /// clone and the prompt name the head by its id whatever the ref holds.
    /// Not for a child of an applied replacement, whose head the snapshot's
    /// own ref pins.
    fn keep_inherited(&mut self, inherited: &Inheritance) -> Result<()> {
        let from = self.queue.run(&inherited.run_id)?;
        let head = CommitSha::parse(inherited.head.clone(), "inherited head")?;
        crate::application::inherit::keep_inherited_head(
            &*crate::application::run_git::for_run(&*self.repository, &from)?,
            from.id(),
            &head,
        )
    }
    /// Write `run`'s prompt (`prompt.txt` in `run_dir`) for its task, as
    /// its worker's route and provider take it, with the known failures in
    /// [`KNOWN_FAILURES_FILE`]; the branch it inherits, if any, and what
    /// the prompt takes (ADR-t2072-1). Written again when the
    /// run moves to the other provider (ADR-t813-2), whose worker is told
    /// otherwise.
    pub(super) fn write_prompt(
        &mut self,
        task: &crate::domain::Task,
        run: &TaskRun,
        run_dir: &Path,
    ) -> Result<(Option<Inheritance>, PromptBytes)> {
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
        let inherited = self.inheritance(run, run_dir)?;
        let known_failures = self.known_failures(task.id(), &run_dir.join(KNOWN_FAILURES_FILE));
        let fitted = prompt(
            task,
            run,
            goal.as_ref(),
            &predecessors,
            &goal_predecessors,
            &siblings,
            inherited.as_ref(),
            &self.e2e_paths(),
            known_failures.as_ref(),
        )?;
        let fitted = fitted.with_language(self.verifier.language().as_ref());
        self.files
            .write(&run_dir.join("prompt.txt"), fitted.text.as_bytes())?;
        Ok((inherited, fitted.bytes))
    }
    /// Plan paths, create the run directory and worktree, and start the
    /// session wrapper in the background (ADR-t1433-3). Any error leaves
    /// what was created for inspection, but a half made clone, which
    /// [`Repository::create_clone`] removes.
    pub(super) fn provision(&mut self, claimed: &TaskRun) -> Result<SessionWatch> {
        let state_dir = &self.layout.runs_dir;
        let paths = RunPaths::new(state_dir, claimed.id());
        let run_dir = paths.run_dir.clone();
        // A worker set to the container works in a clone of its own, not a
        // worktree of this repository (ADR-t2113-2 decision 1).
        let workplace = if self.landing.runtimes.worker
            == crate::domain::container_runtime::Runtime::Container
        {
            crate::domain::run_clone::clone_path(state_dir, claimed.id())
        } else {
            paths.worktree.clone()
        };
        let plan = RunPlan {
            repo_path: path_text(&self.layout.repo_root)?,
            run_dir: path_text(&run_dir)?,
            branch: format!("dagq/{}", claimed.id()),
            worktree_path: path_text(&workplace)?,
            receipt_path: path_text(&paths.receipt)?,
            log_path: path_text(&paths.log)?,
        };
        // Save intended paths before any external resource is created.
        self.queue
            .plan_run(claimed.id(), self.registration.token(), &plan)?;
        self.files.create_dir_all(state_dir)?;
        self.files
            .create_new_dir(&run_dir)
            .context("run directory must be new")?;
        let run_env = self.verifier.run_env(&run_dir)?;
        let run = self.queue.run(claimed.id())?;
        // The server a worker's turns build through is the supervisor's to
        // start, on either provider (ADR-t2086-1); its wrapper looks again
        // before each turn.
        self.ensure_sccache(crate::domain::sccache::CheckReason::BeforeWorker);
        let task = self.queue.show(run.task_id())?.task;
        let (inherited, prompt_bytes) = self.write_prompt(&task, &run, &run_dir)?;
        if let Some(inherited) = &inherited {
            self.queue.record_runtime_event(
                run.id(),
                EventKind::RunInherited,
                json!({"inherit_from_run": inherited.run_id, "head": inherited.head, "branch": inherited.branch,
                       "replan_operation_id": inherited.replan.as_ref().map(|replan| replan.operation),
                       "carry": inherited.replan.as_ref().map(|replan| replan.carry)}),
            )?;
            info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} carries run {}'s work over from {}", run.id(), run.task_id(), inherited.run_id, inherited.head);
        }
        // A running wrapper must not change when the development binary is rebuilt.
        self.files
            .copy(&self.layout.runner, &run_dir.join(RUN_RUNNER_FILE))
            .context("snapshot runtime binary")?;
        self.prepare_turns(&run, &run_dir)?;
        if let Some(inherited) = &inherited
            && inherited.replan.is_none()
            && let Err(error) = self.keep_inherited(inherited)
        {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the head {} it carries over from run {} could not be kept under its ref: {error:#}", run.id(), inherited.head, inherited.run_id);
        }
        let git_output = if run.in_container() {
            let carried = inherited
                .as_ref()
                .map(|inherited| (inherited.clone_ref(), inherited.head.clone()));
            self.repository.create_clone(
                &run,
                carried
                    .as_ref()
                    .map(|(name, head)| (name.as_str(), head.as_str())),
            )?
        } else {
            self.repository.create_worktree(&run)?
        };
        self.files
            .write(&run_dir.join("worktree-create.txt"), git_output.as_bytes())?;
        self.queue.record_runtime_event(
            run.id(),
            EventKind::WorktreeCreated,
            json!({"path": plan.worktree_path, "branch": plan.branch}),
        )?;
        self.warn_ignored_wrapper_setting();
        let log = self.session_log(&run_dir, None, false);
        let command = background::wrapper_command(vec![
            path_text(&run_dir.join(RUN_RUNNER_FILE))?,
            "--db".into(),
            path_text(&self.layout.db)?,
            "session".into(),
            "--run".into(),
            run.id().to_string(),
            "--lease".into(),
            self.registration.token().to_string(),
            "--claude".into(),
            path_text(&self.layout.claude)?,
            "--codex".into(),
            path_text(&self.layout.codex)?,
        ]);
        // The executor's telemetry token, with the worker's (ADR-t1662-3
        // decision 11).
        self.issue_run_telemetry(&run);
        let handle = self
            .actors()
            .spawn(ActorExecutionSpec::new(
                ActorContext::worker(run.id(), run.task_id()),
                WorkspaceAccess::Write(workplace.clone()),
                ActorProgram::RunSession {
                    run: &run,
                    wrapper: command,
                    run_env,
                    log: &log,
                },
            ))?
            .workspace()?;
        if let Err(error) =
            self.queue
                .workspace_created(run.id(), self.registration.token(), &handle)
        {
            // Unrecorded, the wrapper would run on with nothing to find it
            // by, and its registration is refused (task 806).
            return Err(match stop_session(self.sessions, &handle, StopRoute::Unrecorded) {
                Ok(()) => error.context(format!(
                    "the background wrapper {handle} of run {} could not be recorded and was stopped",
                    run.id()
                )),
                Err(stop) => error.context(format!(
                    "the background wrapper {handle} of run {} could not be recorded, and stopping it failed: {stop:#}",
                    run.id()
                )),
            });
        }
        self.record_launch(&run, &handle, &log, Some(&prompt_bytes))?;
        info!(task_id = %run.task_id(), run_id = %run.id(), "task {} running in the background as {}; run {}", run.task_id(), handle, run.id());
        Ok(SessionWatch {
            workspace: handle,
            run_dir,
            receipt_path: PathBuf::from(plan.receipt_path),
            idle_marker: run.idle_marker_path()?,
            startup: self.generators.clock.monotonic(),
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
            awaits_answer: None,
        })
    }
}

/// Whether `limit` has passed at `now` (the supervisor's monotonic clock)
/// since `since`: a wait of exactly `limit` is over. The waits of a
/// session, a revise, a resume and a reopen are all judged by it, with the
/// times read from the injected [`Clock`] (task 1557).
pub(super) fn passed(now: Instant, since: Instant, limit: Duration) -> bool {
    now.saturating_duration_since(since) >= limit
}

/// Whether the wrapper of a session started at `startup` may still
/// register at `now`: within the registration `timeout`, after which the
/// session (or the resumed one) is an error that retains the run.
pub(super) fn registration_pending(now: Instant, startup: Instant, timeout: Duration) -> bool {
    !passed(now, startup, timeout)
}

/// Whether background work the session left running after its receipt,
/// first seen at `receipt_seen_at`, was waited for long enough at `now`:
/// the resume `timeout`, after which an idle marker that stopped after the
/// receipt is taken as the session's idle. No receipt seen is no wait.
pub(super) fn waited_out_after_receipt(
    now: Instant,
    receipt_seen_at: Option<Instant>,
    timeout: Duration,
) -> bool {
    receipt_seen_at.is_some_and(|at| passed(now, at, timeout))
}

/// Whether a watch that follows the `worker_question`s created from
/// `asks_from` ([`SessionWatch::asks_from`]) follows one created at
/// `created_at` (both unix seconds): its answer is typed and it holds the
/// session; an earlier one is the inbox's (task 582). The queue's
/// `has_unclosed_worker_question_since` applies the same bound.
pub(super) fn follows_ask(created_at: i64, asks_from: i64) -> bool {
    created_at >= asks_from
}

/// The answers of `answers` a watch following the asks created from
/// `asks_from` types ([`follows_ask`]): those of the asks before it are the
/// inbox's to deliver by hand, and the queue's `undelivered_answers` does
/// not leave them out.
pub(super) fn followed_answers(
    mut answers: Vec<crate::domain::Ask>,
    asks_from: i64,
) -> Vec<crate::domain::Ask> {
    answers.retain(|ask| follows_ask(ask.created_at, asks_from));
    answers
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
    /// The oldest of the first session's own `worker_question`s whose
    /// answer it waits for in its slot, before its receipt and any
    /// `/exit`, as the last poll or the adoption found it: the run is in
    /// the phase `waiting` meanwhile, not `worker`
    /// ([`super::stages::question_moves`]). A revise's or a resume's live
    /// watch leaves it to its own watch.
    pub(super) awaits_answer: Option<AskId>,
}

impl SessionWatch {
    /// The watch of a live session asked at `input_at` to fix what its
    /// review or a conflict named ([`ReviseWatch`]), or what parked its run
    /// ([`ResumeWatch`]): only the answers of its `worker_question`s and its
    /// recovery are followed (task 238, ADR-0071 decision 17). `now` is
    /// the supervisor's monotonic clock.
    pub(super) fn fixing(
        run: &TaskRun,
        workspace: &str,
        input_at: SystemTime,
        now: Instant,
    ) -> Result<Self> {
        Ok(SessionWatch {
            workspace: workspace.to_owned(),
            run_dir: PathBuf::from(run.run_dir().context("missing run directory")?),
            receipt_path: PathBuf::from(run.receipt_path().context("missing receipt path")?),
            idle_marker: run.idle_marker_path()?,
            startup: now,
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
            awaits_answer: None,
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
        self.awaits_answer = None;
        let processes = sv.queue.processes(run.id())?;
        self.watch_first_commit(sv, run)?;
        self.observe_receipt(sv, run)?;
        self.stall.settle(sv, run, self.receipt_seen)?;
        self.stop_idle_job(sv, run, true);
        if let Some(moved) = self.validate_when_idle(sv, run, &processes)? {
            return Ok(moved);
        }
        let wrapper = processes.iter().find(|p| p.role == "wrapper");
        // A lost session no attempt could open again (task 1372) ends as
        // one that exited, with its lost wrapper's code: the attempts
        // forgot its processes.
        let lost_exit = wrapper
            .is_none()
            .then(|| sv.resume.reopens.get(run.id()).and_then(|r| r.lost_exit()))
            .flatten();
        // The receipt is read once the session ended, whatever order its
        // receipt and its exit were seen in: one a turn wrote before it
        // failed goes to validation (ADR-t1594-1).
        if let Some(code) = lost_exit {
            sv.resume.forget_reopen(run.id());
            self.end_exited(sv, run, false)?;
            let receipt = sv.files.is_file(&self.receipt_path);
            return sv
                .queue
                .finish_lost_session(run.id(), sv.registration.token(), code, receipt)
                .map(Some);
        }
        if let Some(wrapper) = wrapper {
            if wrapper.exited_at.is_some() {
                return self.finish_exited(sv, run).map(Some);
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
                    let timeout = sv.sessions.exit_timeout();
                    sv.queue.record_runtime_event(
                        run.id(),
                        EventKind::ExitRequested,
                        json!({"workspace_id": self.workspace, "timeout_secs": timeout.as_secs()}),
                    )?;
                    let workspace = self.workspace.clone();
                    submit(sv, run, &workspace, Input::Exit, "/exit")?;
                    info!(run_id = %run.id(), "exit requested for {} after its wrapper went silent; waiting for session exit", run.id());
                    self.exit_requested = Some(sv.generators.clock.monotonic());
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
                    if sv.resume.reopens.get(run.id()).is_some_and(|r| r.settled()) {
                        sv.resume.forget_reopen(run.id());
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
        } else if super::recovery::waits_for_memory(sv, run)? {
            // The wrapper registers with its first turn's process, which
            // waits for the dagq machine's memory: its time starts once
            // it no longer does (ADR-t3163-1 decision 4).
            self.startup = sv.generators.clock.monotonic();
        } else {
            let timeout = sv.sessions.registration_timeout();
            ensure!(
                registration_pending(sv.generators.clock.monotonic(), self.startup, timeout),
                "wrapper did not register within {} seconds",
                timeout.as_secs()
            );
        }
        self.awaits_answer = self.awaited_question(sv, run)?;
        Ok(None)
    }

    /// The receipt, recorded as `receipt_observed` the first time it is
    /// seen.
    fn observe_receipt(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        if self.receipt_seen || !sv.files.is_file(&self.receipt_path) {
            return Ok(());
        }
        self.receipt_seen = true;
        self.receipt_seen_at = Some(sv.generators.clock.monotonic());
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
        Ok(())
    }

    /// The session idle after its receipt goes to validation with the
    /// session open: `Some(Some(run))` once supervision finished,
    /// `Some(None)` when the poll ends here (its stale receipt was asked to
    /// be rewritten), `None` when the session is not idle after its receipt
    /// and the poll goes on.
    fn validate_when_idle(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        processes: &[RunProcess],
    ) -> Result<Option<Option<TaskRun>>> {
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
        let waited_out = waited_out_after_receipt(
            sv.generators.clock.monotonic(),
            self.receipt_seen_at,
            sv.sessions.resume_timeout(),
        );
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
                        !n.answered_by(idle.modified()) && !n.waited_out(&*sv.files, sv.sessions)
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
                return Ok(Some(None));
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
                .finish_supervision_live(run.id(), sv.registration.token())
                .map(|run| Some(Some(run)));
        }
        Ok(None)
    }

    /// The session whose wrapper exited ends its supervision: with its
    /// receipt it goes to validation whatever its exit code, without one
    /// it fails (ADR-t1594-1).
    fn finish_exited(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<TaskRun> {
        sv.resume.forget_reopen(run.id());
        self.end_exited(sv, run, true)?;
        let receipt = sv.files.is_file(&self.receipt_path);
        sv.queue
            .finish_supervision(run.id(), sv.registration.token(), receipt)
    }

    /// One observation while the supervisor drains for a handoff
    /// (ADR-t3077-1 decision 2): the receipt is observed as [`Self::poll`]
    /// does, and, with `validate`, a session idle after it or whose wrapper
    /// exited after it goes to validation; nothing else of the session is
    /// followed, and no input but a stale receipt's request is typed into
    /// it. `Some` once supervision finished.
    pub(super) fn poll_handing_off(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        validate: bool,
    ) -> Result<Option<TaskRun>> {
        self.observe_receipt(sv, run)?;
        if !validate || !self.receipt_seen {
            return Ok(None);
        }
        self.stall.settle(sv, run, self.receipt_seen)?;
        self.stop_idle_job(sv, run, true);
        let processes = sv.queue.processes(run.id())?;
        if let Some(moved) = self.validate_when_idle(sv, run, &processes)? {
            return Ok(moved);
        }
        let exited = processes
            .iter()
            .find(|p| p.role == "wrapper")
            .is_some_and(|w| w.exited_at.is_some());
        if exited {
            return self.finish_exited(sv, run).map(Some);
        }
        Ok(None)
    }

    /// The question the first session waits for in its slot: the oldest
    /// unclosed `worker_question` it follows, while it wrote no receipt
    /// and was not asked to exit (a receipt moves it on past its
    /// questions, as it ends a wait outside the slots).
    pub(super) fn awaited_question(
        &self,
        sv: &Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<AskId>> {
        if self.receipt_seen || self.exit_requested.is_some() {
            return Ok(None);
        }
        sv.oldest_unclosed_question(run, self.asks_from)
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
        if run.worktree_path().is_none() {
            return Ok(());
        }
        let head = match crate::application::run_git::head(&*sv.repository, run) {
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
        let mut answers = followed_answers(sv.queue.undelivered_answers(run.id())?, self.asks_from);
        if answers.is_empty() {
            return Ok(None);
        }
        // Not while a replanning or a hold fences the task: the fence
        // refuses the turn, which would be no failure of the delivery; the
        // answer stays open for the session resumed after it.
        if sv.queue.turn_fence(run.task_id())?.is_some() {
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
            let what = crate::domain::turn::answer_what(ask.id);
            let sent_at = sv.files.now();
            let workspace = self.workspace.clone();
            sv.pause_at(super::PausePoint::AnswerSend, run.id());
            match submit(sv, run, &workspace, Input::from(&text), &what) {
                // Sent: failing to record it must not cost the live run its
                // lease, so it is only noted (the ask then shows unclosed).
                Ok(_submission) => {
                    typed = Some(sent_at);
                    self.stall.input_sent(sent_at, Some(&text.text));
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
                // A fence that came after the check above: the delivery
                // waits for the session resumed after it, and is no failure
                // that would keep it from it.
                Err(error)
                    if error
                        .chain()
                        .any(|cause| cause.is::<super::headless::TurnFenced>()) =>
                {
                    info!(ask_id = %ask.id, run_id = %run.id(), "answer of ask {} waits: {error:#}", ask.id);
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

impl Supervisor<'_> {
    /// Read `[fresh_session]` of `dagq.toml` again (ADR-t2080-1), as
    /// `[provider_fallback]` is: a file that cannot be read keeps the
    /// threshold in use, with one warning until the error changes.
    pub(super) fn reread_fresh_session(&mut self) {
        let Some(read) = self.fresh_session_file.clone() else {
            return;
        };
        let to = match read() {
            Ok(Some(to)) => to,
            Ok(None) => {
                self.fresh_session_error = None;
                return;
            }
            Err(error) => {
                let message = format!("{error:#}");
                if self.fresh_session_error.as_ref() != Some(&message) {
                    warn!(error = %message, "[fresh_session] of dagq.toml not read: {message}; keeping peak_context_above = {:?}", self.fresh_session.peak_context_above);
                    self.fresh_session_error = Some(message);
                }
                return;
            }
        };
        self.fresh_session_error = None;
        if to != self.fresh_session {
            info!(
                "[fresh_session] of dagq.toml changed: peak_context_above {:?} -> {:?}",
                self.fresh_session.peak_context_above, to.peak_context_above
            );
            self.fresh_session = to;
        }
    }

    /// Whether a send-back or resume of `run`'s worker goes on in its
    /// session or in a new one (ADR-t2080-1 decisions 1 and 4): by its
    /// previous turn's `peak_context` and `[fresh_session]` alone. A new
    /// session recorded and asked nothing yet
    /// ([`fresh_session::unrequested_renewal`]) is the one it goes on in,
    /// whatever the threshold is now: the session wrapper starts it anew.
    pub(super) fn next_session(&self, run: &TaskRun) -> Result<NextSession> {
        let events = self.queue.run_events(run.id())?;
        if let Some((peak_context, threshold)) = fresh_session::unrequested_renewal(&events) {
            return Ok(NextSession::Fresh {
                peak_context,
                threshold,
            });
        }
        Ok(fresh_session::next_session(
            fresh_session::previous_peak_context(&events),
            self.fresh_session.peak_context_above,
        ))
    }

    /// What a send-back or resume writes to `run`'s worker as its next
    /// request ([`Self::next_session`]): `request` (built without the
    /// language's instruction) with it, for the same session; for a new
    /// one, the handoff prompt ([`Self::handoff_prompt`]) that carries
    /// `request`, with why the session is new. `request_file` is where the
    /// handoff says the whole request is, written only when the handoff cut
    /// it. The caller records the new session ([`Self::renew_session`])
    /// before it writes the request.
    pub(super) fn session_request(
        &mut self,
        run: &TaskRun,
        request: FittedPrompt,
        request_file: &Path,
    ) -> Result<(FittedPrompt, Option<(u64, u64)>)> {
        let NextSession::Fresh {
            peak_context,
            threshold,
        } = self.next_session(run)?
        else {
            return Ok((
                request.with_language(self.verifier.language().as_ref()),
                None,
            ));
        };
        let fresh = (peak_context, threshold);
        let handoff = self.handoff_prompt(run, fresh, (&request.text, request_file))?;
        if handoff.bytes.omitted.contains_key("request") {
            self.files.write(request_file, request.text.as_bytes())?;
        }
        Ok((handoff, Some(fresh)))
    }

    /// The handoff prompt of a new session of `run`'s worker
    /// ([`handoff_text`], ADR-t2852-1): its own commits
    /// (`review::run_commits`) and its worktree's changes as git shows
    /// them, the records of its local checks on the tree of its head (the
    /// others counted, as a review counts them),
    /// the tests that fail already on the landing branch's CI with the
    /// names of the paths its commits and its worktree change and its task
    /// declares (the whole list written beside `request_file`), the resume
    /// timeout, the summary of the receipt it wrote last and the `request`
    /// it would have been sent (with the file that holds it whole). What
    /// git cannot show is said, not an error: the session reads it in its
    /// worktree. The language's instruction is added here once, so
    /// `request` is passed without it.
    pub(super) fn handoff_prompt(
        &mut self,
        run: &TaskRun,
        (peak_context, threshold): (u64, u64),
        (request, request_file): (&str, &Path),
    ) -> Result<FittedPrompt> {
        run.worktree_path().context("missing worktree")?;
        let unread = |what: &str, error: anyhow::Error| {
            warn!(run_id = %run.id(), "the {what} of {} could not be read for its handoff: {error:#}", run.id());
            format!("(could not be read: run `git {what}` in your worktree)")
        };
        let commits = crate::application::review::run_commits(&*self.repository, run)
            .unwrap_or_else(|error| unread("log", error));
        let status = crate::application::run_git::status(&*self.repository, run);
        let mut touched: Vec<String> = status
            .as_deref()
            .map(|status| status.lines().filter_map(porcelain_path).collect())
            .unwrap_or_default();
        let changes = status.unwrap_or_else(|error| unread("status", error));
        let head_tree = crate::application::run_git::head(&*self.repository, run)
            .and_then(|head| self.repository.tree_of(head.as_str()));
        let head_tree = head_tree
            .map_err(|error| warn!(run_id = %run.id(), "the tree of {}'s head could not be read for its handoff: {error:#}", run.id()))
            .ok();
        match crate::application::review::run_changed_paths(&*self.repository, run) {
            Ok(paths) => touched.extend(paths),
            Err(error) => {
                warn!(run_id = %run.id(), "the paths {} changes could not be read for its handoff: {error:#}", run.id());
            }
        }
        // The declared paths only add names: without them the run's own
        // changes still name its tests.
        match self.queue.show(run.task_id()) {
            Ok(detail) => touched.extend(detail.task.paths().iter().cloned()),
            Err(error) => {
                warn!(run_id = %run.id(), "task {}'s paths could not be read for its handoff: {error:#}", run.task_id());
            }
        }
        let touched = crate::application::prompt::touched_names(&touched);
        let stem = request_file
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("handoff");
        let run_dir = run.run_dir().map(Path::new);
        let known = run_dir.and_then(|dir| {
            self.known_failures(
                run.task_id(),
                &dir.join(format!("{stem}-handoff-known-failures.json")),
            )
        });
        // Judged against the head's tree as a review judges them against
        // its commit's: without the tree no record is on it.
        let (checks, older_checks) = run_dir.map_or_else(
            || (LocalChecks::default(), 0),
            |dir| {
                crate::application::review::newest_local_checks(
                    &*self.files,
                    dir,
                    head_tree.as_deref().unwrap_or_default(),
                )
            },
        );
        let receipt = run
            .receipt_path()
            .and_then(|path| self.files.read_to_string(Path::new(path)).ok())
            .and_then(|text| Receipt::parse(&text).ok());
        let prompt = handoff_text(
            run,
            &HandoffMaterial {
                peak_context,
                threshold,
                resume_timeout: self.sessions.resume_timeout().as_secs(),
                commits: &commits,
                changes: &changes,
                head_tree: head_tree.as_deref(),
                checks: &checks,
                older_checks,
                known_failures: known.as_ref(),
                touched: &touched,
                receipt_summary: receipt.as_ref().map(Receipt::summary),
                request,
                request_file: Some(request_file),
            },
        );
        Ok(prompt.with_language(self.verifier.language().as_ref()))
    }

    /// Record that `run`'s worker goes on in a new session (ADR-t2080-1
    /// decision 5) as `session_renewed` (why, the provider it stays on, the
    /// new session's name), before its handoff is written: the session
    /// wrapper starts the next request it takes after the record as the
    /// first turn of a new session, with the task's prompt before it, and
    /// carries no conversation over. The run, its worktree, its branch and
    /// its provider stay. A new session recorded and asked nothing yet
    /// ([`fresh_session::unrequested_renewal`]) is not recorded again.
    pub(super) fn renew_session(&mut self, run: &TaskRun, fresh: (u64, u64)) -> Result<()> {
        let events = self.queue.run_events(run.id())?;
        if fresh_session::unrequested_renewal(&events).is_some() {
            return Ok(());
        }
        let payload = fresh_session::renewed_payload(
            run.id().as_str(),
            run.actual_provider(),
            &events,
            fresh,
        );
        info!(run_id = %run.id(), "run {}'s worker goes on in a new session ({}): its previous turn's context took {} tokens, above {}", run.id(), payload["session"], fresh.0, fresh.1);
        self.queue
            .record_runtime_event(run.id(), EventKind::SessionRenewed, payload)?;
        // The new session is the run's before its first turn starts.
        self.register_run_sessions(run.id());
        Ok(())
    }
}

/// The path a line of `git status --porcelain` names: the new one of a
/// rename.
fn porcelain_path(line: &str) -> Option<String> {
    let path = line.get(3..)?;
    let path = path.rsplit(" -> ").next().unwrap_or(path);
    let path = path.trim().trim_matches('"');
    (!path.is_empty()).then(|| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Ask;

    /// A porcelain line names its path, a rename its new one.
    #[test]
    fn a_porcelain_line_names_its_path() {
        assert_eq!(
            porcelain_path(" M src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(porcelain_path("?? notes.txt").as_deref(), Some("notes.txt"));
        assert_eq!(
            porcelain_path("R  tests/old.rs -> tests/new.rs").as_deref(),
            Some("tests/new.rs")
        );
        assert_eq!(porcelain_path(""), None);
        assert_eq!(porcelain_path("?? "), None);
    }

    /// An origin for the monotonic times the decisions are given: only the
    /// offsets from it matter, never what the clock read.
    fn origin() -> Instant {
        Instant::now()
    }

    const MS: Duration = Duration::from_millis(1);

    /// A wait of exactly its limit is over; a millisecond before it is not,
    /// and a time before the start (an adopted start) is no wait at all.
    #[test]
    fn a_wait_is_over_at_its_limit_and_not_a_millisecond_before() {
        let t0 = origin() + Duration::from_secs(3600);
        let limit = Duration::from_secs(30);
        assert!(!passed(t0 + limit - MS, t0, limit));
        assert!(passed(t0 + limit, t0, limit));
        assert!(passed(t0 + limit + MS, t0, limit));
        assert!(!passed(t0 - MS, t0, limit));
        assert!(passed(t0, t0, Duration::ZERO));
    }

    /// A wrapper registers until the registration timeout since the
    /// session's start; at the timeout itself it is too late.
    #[test]
    fn a_wrapper_may_register_until_its_timeout() {
        let t0 = origin();
        let timeout = Duration::from_secs(60);
        assert!(registration_pending(t0, t0, timeout));
        assert!(registration_pending(t0 + timeout - MS, t0, timeout));
        assert!(!registration_pending(t0 + timeout, t0, timeout));
        assert!(!registration_pending(t0 + timeout + MS, t0, timeout));
    }

    /// Background work after the receipt is waited for up to the resume
    /// timeout from when the receipt was first seen; without a receipt
    /// nothing is waited out.
    #[test]
    fn background_work_after_the_receipt_is_waited_out_at_the_resume_timeout() {
        let t0 = origin();
        let timeout = Duration::from_secs(900);
        assert!(!waited_out_after_receipt(t0 + timeout * 2, None, timeout));
        assert!(!waited_out_after_receipt(
            t0 + timeout - MS,
            Some(t0),
            timeout
        ));
        assert!(waited_out_after_receipt(t0 + timeout, Some(t0), timeout));
    }

    /// An answered `worker_question` of the run, created at `created_at`.
    fn answered(id: i64, created_at: i64) -> Ask {
        serde_json::from_value(json!({
            "id": id, "kind": "worker_question", "task_id": 1, "run_id": "run",
            "question": "which?", "options": [], "answer": "this",
            "asked_by": "worker", "reason_category": "scope",
            "created_at": created_at, "answered_at": created_at + 1,
            "closed_at": null,
        }))
        .unwrap()
    }

    /// A revise's watch types the answers of the questions asked at or
    /// after its request and leaves the one from before it to the inbox;
    /// a watch from 0 types every answer of the run.
    #[test]
    fn a_revise_types_only_the_answers_of_its_own_questions() {
        let revised_at = 1_000;
        let answers = vec![
            answered(1, revised_at - 1),
            answered(2, revised_at),
            answered(3, revised_at + 5),
        ];
        let ids = |answers: Vec<Ask>| answers.into_iter().map(|ask| ask.id).collect::<Vec<_>>();
        assert_eq!(
            ids(followed_answers(answers.clone(), revised_at)),
            [AskId::new(2), AskId::new(3)]
        );
        assert_eq!(
            ids(followed_answers(answers, 0)),
            [AskId::new(1), AskId::new(2), AskId::new(3)]
        );
    }
}

/// A child's claim could not be handed its part of a replaced run's saved
/// work ([`Supervisor::replan_inheritance`]): a failure of that run alone,
/// which provisioning abandons with the error for its recovery while the
/// supervisor goes on claiming other tasks.
#[derive(Debug)]
pub(super) struct ReplanCarryFailed(pub(super) String);

impl std::fmt::Display for ReplanCarryFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReplanCarryFailed {}
