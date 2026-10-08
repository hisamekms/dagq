//! Resumed sessions of `needs_session` runs (ADR-0019): which runs to
//! resume, the resolution request and the [`ResumeWatch`] of the session.

use super::*;
use crate::domain::EventKind;
use crate::domain::language::with_instruction;
use crate::domain::{
    ParkCause, RunEvent, Task, required_of,
    run::{RunWorkspace, run_workspaces},
};

/// The checks a resumed session's receipt must back: the task's but
/// `e2e`, which the runtime runs itself after the review (ADR-t1233-2).
fn resume_required(task: &Task, run: &TaskRun) -> Vec<EvidenceCheck> {
    required_of(task.required_evidence(), run.actual_provider())
}

impl Supervisor<'_> {
    /// The `needs_session` runs to resume with attempts left (ADR-0019
    /// decision 1), oldest first: a run with a lease that is not stale, or
    /// whose last session still runs, is someone's already. A run out of
    /// attempts is handed on here, and one whose ended session left a
    /// stuck_exit ask has it closed, whether or not a slot is free; the
    /// fill pass resumes the rest in the order of its line (ADR-t1850-1).
    pub(super) fn resume_candidates(&mut self) -> Result<Vec<ResumeCandidate>> {
        let candidates = self.queue.runs_needing_session()?;
        // A resumed session let go after the exit timeout raised a
        // stuck_exit ask; once it ended nobody needs to answer it, whether
        // or not a slot is free.
        for candidate in &candidates {
            let alive = candidate
                .wrapper
                .as_ref()
                .is_some_and(|w| w.exited_at.is_none() && self.wrapper_lives(w));
            if !alive {
                for ask in self.queue.close_stuck_exit_asks(
                    candidate.run.id(),
                    "the session exited; closed by the runtime",
                )? {
                    info!(run_id = %candidate.run.id(), ask_id = %ask.id, "session of {} exited; closed its stuck_exit ask {}", candidate.run.id(), ask.id);
                }
            }
        }
        let mut ready = Vec::new();
        for candidate in candidates {
            let run = &candidate.run;
            if self.in_slot(run.id()) {
                continue;
            }
            if self.no_claude && run.actual_provider() == crate::domain::Provider::Claude {
                // Do not spend resume attempts trying to launch a forbidden provider.
                continue;
            }
            let now = self.generators.clock.now();
            let session_alive = candidate
                .wrapper
                .as_ref()
                .is_some_and(|w| w.exited_at.is_none() && self.wrapper_lives(w));
            // A previous session whose wrapper process lives on, however
            // silent, is never joined by a second one on the same worktree.
            if candidate
                .lease
                .as_ref()
                .is_some_and(|lease| !self.lease_stale(lease, now))
                || session_alive
            {
                continue;
            }
            // Out of attempts: the run is retried with its branch carried
            // over, or a person decides, whether or not a slot is free.
            if candidate.resumes.exhausted(self.resume_config) {
                if let Err(error) = self.exhaust_resumes(run, candidate.resumes) {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its used-up resumes could not be handed to a person: {error:#}", run.id());
                }
                continue;
            }
            ready.push(candidate);
        }
        Ok(ready)
    }
    /// Resume `candidate`, one of [`Self::resume_candidates`], in a free
    /// slot: the fill pass calls it in the order of its line (ADR-t1850-1).
    pub(super) fn resume_run(&mut self, candidate: ResumeCandidate) -> Result<()> {
        let ResumeCandidate { run, resumes, .. } = candidate;
        self.close_left_resume_workspaces(&run)?;
        let main = self.repository.main_head()?;
        // Read before the resume begins, so a failure leaves the run parked.
        let branch = self.repository.landing_branch()?.name;
        if let Some(head) = self.resolved_head(&run, &main)? {
            self.skip_resume(&run, &head, &main)?;
            return Ok(());
        }
        // The session runs the worker's checks with the worker's
        // `[run.env]`, read before the resume begins like the branch (a
        // run without a run directory fails in `start_resume`).
        let run_env = match run.run_dir() {
            Some(run_dir) => self.verifier.run_env(Path::new(run_dir))?,
            None => Vec::new(),
        };
        let (reason, kind) = resume_reason(&self.queue.run_events(run.id())?, run.last_error());
        // Not while the cleanup job clears the run's worktree (task 405).
        let cleaning = self.cleanup.cleaning();
        let mut guard = cleanup::lock_cleaning(&cleaning);
        if !guard.may_lease(run.id()) {
            self.cleanup.deferred = true;
            return Ok(());
        }
        let begun = self.queue.begin_resume(
            run.id(),
            &self.token,
            &main,
            reason.as_deref(),
            self.resume_config,
        )?;
        drop(guard);
        let Some((run, attempt)) = begun else {
            return Ok(());
        };
        self.ensure_sccache(crate::domain::sccache::CheckReason::BeforeResume);
        let request = ResumeRequest {
            main,
            branch,
            reason: reason.unwrap_or_else(|| "(no reason recorded)".to_owned()),
            kind,
        };
        match self.start_resume(&run, attempt, &request, run_env) {
            Ok(watch) => {
                info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} resumed (attempt {attempt}; {} of at most {MAX_RESUME_ATTEMPTS} counted before it) in workspace {}", run.id(), run.task_id(), resumes.counted, watch.workspace);
                self.slots.push(Slot::new(run, Phase::Resume(watch)));
            }
            Err(error) => {
                let message = format!("run {} could not be resumed: {error:#}", run.id());
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{}", message);
                self.give_up_resume(
                    &run,
                    attempt,
                    None,
                    message,
                    &reason_of_error(&error, ReasonCode::Other),
                );
            }
        }
        Ok(())
    }
    /// The worktree head of a `needs_session` run an earlier resume already
    /// resolved although it was not judged so (its session rewrote the
    /// receipt before the attempt that saw it, or went idle without
    /// rewriting it again): the run was last parked by the landing or
    /// validation (not a person's `send_back`, `landing_decided`), the last
    /// of the parking events, `resume_finished` and `resume_skipped` is a
    /// `resume_finished` with `outcome: unresolved` (a session ran; a resume
    /// that could not start changed nothing), the receipt parses with
    /// this run's `run_id`, `succeeded` and the task's required evidence,
    /// its `commit` is the head of a clean worktree, and that head has
    /// `main` as a proper ancestor. `None` whenever one of these fails or
    /// cannot be read, and the run is resumed as before. Back in
    /// `needs_session` after a skip, however it got there, the run needs a
    /// resume first, so a skip never repeats without one.
    pub(super) fn resolved_head(
        &mut self,
        run: &TaskRun,
        main: &CommitSha,
    ) -> Result<Option<CommitSha>> {
        let unresolved_since_park =
            RunHistory::from_events(&self.queue.run_events(run.id())?).unresolved_since_park();
        // A run no session ran on since its park is resumed: nothing else
        // is read for it.
        if !unresolved_since_park {
            return Ok(None);
        }
        let (Some(worktree), Some(receipt_path)) = (&run.worktree_path(), &run.receipt_path())
        else {
            return Ok(None);
        };
        let Some(receipt) = self
            .files
            .read_to_string(Path::new(receipt_path))
            .ok()
            .and_then(|text| Receipt::parse(&text).ok())
        else {
            return Ok(None);
        };
        let task = self.queue.show(run.task_id())?.task;
        let worktree = Path::new(worktree);
        let Ok(head) = self.repository.head(worktree) else {
            return Ok(None);
        };
        let clean = self
            .repository
            .status(worktree)
            .is_ok_and(|status| status.trim().is_empty());
        let main_is_ancestor = self
            .repository
            .is_ancestor(main.as_str(), head.as_str())
            .unwrap_or(false);
        let skips = skips_resume(&SkipFacts {
            unresolved_since_park,
            run_id: run.id(),
            receipt: &receipt,
            required: &resume_required(&task, run),
            head: &head,
            main,
            clean,
            main_is_ancestor,
        });
        Ok(skips.then_some(head))
    }
    /// Move a run [`Self::resolved_head`] found resolved on without opening
    /// a session or using an attempt: record `resume_skipped` and, under
    /// its lease, land it when its integrate was approved, or validate and
    /// review it (with no session to keep) otherwise.
    pub(super) fn skip_resume(
        &mut self,
        run: &TaskRun,
        head: &CommitSha,
        main: &CommitSha,
    ) -> Result<()> {
        let approved = self
            .queue
            .has_run_event(run.id(), event_kind::INTEGRATION_APPROVED)?;
        // Not while the cleanup job clears the run's build outputs (task
        // 1289): it lands or is validated from its worktree.
        let cleaning = self.cleanup.cleaning();
        let mut guard = cleanup::lock_cleaning(&cleaning);
        if !guard.may_lease(run.id()) {
            self.cleanup.deferred = true;
            return Ok(());
        }
        let skipped = self
            .queue
            .skip_resume(run.id(), &self.token, head, main, approved)?;
        drop(guard);
        let Some(run) = skipped else {
            return Ok(());
        };
        info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} was already resolved at {head} on main {main}; {} without a resume", run.id(), run.task_id(), if approved {
            "landing it"
        } else {
            "validating it"
        });
        let phase = if approved {
            self.queue_landing(&run, "resume");
            Phase::AwaitingSlot
        } else {
            Phase::Validating(Some(self.validate(run.clone())), None)
        };
        self.slots.push(Slot::new(run, phase));
        Ok(())
    }
    /// End a `needs_session` run whose resumes are used up (ADR-0047
    /// decision 24). A run whose review passed and that waits only because
    /// of a conflict with main is retried with its branch carried over, once
    /// per task ([`Self::retry_inheriting`]). Otherwise the run becomes
    /// `failed` with its `resume_exhausted` alert recorded
    /// (`recovery_requested`), and the recovery job decides what follows
    /// (ADR-0047 decision 39): resuming is no longer one of its options. The
    /// workspaces it left open are closed as after a recovery round. A run
    /// of a task that moved on is left alone.
    pub(super) fn exhaust_resumes(&mut self, run: &TaskRun, resumes: ResumeCount) -> Result<()> {
        let detail = self.queue.show(run.task_id())?;
        if detail.task.status() != TaskStatus::InProgress
            || detail
                .runs
                .last()
                .is_some_and(|latest| *latest.id() != *run.id())
        {
            return Ok(());
        }
        let last_error = run.last_error().map(str::to_owned).unwrap_or_default();
        let resumed = resumed_text(resumes, self.resume_config);
        let reason = format!(
            "{resumed} and still needs a session: {}",
            tail(&last_error, 500)
        );
        if inherits_on_exhaustion(&self.queue.run_events(run.id())?, &detail.events) {
            match self.inherited_head(run) {
                Ok(Some(head)) => return self.retry_inheriting(run, head, &reason),
                Ok(None) => {
                    info!(run_id = %run.id(), "run {}: its branch has no commit to carry over; the recovery job takes it", run.id());
                }
                Err(error) => {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its branch could not be kept for a retry: {error:#}; the recovery job takes it", run.id());
                }
            }
        }
        let Some(failed) = self.queue.exhaust_resumes(
            run.id(),
            &Exhaustion::Recover,
            &reason,
            self.resume_config,
        )?
        else {
            return Ok(());
        };
        warn!(run_id = %failed.id(), task_id = %failed.task_id(), "run {} of task {} used up its resumes; it is failed and goes to the recovery job (resume_exhausted)", failed.id(), failed.task_id());
        self.close_open_workspaces(&failed, WorkspaceCloser::Triage)?;
        Ok(())
    }
    /// The commit of the run's branch a retry carries over
    /// ([`crate::application::inherit::carried_head`]), kept under
    /// `refs/dagq/runs/<run-id>` so that it outlives the branch. `None`
    /// when the branch holds nothing on top of the run's base.
    pub(super) fn inherited_head(&mut self, run: &TaskRun) -> Result<Option<CommitSha>> {
        use crate::application::inherit::{carried_head, keep_carried_head};
        let Some(head) = carried_head(run, &*self.files, &*self.repository)? else {
            return Ok(None);
        };
        keep_carried_head(&*self.repository, run.id(), &head)?;
        Ok(Some(head))
    }
    /// Retry the task of a run whose resumes were used up on conflicts
    /// after its review passed, with its branch carried over (ADR-0047
    /// decision 24): the run becomes `failed`, the task `ready` without a
    /// plan review (its content is unchanged), and the next run's prompt
    /// asks to bring `head` onto the current main. Recorded as
    /// `triage_finished` (`action: retry_inherit`) and `auto_repaired`
    /// (`repair: inherit_retry`); the workspaces the run left open are
    /// closed as after a triage.
    fn retry_inheriting(&mut self, run: &TaskRun, head: CommitSha, reason: &str) -> Result<()> {
        let exhaustion = Exhaustion::Inherit {
            branch: run.branch().map(str::to_owned),
            head: head.clone(),
        };
        let Some(failed) =
            self.queue
                .exhaust_resumes(run.id(), &exhaustion, reason, self.resume_config)?
        else {
            return Ok(());
        };
        info!(run_id = %failed.id(), task_id = %failed.task_id(), "run {} of task {} used up its resumes on conflicts after its review passed; the task is ready again and its next run carries {head} over", failed.id(), failed.task_id());
        self.close_open_workspaces(&failed, WorkspaceCloser::Triage)?;
        self.note_triaged(&failed);
        Ok(())
    }
    /// Stop the resume wrappers earlier attempts of this run left running
    /// (a session let go after the exit timeout, or one that might have
    /// lived when a resume failed), found by the handles recorded in its
    /// `workspace_created` / `resume_finished` events (ADR-0026), and
    /// record each stop as `workspace_closed` (`by: supervisor`). The
    /// caller checked that no session of the run is alive.
    pub(super) fn close_left_resume_workspaces(&mut self, run: &TaskRun) -> Result<()> {
        let events = self.queue.run_events(run.id())?;
        let left: Vec<RunWorkspace> = run_workspaces(run, &events)
            .into_iter()
            .filter(|w| w.resume_attempt.is_some() && !w.closed)
            .collect();
        for workspace in left {
            if self.run_session_open(&workspace.workspace_id)? {
                info!(run_id = %run.id(), "run {}: stopping the resume wrapper {} left by an earlier attempt; its session has ended", run.id(), workspace.workspace_id);
                stop_session(self.cmux, &workspace.workspace_id, StopRoute::Resume)?;
                self.queue.record_workspace_closed(
                    run.id(),
                    &workspace.workspace_id,
                    json!({"resume_attempt": workspace.resume_attempt, "by": "supervisor"}),
                )?;
            }
        }
        Ok(())
    }
    /// Write the resolution request, refresh the runtime snapshot (the one
    /// the worker ran may predate `session --resume`) and start the resume
    /// wrapper in the background with the same settings and `run_env` as
    /// the worker's.
    pub(super) fn start_resume(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        request: &ResumeRequest,
        run_env: Vec<(String, String)>,
    ) -> Result<ResumeWatch> {
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        let worktree = Path::new(run.worktree_path().context("missing worktree")?);
        ensure!(
            self.files.is_dir(worktree),
            "worktree {} is missing",
            worktree.display()
        );
        let task = self.queue.show(run.task_id())?.task;
        let landed = landed_since(&mut *self.queue, &*self.repository, run, &request.main)?;
        let message = with_instruction(
            resume_request(&task, run, request, &landed)?,
            self.verifier.language().as_ref(),
        );
        self.files.write(
            &run_dir.join(format!("resume-{attempt}.txt")),
            message.as_bytes(),
        )?;
        self.files
            .copy(&self.layout.runner, &run_dir.join(RUN_RUNNER_FILE))
            .context("snapshot runtime binary")?;
        self.prepare_turns(run, &run_dir)?;
        // The resumed worker's broker token is issued again; `required`
        // resumes no worker without the tools (ADR-t838-1).
        self.broker_grant_or_refuse(run)?;
        self.warn_ignored_wrapper_setting();
        let log = self.session_log(&run_dir, Some(attempt), false);
        let command = background::wrapper_command(vec![
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
            "--resume".into(),
        ]);
        // The worker's env (the same session of the run), then `[run.env]`
        // after the runtime's own names, as for the worker's first session.
        let workspace = self
            .actors()
            .spawn(ActorExecutionSpec::new(
                ActorContext::worker(run.id(), run.task_id()),
                WorkspaceAccess::Write(PathBuf::from(
                    run.worktree_path().context("missing worktree")?,
                )),
                ActorProgram::RunSession {
                    run,
                    wrapper: command,
                    run_env,
                    log: &log,
                },
            ))?
            .workspace()?;
        // Every session of the run is recorded, so whatever ends the run
        // finds this wrapper to stop; one that cannot be is stopped now
        // (task 806).
        if let Err(error) = self.queue.record_runtime_event(
            run.id(),
            EventKind::WorkspaceCreated,
            json!({"workspace_id": workspace, "resume_attempt": attempt}),
        ) {
            return Err(match stop_session(self.cmux, &workspace, StopRoute::Resume) {
                Ok(()) => error.context(format!(
                    "the resume's background wrapper {workspace} of run {} could not be recorded and was stopped",
                    run.id()
                )),
                Err(stop) => error.context(format!(
                    "the resume's background wrapper {workspace} of run {} could not be recorded, and stopping it failed: {stop:#}",
                    run.id()
                )),
            });
        }
        self.record_launch(run, &workspace, &log, None)?;
        let now = self.generators.clock.monotonic();
        Ok(ResumeWatch {
            workspace: workspace.clone(),
            attempt,
            run_dir,
            receipt_path: PathBuf::from(run.receipt_path().context("missing receipt path")?),
            idle_marker: run.idle_marker_path()?,
            started_at: self.files.now(),
            startup: now,
            message,

            message_sent: None,
            start: None,
            exit_requested: None,

            required_evidence: resume_required(&task, run),
            approved: self
                .queue
                .has_run_event(run.id(), event_kind::INTEGRATION_APPROVED)?,
            silent: false,
            exit_for_silence: false,
            stale: None,
            delivered_closed: None,
            recovery: RecoveryWatch::default(),
            live: Box::new(SessionWatch::fixing(
                run,
                &workspace,
                self.files.now(),
                now,
            )?),
        })
    }
    /// A [`ResumeWatch`] that goes on watching a resumed session another
    /// process started: after a handoff from its `handoff.json`, after an
    /// adoption from the run's events ([`Self::adopt_resume`]). Nothing is
    /// sent or asked twice: the request only when `message_sent_at` is
    /// unknown, and never a second exit request after a recorded one.
    pub(super) fn rebuilt_resume(
        &mut self,
        run: &TaskRun,
        state: ResumeState,
    ) -> Result<ResumeWatch> {
        let ResumeState {
            workspace,
            attempt,
            started_at,
            message,
            message_sent_at,
            exit_requested,

            exit_for_silence,
            approved,
        } = state;
        let task = self.queue.show(run.task_id())?.task;
        let now = self.generators.clock.monotonic();
        // Answers are followed from the request on. A delivered answer
        // closes its ask and moves the last request time forward.
        let mut live = Box::new(SessionWatch::fixing(
            run,
            &workspace,
            message_sent_at.unwrap_or(started_at),
            now,
        )?);
        // The recovery jobs the previous process recorded during this
        // resume carry over, as for an adopted revise: the
        // anchor is the `resume_started` of the attempt (task 743).
        let events = self.queue.run_events(run.id())?;
        let anchor = events
            .iter()
            .rfind(|e| {
                e.kind == event_kind::RESUME_STARTED
                    && e.payload["attempt"].as_u64() == Some(attempt as u64)
            })
            .map(|e| e.id);
        if let Some(anchor) = anchor {
            live.adopt(&*self.queue, run, anchor)?;
        }
        // Keep the original exit request time across adoption.
        let exit_requested = exit_requested.then(|| {
            self.exit_requested_at(
                &events,
                |e| {
                    anchor.is_none_or(|anchor| e.id > anchor)
                        && e.payload["resume_attempt"].as_u64() == Some(attempt as u64)
                },
                now,
            )
            .unwrap_or(now)
        });
        Ok(ResumeWatch {
            live,
            stale: adopted_stale_nudge(&*self.queue, run, RESUME_PHASE, Some(attempt))?,
            delivered_closed: None,
            workspace,
            attempt,
            run_dir: PathBuf::from(run.run_dir().context("missing run directory")?),
            receipt_path: PathBuf::from(run.receipt_path().context("missing receipt path")?),
            idle_marker: run.idle_marker_path()?,
            started_at,
            startup: now,
            message,

            // The resume timeout runs from the send, not the takeover.
            message_sent: message_sent_at.map(|at| {
                let ago = self.files.now().duration_since(at).unwrap_or_default();
                (now.checked_sub(ago).unwrap_or(now), at)
            }),
            // Whether the session took the request is not checked again, as
            // for an adopted revise request.
            start: None,
            exit_requested,
            required_evidence: resume_required(&task, run),
            approved,
            silent: false,
            exit_for_silence,
            // A recovery job the previous process ran is gone.
            recovery: RecoveryWatch::default(),
        })
    }
    /// Rebuild the resume of an adopted `needs_session` run from its
    /// events and run files (task 356, ADR-0047 decision 24): the attempt
    /// of the last `resume_started`, the workspace its `workspace_created`
    /// recorded, the request from `resume-<attempt>.txt` (its mtime stands
    /// for the start: a receipt no newer is from before the resume), and
    /// whether the request was queued (`resume_request_sent`) and exit requested
    /// (`exit_requested` of the attempt) since.
    pub(super) fn adopt_resume(&mut self, run: &TaskRun) -> Result<ResumeWatch> {
        let events = self.queue.run_events(run.id())?;
        let (started, attempt, workspace) =
            resume_in_progress(&events).context("adopted run has no resume in progress")?;
        let since: Vec<&RunEvent> = events.iter().filter(|e| e.id > started).collect();
        let of_attempt =
            |e: &&&RunEvent| e.payload["resume_attempt"].as_u64() == Some(attempt as u64);
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let request = run_dir.join(format!("resume-{attempt}.txt"));
        let message = self.files.read_to_string(&request)?;
        let started_at = self.files.modified(&request)?;
        let message_sent_at = since
            .iter()
            .filter(|e| e.kind == RESUME_REQUEST_SENT)
            .find(of_attempt)
            .and_then(|e| e.payload["sent_at"].as_f64())
            .map(|at: f64| UNIX_EPOCH + Duration::from_secs_f64(at.max(0.0)));
        let exit = since
            .iter()
            .filter(|e| e.kind == event_kind::EXIT_REQUESTED)
            .rfind(of_attempt);
        // A silent wrapper's `/exit` follows its expiry in the same tick;
        // a silence that ended before an `/exit` for another reason does
        // not make that one a silent exit.
        let exit_for_silence = exit.is_some_and(|exit| {
            since
                .iter()
                .rev()
                .find(|e| e.id < exit.id)
                .is_some_and(|e| e.kind == event_kind::WRAPPER_HEARTBEAT_EXPIRED)
        });
        self.rebuilt_resume(
            run,
            ResumeState {
                workspace,
                attempt,
                started_at,
                message,
                message_sent_at,

                exit_requested: exit.is_some(),

                exit_for_silence,
                approved: self
                    .queue
                    .has_run_event(run.id(), event_kind::INTEGRATION_APPROVED)?,
            },
        )
    }
    /// The resumed session ended, or resolved the run: record
    /// `resume_finished` and move the run on. A resolved run whose
    /// integrate was approved has exited; its workspace is closed and it
    /// keeps its lease and waits for the landing slot. An unapproved
    /// resolved run keeps its session and lease and goes through
    /// validation and review like the worker's (ADR-0027 decision 3); a
    /// `failed` receipt ends the run; anything else leaves it
    /// `needs_session` for the next attempt, or for a human after the last.
    pub(super) fn finish_resumed_session(
        &mut self,
        slot: &mut Slot,
        attempt: usize,
        workspace: &str,
        verdict: ResumeVerdict,
    ) -> Result<Step> {
        let approved = self
            .queue
            .has_run_event(slot.run.id(), event_kind::INTEGRATION_APPROVED)?;
        let reviewed = matches!(verdict.kind, ResumeOutcome::Resolved) && !approved;
        // A session let go after the exit timeout still runs: its
        // wrapper stays, and blocks the next attempt until it ends. A
        // session going on to review keeps it until the verdict.
        let closed = verdict.closed
            || !verdict.exit_timed_out
                && !reviewed
                && match stop_run_session(self.cmux, workspace, StopRoute::Resume) {
                    Ok(()) => true,
                    Err(error) => {
                        warn!(run_id = %slot.run.id(), error = %format_args!("{error:#}"), "run {}: the resume's wrapper {workspace} could not be stopped: {error:#}", slot.run.id());
                        false
                    }
                };
        let mut payload = json!({
            "attempt": attempt,
            "outcome": verdict.outcome(),
            "head": verdict.head,
            "workspace_id": workspace,
            "workspace_closed": closed,
            "approved": approved,
        });
        if verdict.exit_timed_out {
            payload["exit_timed_out"] = json!(true);
        }
        if verdict.closed {
            payload["exit_forced_close"] = json!(true);
        }
        if reviewed {
            payload["session_live"] = json!(verdict.live);
        }
        let id = slot.run.id().clone();
        let run = match verdict.kind {
            ResumeOutcome::Resolved if approved => {
                let run = self
                    .queue
                    .finish_resume(&id, &self.token, None, None, true, payload)?;
                self.queue_landing(&run, "resume");
                slot.run = run;
                slot.phase = Phase::AwaitingSlot;
                return Ok(Step::Continue);
            }
            ResumeOutcome::Resolved => {
                let run = self.queue.finish_resume(
                    &id,
                    &self.token,
                    Some(RunStatus::Validating),
                    None,
                    true,
                    payload,
                )?;
                let handle = self.validate(run.clone());
                slot.run = run;
                // A workspace closed to go on has no session left.
                let session = (!verdict.closed).then(|| SessionRef {
                    workspace: workspace.to_owned(),
                    resume: Some(attempt),
                });
                slot.phase = Phase::Validating(Some(handle), session);
                return Ok(Step::Continue);
            }
            ResumeOutcome::Failed(reason) => self.queue.finish_resume(
                &id,
                &self.token,
                Some(RunStatus::Failed),
                Some(&reason),
                false,
                Reason::new(ReasonCode::WorkerFailed).on(payload),
            )?,
            ResumeOutcome::Unresolved => {
                payload["exhausted"] =
                    json!(resumes_exhausted(&*self.queue, &id, self.resume_config));
                self.queue
                    .finish_resume(&id, &self.token, None, None, false, payload)?
            }
        };
        Ok(Step::Done(Box::new(run)))
    }
}

/// What [`Supervisor::resolved_head`] read of a `needs_session` run to
/// judge whether an earlier resume already resolved it.
pub(super) struct SkipFacts<'a> {
    /// [`RunHistory::unresolved_since_park`]: last parked by the landing,
    /// its recheck or validation, and a session ran since, judged
    /// `unresolved`.
    pub(super) unresolved_since_park: bool,
    pub(super) run_id: &'a RunId,
    /// The run's receipt as it parses now.
    pub(super) receipt: &'a Receipt,
    /// The checks its receipt must back ([`resume_required`]).
    pub(super) required: &'a [EvidenceCheck],
    /// The head of its worktree.
    pub(super) head: &'a CommitSha,
    pub(super) main: &'a CommitSha,
    /// The worktree has no change left.
    pub(super) clean: bool,
    /// `main` is an ancestor of `head`.
    pub(super) main_is_ancestor: bool,
}

/// Whether the run is moved on without a resume
/// ([`Supervisor::skip_resume`]): a session ran since its park, its receipt
/// is this run's, `succeeded`, backs the required checks and names the
/// head of its clean worktree, and that head has `main` as a proper
/// ancestor.
pub(super) fn skips_resume(facts: &SkipFacts<'_>) -> bool {
    let receipt = facts.receipt;
    facts.unresolved_since_park
        && receipt.run_id() == facts.run_id.as_str()
        && receipt.result() == ReceiptResult::Succeeded
        && receipt.missing_evidence(facts.required).is_empty()
        && receipt.names_commit(facts.head.as_str())
        && facts.head != facts.main
        && facts.clean
        && facts.main_is_ancestor
}

/// How often the run was resumed, for its used-up reason and ask: the
/// counted resumes against [`MAX_RESUME_ATTEMPTS`], and the conflict-only
/// ones against `config`'s limit (ADR-0047 decision 24) when there were any, with the conflict
/// precheck's requests that shared their limit, and the kill-only ones
/// against [`KILL_ONLY_RESUME_LIMIT`] (ADR-t946-1).
fn resumed_text(resumes: ResumeCount, config: ResumeConfig) -> String {
    let limit = config.conflict_only_limit;
    let killed = if resumes.kill_only == 0 {
        String::new()
    } else {
        format!(
            ", {} of at most {KILL_ONLY_RESUME_LIMIT} after a signal from outside killed its session",
            resumes.kill_only
        )
    };
    if resumes.conflict_attempts() == 0 && resumes.kill_only == 0 {
        format!(
            "resumed {} times (at most {MAX_RESUME_ATTEMPTS})",
            resumes.counted
        )
    } else if resumes.conflict_attempts() == 0 {
        format!(
            "resumed {} times ({} of at most {MAX_RESUME_ATTEMPTS} counted{killed})",
            resumes.total(),
            resumes.counted,
        )
    } else if resumes.conflict_requests == 0 {
        format!(
            "resumed {} times ({} of at most {MAX_RESUME_ATTEMPTS} counted{killed}, and {} of at most {limit} for conflicts only after its review passed)",
            resumes.total(),
            resumes.counted,
            resumes.conflict_only
        )
    } else {
        format!(
            "resumed {} times ({} of at most {MAX_RESUME_ATTEMPTS} counted{killed}) and asked {} times by the conflict precheck ({} of at most {limit} attempts for conflicts only after its review passed)",
            resumes.total(),
            resumes.counted,
            resumes.conflict_requests,
            resumes.conflict_attempts()
        )
    }
}

/// Why the run waits for a session: the reason of its latest
/// `integration_deferred` / `integration_error` / `evidence_missing` /
/// `scope_violation` / `landing_decided` event (a runtime error since, such
/// as a failed resume, may have replaced `last_error`) in `events`, else
/// the run's `last_error`;
/// and what kind of request that makes: `evidence_missing` (or a landing
/// deferred for missing evidence, whose payload names the `checks`),
/// `scope_violation` (or a landing deferred for it, whose payload names the
/// paths), a review sent back, the triage's resume (`triage_finished`,
/// whose `instruction` is the reason, or a person's `triage_decided`), or a
/// landing.
pub(super) fn resume_reason(
    events: &[RunEvent],
    last_error: Option<&str>,
) -> (Option<String>, ResumeKind) {
    let parked = RunHistory::from_events(events).last_park();
    let reason = parked
        .and_then(|park| park.reason)
        .map(str::to_owned)
        .or_else(|| last_error.map(str::to_owned));
    let kind = match parked.map(|park| park.cause) {
        Some(ParkCause::EvidenceMissing) => ResumeKind::EvidenceMissing,
        Some(ParkCause::ScopeViolation) => ResumeKind::ScopeViolation,
        Some(ParkCause::SentBack) => ResumeKind::SentBack,
        Some(ParkCause::Triage) => ResumeKind::Triage,
        Some(ParkCause::Recheck) => ResumeKind::Recheck,
        Some(ParkCause::SessionGone) => ResumeKind::SessionGone,
        Some(ParkCause::E2e) => ResumeKind::E2e,
        Some(ParkCause::Landing) | None => ResumeKind::Landing,
    };
    (reason, kind)
}

/// The tasks landed on `main` since the run's base, oldest first, from the
/// `Dagq-Task` trailers, each by ID and title.
pub(super) fn landed_since(
    queue: &mut dyn Queue,
    repository: &dyn Repository,
    run: &TaskRun,
    main: &CommitSha,
) -> Result<Vec<LandedTask>> {
    let mut landed = Vec::new();
    for task_id in repository.landed_task_ids(run.base_commit().as_str(), main.as_str())? {
        let Ok(detail) = queue.show(task_id) else {
            continue;
        };
        landed.push(LandedTask {
            task_id,
            title: detail.task.title().to_owned(),
        });
    }
    Ok(landed)
}

/// The options of the `decide` ask the recovery job of a run whose resumes
/// are used up escalates to: a subset of [`TRIAGE_OPTIONS`], applied the
/// same way.
pub(super) const EXHAUSTED_OPTIONS: &[&str] = &["retry", "cancel"];

/// Watches one resumed session: its wrapper registration, the resolution
/// request once its input box is ready and whether the session took it
/// (task 285), the rewritten receipt and the idle marker, the single
/// `/exit`, and the wrapper's exit.
pub(super) struct ResumeWatch {
    pub(super) workspace: String,
    pub(super) attempt: usize,
    pub(super) run_dir: PathBuf,
    pub(super) receipt_path: PathBuf,
    pub(super) idle_marker: PathBuf,
    /// A receipt no newer than this is the one from before the resume.
    /// Like every time compared with a file's mtime (`sent_at` of a revise
    /// or conflict request, `message_sent`), it is read from the wall clock
    /// that stamps the files, not from the injected [`Clock`].
    pub(super) started_at: SystemTime,
    pub(super) startup: Instant,
    pub(super) message: String,

    /// When the resolution request was sent (for its timeout, and for the
    /// idle marker of the response to it).
    pub(super) message_sent: Option<(Instant, SystemTime)>,
    /// The queued request time used to recognize its idle marker.
    pub(super) start: Option<SystemTime>,
    pub(super) exit_requested: Option<Instant>,
    /// The task's required checks: a rewritten receipt still without them
    /// has not resolved the run.
    pub(super) required_evidence: Vec<EvidenceCheck>,
    /// Its integrate was called: resolved, it exits and lands without a
    /// review; otherwise it stays open for validation and review.
    pub(super) approved: bool,
    /// The wrapper went silent while its process lived on
    /// (`wrapper_heartbeat_expired` is recorded); cleared when its
    /// heartbeat comes back before any `/exit` (task 606).
    pub(super) silent: bool,
    /// The `/exit` was sent because of that silence.
    pub(super) exit_for_silence: bool,
    /// Idle with a receipt for an older commit: the one request of this
    /// attempt to rewrite it (task 357).
    pub(super) stale: Option<StaleNudge>,
    /// The second this watch's own delivery of an answer closed the last
    /// `worker_question` in: that close is the answer typed at `input_at`,
    /// not one delivered by hand, and moves no clock (task 931).
    pub(super) delivered_closed: Option<i64>,
    pub(super) recovery: RecoveryWatch,
    /// The session's worker questions and recovery, followed as a revise's
    /// are; `input_at` is the last queued request time.
    pub(super) live: Box<SessionWatch>,
}

/// The resume a run is in, from its events: the last resume event is a
/// `resume_started` and the workspace of its attempt was recorded
/// (`workspace_created`); its event ID, attempt and workspace.
pub(super) fn resume_in_progress(events: &[RunEvent]) -> Option<(EventId, usize, String)> {
    let started = events.iter().rev().find(|e| {
        matches!(
            e.kind.as_str(),
            event_kind::RESUME_STARTED | event_kind::RESUME_FINISHED | event_kind::RESUME_SKIPPED
        )
    })?;
    if started.kind != event_kind::RESUME_STARTED {
        return None;
    }
    let attempt = started.payload["attempt"].as_u64()?;
    let workspace = events
        .iter()
        .filter(|e| e.id > started.id && e.kind == event_kind::WORKSPACE_CREATED)
        .find(|e| e.payload["resume_attempt"].as_u64() == Some(attempt))?
        .payload["workspace_id"]
        .as_str()?
        .to_owned();
    Some((started.id, attempt as usize, workspace))
}

/// The run event recording that the resolution request of a resume was
/// typed (`resume_attempt`, `workspace_id`, `sent_at` on the files' wall clock):
/// a supervisor that adopts the resume does not send it again.
pub const RESUME_REQUEST_SENT: &str =
    crate::domain::event_kind::EventKind::ResumeRequestSent.as_str();

/// What a [`ResumeWatch`] taken over from another process starts from.
pub(super) struct ResumeState {
    pub(super) workspace: String,
    pub(super) attempt: usize,
    pub(super) started_at: SystemTime,
    pub(super) message: String,
    pub(super) message_sent_at: Option<SystemTime>,
    pub(super) exit_requested: bool,

    pub(super) exit_for_silence: bool,
    pub(super) approved: bool,
}

/// What a resumed session left behind when it exited.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ResumeOutcome {
    /// A rewritten `succeeded` receipt names the worktree head.
    Resolved,
    /// A rewritten receipt reports `failed`; the reason for `last_error`.
    Failed(String),
    /// Anything else: no rewritten receipt, or one for another commit.
    Unresolved,
}

/// The receipt at `path` the session rewrote during the resume that
/// started at `started_at`, if any: one no newer than that is the one from
/// before the resume.
pub(super) fn rewritten_receipt(
    files: &dyn RunFiles,
    path: &Path,
    started_at: SystemTime,
) -> Option<Receipt> {
    let modified = files.modified(path).ok()?;
    if modified <= started_at {
        return None;
    }
    Receipt::parse(&files.read_to_string(path).ok()?).ok()
}

/// What the `receipt` a resumed session of run `run_id` rewrote says, with
/// `head` the worktree's HEAD when the worktree is clean: resolved when it
/// is this run's, `succeeded` (not `failed`), names `head` and backs the
/// `required` checks; failed when it reports `failed`; unresolved
/// otherwise, or without one.
pub(super) fn resume_outcome(
    receipt: Option<&Receipt>,
    run_id: &RunId,
    required: &[EvidenceCheck],
    head: Option<&CommitSha>,
) -> ResumeOutcome {
    match receipt {
        Some(receipt) if receipt.run_id() != run_id.as_str() => ResumeOutcome::Unresolved,
        Some(receipt) if receipt.result() == ReceiptResult::Failed => ResumeOutcome::Failed(
            format!("session reported the run as failed: {}", receipt.summary()),
        ),
        Some(receipt)
            if head.is_some_and(|head| receipt.names_commit(head.as_str()))
                && receipt.missing_evidence(required).is_empty() =>
        {
            ResumeOutcome::Resolved
        }
        _ => ResumeOutcome::Unresolved,
    }
}

pub(super) struct ResumeVerdict {
    pub(super) kind: ResumeOutcome,
    pub(super) head: Option<CommitSha>,
    /// The session did not exit within the exit timeout of `/exit`: it is
    /// let go (still running, its workspace kept) so the slot and the lease
    /// are not held forever.
    pub(super) exit_timed_out: bool,
    /// The session resolved the run and is still running, never asked to
    /// exit: it goes on to validation and review (ADR-0027 decision 3).
    pub(super) live: bool,
    /// The session held its `/exit` back through its retries and its
    /// workspace was closed to go on (ADR-0047 decision 25): it is not
    /// closed again.
    pub(super) closed: bool,
}

impl ResumeVerdict {
    pub(super) fn outcome(&self) -> &'static str {
        match self.kind {
            ResumeOutcome::Resolved => "resolved",
            ResumeOutcome::Failed(_) => "failed",
            ResumeOutcome::Unresolved => "unresolved",
        }
    }
}

/// What a session asked to exit does while it has not exited.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ExitWait {
    /// Within the exit timeout: the exit is waited for.
    Waiting,
    /// The exit timeout passed: the session is let go, still running, so
    /// the slot and the lease are not held forever.
    LetGo,
}

/// What a resumed session asked at `requested` to exit does at `now`: it
/// is waited for until the exit `timeout`, and let go at it.
pub(super) fn exit_wait(now: Instant, requested: Instant, timeout: Duration) -> ExitWait {
    if passed(now, requested, timeout) {
        ExitWait::LetGo
    } else {
        ExitWait::Waiting
    }
}

/// Why a resumed session whose resolution request was sent at `sent` (or
/// last restarted by an input) is asked to exit at `now` for its time: the
/// resume `timeout` passed and no request to rewrite a stale receipt
/// still waits for its answer (`stale_waits`, which has its own timeout).
/// `None` while it is waited for.
pub(super) fn resume_deadline(
    now: Instant,
    sent: Instant,
    timeout: Duration,
    stale_waits: bool,
) -> Option<&'static str> {
    (passed(now, sent, timeout) && !stale_waits)
        .then_some("did not finish within the resume timeout")
}

impl ResumeWatch {
    /// Record the exit request before writing its file, and stop live recovery.
    fn request_exit(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        sv.queue.record_runtime_event(
            run.id(),
            EventKind::ExitRequested,
            json!({"workspace_id": self.workspace, "resume_attempt": self.attempt}),
        )?;
        self.end_live(sv, run)?;
        self.exit_requested = Some(sv.generators.clock.monotonic());
        submit(sv, run, &self.workspace, Input::Exit, "exit request")?;
        Ok(())
    }

    /// The resumed session's processes before its `/exit` (task 469): the
    /// `idle_process` alert. The resume ends at its timeout by itself, so
    /// an escalation is left to that ([`leave_idle_to_phase`]).
    fn watch_idle_processes(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        let live = Live {
            workspace: &self.workspace,
            run_dir: &self.run_dir,
            allowed: &IDLE_PROCESS_ACTIONS,

            at_prompt: false,

            park: false,
        };
        if let LiveStep::Escalate(attempt, escalation) =
            self.live
                .recovery
                .watch_idle(sv, run, &live, RESUME_PHASE)?
        {
            leave_idle_to_phase(sv, run, attempt, &escalation, RESUME_PHASE)?;
        }
        Ok(())
    }

    /// The stage ends: the dialog recorded during it is cleared and its
    /// recovery job stopped, as a revise's.
    fn end_live(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        self.live.recovery.stop(sv, run);
        // A `stalled` ask of a send it did not take ends with the stage,
        // and so does the send's detection.
        self.live.stall.ended(sv, run)?;
        Ok(())
    }

    /// Start the stage's clocks again (ADR-0071 decision 15): the resume
    /// timeout of the request, or before it the wait for a ready input
    /// box, and the timeout of a stale-receipt request not settled yet,
    /// with the idle that answers it, from `from`. Nothing is carried over.
    /// An input typed into the session restarts them from when it was
    /// typed, not from after the send returned: a session that answered it
    /// at once wrote its idle marker in between, and a later start left
    /// that idle unseen until the request's timeout (task 931). `now` is
    /// the supervisor's monotonic clock.
    pub(super) fn restart_clocks(&mut self, from: SystemTime, now: Instant) {
        if let Some((sent, _)) = &mut self.message_sent {
            *sent = now;
        }
        if let Some(nudge) = &mut self.stale
            && !nudge.settled
        {
            nudge.at = from;
        }
    }

    /// Record how the request to rewrite a stale receipt ended, once the
    /// attempt ends with the session alive: `rewritten` when the receipt
    /// changed after it was typed, even while the run waited and its clock
    /// was restarted.
    fn settle_stale(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        let Some(nudge) = &mut self.stale else {
            return Ok(());
        };
        let rewritten = nudge.rewritten(&*sv.files, &self.receipt_path);
        let outcome = if rewritten { "rewritten" } else { "unchanged" };
        nudge.settle(sv, run, RESUME_PHASE, Some(self.attempt), outcome)
    }

    /// `head` is the worktree's HEAD when the worktree is clean, `None`
    /// otherwise: a resolved receipt must name a clean head.
    pub(super) fn verdict(
        &self,
        files: &dyn RunFiles,
        run: &TaskRun,
        head: Option<&CommitSha>,
    ) -> ResumeOutcome {
        resume_outcome(
            rewritten_receipt(files, &self.receipt_path, self.started_at).as_ref(),
            run.id(),
            &self.required_evidence,
            head,
        )
    }

    /// Send the resolution request once the agent's input box has shown
    /// ready on every screen read for `resume_prompt_delay` (task 285): a
    /// request typed while Claude Code boots is lost. A box not ready
    /// within the registration timeout of the agent is recorded as
    /// `input_not_ready` and raised to the inbox once; the request still
    /// goes when it gets ready, and past the resume timeout the session is
    /// asked to exit like one that did not finish.
    fn send_when_ready(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        self.send_request(sv, run)
    }

    /// Send the resolution request, record it and watch whether the
    /// session took it.
    fn send_request(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        let sent_at = sv.files.now();
        let message = self.message.clone();
        let _submission = submit(
            sv,
            run,
            &self.workspace,
            Input::Text(&message),
            "resolution request",
        )?;
        self.message_sent = Some((sv.generators.clock.monotonic(), sent_at));
        self.live.input_at = Some(sent_at);
        // A supervisor that adopts this resume does not send it again; a
        // record that fails is only noted.
        if let Err(error) = sv.queue.record_runtime_event(
            run.id(),
            EventKind::ResumeRequestSent,
            json!({
                "resume_attempt": self.attempt,
                "workspace_id": self.workspace,
                "sent_at": sent_at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64(),
            }),
        ) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{RESUME_REQUEST_SENT} of {} could not be recorded: {error:#}", run.id());
        }
        self.start = Some(sent_at);
        info!(run_id = %run.id(), "resolution request sent to run {} in workspace {}", run.id(), self.workspace);
        Ok(())
    }

    /// One observation; `Some` once the wrapper exited.
    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<ResumeVerdict>> {
        let processes = sv.queue.processes(run.id())?;
        let Some(wrapper) = processes.iter().find(|p| p.role == "wrapper") else {
            let timeout = sv.cmux.registration_timeout();
            ensure!(
                registration_pending(sv.generators.clock.monotonic(), self.startup, timeout),
                "resumed session's wrapper did not register within {} seconds",
                timeout.as_secs()
            );
            return Ok(None);
        };
        let worktree = Path::new(run.worktree_path().context("missing worktree")?);
        if wrapper.exited_at.is_some() {
            // It exited during the retries of its /exit: they repaired it.
            let _workspace = self.workspace.clone();
            // Nobody needs to send anything to a session that exited.
            self.recovery.stop(sv, run);
            self.live.recovery.stop(sv, run);
            self.live.stall.ended(sv, run)?;

            if let Some(nudge) = &mut self.stale {
                nudge.settle(sv, run, RESUME_PHASE, Some(self.attempt), "run_ended")?;
            }
            let head = sv.repository.head(worktree).ok();
            let clean = sv
                .repository
                .status(worktree)
                .is_ok_and(|status| status.trim().is_empty());
            return Ok(Some(ResumeVerdict {
                kind: self.verdict(&*sv.files, run, head.as_ref().filter(|_| clean)),
                head,
                exit_timed_out: false,
                live: false,
                closed: false,
            }));
        }
        let pulse = wrapper_pulse(
            sv,
            run,
            wrapper,
            &self.workspace,
            &mut self.silent,
            "resumed session's wrapper heartbeat expired; session may still be alive",
        )?;
        if matches!(pulse, WrapperPulse::Exited) {
            return Ok(None);
        }
        if matches!(pulse, WrapperPulse::Fresh) && self.exit_requested.is_none() {
            // A silence that ended before any /exit is over: the session
            // may wait again, and a later silence is recorded again (task
            // 606).
            self.silent = false;
        }
        if matches!(pulse, WrapperPulse::Silent) && self.exit_requested.is_none() {
            // Ask once, the way a person would; never kill the session.
            self.request_exit(sv, run)?;
            warn!(run_id = %run.id(), "resumed session of {} lost its wrapper heartbeat; exit requested", run.id());
            self.exit_for_silence = true;
        }
        if let Some(requested) = self.exit_requested {
            let now = sv.generators.clock.monotonic();
            if let ExitWait::Waiting = exit_wait(now, requested, sv.cmux.exit_timeout()) {
                return Ok(None);
            }
            return Ok(Some(ResumeVerdict {
                kind: ResumeOutcome::Unresolved,
                head: sv.repository.head(worktree).ok(),
                exit_timed_out: true,
                live: false,
                closed: false,
            }));
        }
        let Some((_, sent_at)) = self.message_sent else {
            // A headless session starts no agent before its first request.
            if processes.iter().any(|p| p.role == "wrapper") {
                self.send_when_ready(sv, run)?;
            }
            return Ok(None);
        };
        // An answer to a question the session asked during the resume is
        // typed once it went idle at it: the session works again, and gets
        // the resume timeout again (ADR-0071 decision 17, as a revise's).
        // A login or usage limit a person fixed (task 437): the session is
        // told to go on, like an answer typed into it.
        if let Some(typed) = self.live.continue_after_hold(sv, run)? {
            self.live.input_at = Some(typed);
            self.restart_clocks(typed, sv.generators.clock.monotonic());
            self.start = self.live.answer_start.take();
        }
        if let Some(typed) = self.live.deliver_answers(sv, run)? {
            self.live.input_at = Some(typed);
            self.restart_clocks(typed, sv.generators.clock.monotonic());
            self.delivered_closed = sv.queue.last_worker_question_closed(run.id())?;
            self.start = self.live.answer_start.take();
        }
        // A session stopped at its own question waits for its answer,
        // however long a person takes: it neither went idle without a
        // resolving receipt nor ran out of time, and is not asked to
        // rewrite a stale receipt (ADR-0071 decision 16).
        if sv.queue.has_unclosed_worker_question(run.id())? {
            return Ok(None);
        }
        // A turn at its provider's wall (ADR-t813-2): the call went to the
        // other provider, or the run waits in the hold ask.
        match self.live.provider_wall(sv, run)? {
            WallGate::Held => return Ok(None),
            WallGate::Moved(_) => {
                self.restart_clocks(sv.files.now(), sv.generators.clock.monotonic());
            }
            WallGate::Open => (),
        }
        self.watch_idle_processes(sv, run)?;
        // An answer delivered by hand (or by the supervisor this one took
        // the run over from) is input too: its close, in a later second
        // than the last input, moves the last input there. The close of an
        // answer this watch typed is recorded after the send, often in a
        // later second: it is that input, and restarting the clocks at it
        // would leave unseen an idle the session wrote right after the
        // answer (task 931).
        let input_at = self.live.input_at.unwrap_or(sent_at);
        if let Some(closed) = sv.queue.last_worker_question_closed(run.id())?
            && closed > unix_seconds(input_at)
            && self.delivered_closed.is_none_or(|own| closed > own)
        {
            self.live.input_at = Some(UNIX_EPOCH + Duration::from_secs(closed.max(0) as u64));
            self.restart_clocks(sv.files.now(), sv.generators.clock.monotonic());
        }
        let input_at = self.live.input_at.unwrap_or(sent_at).max(sent_at);
        // The idle marker is read before the receipt and the
        // worktree: a receipt rewritten after this read is judged
        // at the next poll, never as idle without it.
        let idle = sv.session_idle(&self.idle_marker)?;
        let head = sv.repository.head(worktree)?;
        let clean = sv.repository.status(worktree)?.trim().is_empty();
        // Resolved (or failed) and idle after the receipt; or idle
        // after the request with no such receipt, which a session
        // that could not resolve it (or stopped at a question)
        // never ends by itself; or no idle at all within the
        // resume timeout (a lost request, a dialog, background
        // work that does not end).
        let verdict = self.verdict(&*sv.files, run, clean.then_some(&head));
        let idle_after_receipt = match (&idle, &verdict) {
            (Some(idle), ResumeOutcome::Resolved | ResumeOutcome::Failed(_)) => idle
                .idle_after_receipt(&*sv.files, &self.receipt_path)?
                .is_some(),
            _ => false,
        };
        // A session idle after its receipt moved on by itself: a `stalled`
        // ask is closed as such before the stage ends, as a revise's, even
        // when its idle marker was written after this poll followed the ask
        // (task 771).
        if idle_after_receipt {
            self.live.stall.settle(sv, run, true)?;
        }
        // An unapproved resolved run keeps its session for
        // validation and review (ADR-0027 decision 3).
        if matches!(verdict, ResumeOutcome::Resolved) && !self.approved && idle_after_receipt {
            self.settle_stale(sv, run)?;
            self.end_live(sv, run)?;
            info!(run_id = %run.id(), "resumed session of {} rewrote its receipt and went idle (head {head}); validating with the session open", run.id());
            return Ok(Some(ResumeVerdict {
                kind: ResumeOutcome::Resolved,
                head: Some(head),
                exit_timed_out: false,
                live: true,
                closed: false,
            }));
        }
        // Once the session was asked to rewrite a stale receipt, only an
        // idle after that request answers it; and only one after the last
        // input typed (an answer) is this turn's.
        let answered_from = self.stale.map_or(input_at, |n| n.at.max(input_at));
        let why = match verdict {
            ResumeOutcome::Unresolved
                if idle
                    .as_ref()
                    .is_some_and(|idle| idle.idle_since(answered_from)) =>
            {
                if self.stale.is_none()
                    && let Some(stale) = stale_receipt(sv, run)
                {
                    let workspace = self.workspace.clone();
                    if let Some((nudge, start)) = nudge_stale_receipt(
                        sv,
                        run,
                        &workspace,
                        RESUME_PHASE,
                        Some(self.attempt),
                        &stale,
                    )? {
                        self.stale = Some(nudge);
                        self.start = Some(start);
                        return Ok(None);
                    }
                }
                Some("went idle without a resolving receipt")
            }
            ResumeOutcome::Unresolved => None,
            _ => idle_after_receipt.then_some("rewrote its receipt and went idle"),
        }
        .or_else(|| {
            // The request's clock, restarted by the inputs above.
            let (sent, _) = self.message_sent?;
            // A request to rewrite a stale receipt gets its own timeout.
            let stale_waits = self
                .stale
                .is_some_and(|n| !n.settled && !n.waited_out(&*sv.files, sv.cmux));
            resume_deadline(
                sv.generators.clock.monotonic(),
                sent,
                sv.cmux.resume_timeout(),
                stale_waits,
            )
        });
        if let Some(why) = why {
            self.settle_stale(sv, run)?;
            // Ask once, the way a person would; never kill the session.
            self.request_exit(sv, run)?;
            info!(run_id = %run.id(), "resumed session of {} {why} (head {head}); exit requested", run.id());
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskId;

    const MS: Duration = Duration::from_millis(1);

    /// A resumed session asked to exit is waited for until its exit
    /// timeout and let go at it, not a millisecond before.
    #[test]
    fn a_resumed_session_asked_to_exit_is_let_go_at_its_exit_timeout() {
        let requested = Instant::now();
        let timeout = Duration::from_secs(120);
        assert_eq!(exit_wait(requested, requested, timeout), ExitWait::Waiting);
        assert_eq!(
            exit_wait(requested + timeout - MS, requested, timeout),
            ExitWait::Waiting
        );
        assert_eq!(
            exit_wait(requested + timeout, requested, timeout),
            ExitWait::LetGo
        );
    }

    /// A resumed session is asked to exit at its resume timeout since the
    /// request (or the input that restarted it), not a millisecond before,
    /// and not while a request to rewrite a stale receipt waits for its
    /// own answer.
    #[test]
    fn a_resume_is_asked_to_exit_at_its_timeout_unless_a_stale_request_waits() {
        let sent = Instant::now();
        let timeout = Duration::from_secs(1800);
        assert_eq!(
            resume_deadline(sent + timeout - MS, sent, timeout, false),
            None
        );
        assert_eq!(
            resume_deadline(sent + timeout, sent, timeout, false),
            Some("did not finish within the resume timeout")
        );
        assert_eq!(resume_deadline(sent + timeout, sent, timeout, true), None);
        let restarted = sent + timeout - MS;
        assert_eq!(
            resume_deadline(sent + timeout, restarted, timeout, false),
            None
        );
    }

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new("r").unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: format!("t{id}"),
            actor: None,
        }
    }

    fn sha(c: char) -> CommitSha {
        CommitSha::parse(c.to_string().repeat(40), "commit").unwrap()
    }

    /// A receipt of `run_id` with `result` for `commit`, whose subagent
    /// review is passed with evidence when `reviewed`.
    fn receipt(run_id: &str, result: &str, commit: &CommitSha, reviewed: bool) -> Receipt {
        Receipt::parse(&receipt_text(run_id, result, commit, reviewed)).unwrap()
    }

    fn receipt_text(run_id: &str, result: &str, commit: &CommitSha, reviewed: bool) -> String {
        let check = |status: &str| json!({"status": status, "evidence_or_reason": "why"});
        json!({
                "run_id": run_id,
                "result": result,
                "commit": commit,
                "tests": check("passed"),
                "e2e": check("not_applicable"),
                "subagent_review": check(if reviewed { "passed" } else { "not_applicable" }),
                "summary": "gave up",
        })
        .to_string()
    }

    /// Task 122: a run an earlier resume already resolved is moved on
    /// without a session only when every condition holds; each one missing
    /// has it resumed as before (the integration test of the skip covers
    /// the wiring: `runtime_resume::a_run_with_a_dirty_worktree_is_resumed`).
    #[test]
    fn a_run_is_skipped_only_when_every_condition_of_the_skip_holds() {
        let run = RunId::new("r").unwrap();
        let other = RunId::new("another-run").unwrap();
        let (head, main, old) = (sha('b'), sha('a'), sha('c'));
        let resolved = receipt("r", "succeeded", &head, false);
        let base = SkipFacts {
            unresolved_since_park: true,
            run_id: &run,
            receipt: &resolved,
            required: &[],
            head: &head,
            main: &main,
            clean: true,
            main_is_ancestor: true,
        };
        assert!(skips_resume(&base));
        let failed = receipt("r", "failed", &head, false);
        let stale = receipt("r", "succeeded", &old, false);
        let at_main = receipt("r", "succeeded", &main, false);
        let cases: [(&str, SkipFacts<'_>); 9] = [
            // Not resumed since it was parked, or a person sent it back
            // (`RunHistory::unresolved_since_park`'s own unit test).
            (
                "no unresolved resume since a system park",
                SkipFacts {
                    unresolved_since_park: false,
                    ..base
                },
            ),
            (
                "the receipt is another run's",
                SkipFacts {
                    run_id: &other,
                    ..base
                },
            ),
            (
                "the receipt reports failed",
                SkipFacts {
                    receipt: &failed,
                    ..base
                },
            ),
            (
                "the required evidence is missing",
                SkipFacts {
                    required: &[EvidenceCheck::SubagentReview],
                    ..base
                },
            ),
            (
                "the receipt names the old head",
                SkipFacts {
                    receipt: &stale,
                    ..base
                },
            ),
            (
                "the head is main itself",
                SkipFacts {
                    receipt: &at_main,
                    head: &main,
                    ..base
                },
            ),
            (
                "the worktree is dirty",
                SkipFacts {
                    clean: false,
                    ..base
                },
            ),
            (
                "main moved past the head",
                SkipFacts {
                    main_is_ancestor: false,
                    ..base
                },
            ),
            (
                "unresolved and dirty",
                SkipFacts {
                    unresolved_since_park: false,
                    clean: false,
                    ..base
                },
            ),
        ];
        for (case, facts) in cases {
            assert!(!skips_resume(&facts), "{case}");
        }
        // The required evidence backed by the receipt does not stop it.
        let reviewed = receipt("r", "succeeded", &head, true);
        assert!(skips_resume(&SkipFacts {
            receipt: &reviewed,
            required: &[EvidenceCheck::SubagentReview],
            ..base
        }));
    }

    /// The used-up reason and ask say how the run was resumed: the counted
    /// resumes alone, or with the kill-only ones, the conflict-only ones
    /// and the conflict precheck's requests that share their limit.
    #[test]
    fn resumed_text_counts_every_kind_of_resume() {
        let text = |counted, conflict_only, kill_only, conflict_requests| {
            resumed_text(
                ResumeCount {
                    counted,
                    conflict_only,
                    kill_only,
                    conflict_requests,
                    parked_for_conflict: false,
                },
                ResumeConfig::default(),
            )
        };
        assert_eq!(text(3, 0, 0, 0), "resumed 3 times (at most 3)");
        assert_eq!(
            text(1, 0, 3, 0),
            "resumed 4 times (1 of at most 3 counted, 3 of at most 3 after a signal from outside killed its session)"
        );
        assert_eq!(
            text(0, 5, 0, 0),
            "resumed 5 times (0 of at most 3 counted, and 5 of at most 5 for conflicts only after its review passed)"
        );
        assert_eq!(
            text(1, 2, 1, 0),
            "resumed 4 times (1 of at most 3 counted, 1 of at most 3 after a signal from outside killed its session, and 2 of at most 5 for conflicts only after its review passed)"
        );
        assert_eq!(
            text(0, 2, 0, 3),
            "resumed 2 times (0 of at most 3 counted) and asked 3 times by the conflict precheck (5 of at most 5 attempts for conflicts only after its review passed)"
        );
        assert_eq!(
            resumed_text(
                ResumeCount {
                    conflict_only: 8,
                    ..ResumeCount::default()
                },
                ResumeConfig {
                    conflict_only_limit: 8
                }
            ),
            "resumed 8 times (0 of at most 3 counted, and 8 of at most 8 for conflicts only after its review passed)"
        );
    }

    /// The resolution request asks for what parked the run, with the
    /// reason of the latest parking event or, without one, `last_error`.
    #[test]
    fn resume_reason_is_the_latest_park_or_the_last_error() {
        let of =
            |kind: &str, payload: Value| resume_reason(&[event(1, kind, payload)], Some("last"));
        assert_eq!(
            resume_reason(&[], Some("last")),
            (Some("last".to_owned()), ResumeKind::Landing)
        );
        assert_eq!(resume_reason(&[], None), (None, ResumeKind::Landing));
        assert_eq!(
            of("integration_deferred", json!({"reason": "conflict"})),
            (Some("conflict".to_owned()), ResumeKind::Landing)
        );
        // A parking event without a reason falls back to `last_error`.
        assert_eq!(
            of("evidence_missing", json!({})),
            (Some("last".to_owned()), ResumeKind::EvidenceMissing)
        );
        for (kind, payload, expected) in [
            (
                "scope_violation",
                json!({"reason": "r"}),
                ResumeKind::ScopeViolation,
            ),
            (
                "landing_decided",
                json!({"reason": "r"}),
                ResumeKind::SentBack,
            ),
            (
                "triage_finished",
                json!({"instruction": "r"}),
                ResumeKind::Triage,
            ),
            ("triage_decided", json!({"reason": "r"}), ResumeKind::Triage),
            (
                "landing_recheck_failed",
                json!({"action": "resumed", "reason": "r"}),
                ResumeKind::Recheck,
            ),
            (
                "session_gone_parked",
                json!({"reason": "r"}),
                ResumeKind::SessionGone,
            ),
            ("run_e2e_failed", json!({"reason": "r"}), ResumeKind::E2e),
        ] {
            assert_eq!(
                of(kind, payload),
                (Some("r".to_owned()), expected),
                "{kind}"
            );
        }
        // The latest park decides, not a later event that parks nothing.
        let events = [
            event(1, "evidence_missing", json!({"reason": "first"})),
            event(2, "integration_deferred", json!({"reason": "second"})),
            event(3, "resume_finished", json!({"outcome": "error"})),
        ];
        assert_eq!(
            resume_reason(&events, Some("resume failed")),
            (Some("second".to_owned()), ResumeKind::Landing)
        );
    }

    /// Only a receipt rewritten after the resume started is judged: one of
    /// this run, `failed`, ends the run with its summary; one that names the
    /// clean head with the required evidence resolves it; anything else
    /// (another run's, another commit, a dirty worktree, missing evidence,
    /// none or unreadable) leaves it unresolved.
    #[test]
    fn a_resumed_session_is_judged_by_the_receipt_it_rewrote() {
        use crate::application::memory_files::MemoryFiles;
        let files = MemoryFiles::default();
        let path = Path::new("/run/receipt.json");
        let started = files.now();
        let later = started + Duration::from_secs(1);
        let run = RunId::new("r").unwrap();
        let head = sha('b');
        let judge =
            |text: String, at: SystemTime, head: Option<&CommitSha>, required: &[EvidenceCheck]| {
                files.put(path, at, &text);
                resume_outcome(
                    rewritten_receipt(&files, path, started).as_ref(),
                    &run,
                    required,
                    head,
                )
            };
        let text = |run_id: &str, result: &str, commit: &CommitSha| {
            receipt_text(run_id, result, commit, false)
        };
        let resolved = text("r", "succeeded", &head);
        assert_eq!(
            judge(resolved.clone(), later, Some(&head), &[]),
            ResumeOutcome::Resolved
        );
        // The receipt from before the resume, however good.
        assert_eq!(
            judge(resolved.clone(), started, Some(&head), &[]),
            ResumeOutcome::Unresolved
        );
        assert_eq!(
            judge(resolved.clone(), later, None, &[]),
            ResumeOutcome::Unresolved
        );
        assert_eq!(
            judge(
                resolved,
                later,
                Some(&head),
                &[EvidenceCheck::SubagentReview]
            ),
            ResumeOutcome::Unresolved
        );
        assert_eq!(
            judge(text("r", "succeeded", &sha('c')), later, Some(&head), &[]),
            ResumeOutcome::Unresolved
        );
        assert_eq!(
            judge(text("r", "failed", &head), later, None, &[]),
            ResumeOutcome::Failed("session reported the run as failed: gave up".to_owned())
        );
        // Another run's failed receipt is not this run's failure.
        assert_eq!(
            judge(text("other", "failed", &head), later, Some(&head), &[]),
            ResumeOutcome::Unresolved
        );
        assert_eq!(
            judge("{not json".to_owned(), later, Some(&head), &[]),
            ResumeOutcome::Unresolved
        );
        files.remove_file(path).unwrap();
        assert!(rewritten_receipt(&files, path, started).is_none());
    }

    /// The resume a run is in needs its `resume_started` to be the last
    /// resume event and the workspace of that attempt recorded after it.
    #[test]
    fn the_resume_in_progress_is_the_last_started_one_with_its_workspace() {
        let first = [
            event(1, "resume_started", json!({"attempt": 1})),
            event(
                2,
                "workspace_created",
                json!({"workspace_id": "w1", "resume_attempt": 1}),
            ),
        ];
        assert_eq!(
            resume_in_progress(&first),
            Some((EventId::new(1), 1, "w1".to_owned()))
        );
        let mut finished = first.to_vec();
        finished.push(event(3, "resume_finished", json!({"attempt": 1})));
        assert_eq!(resume_in_progress(&finished), None);
        // A second attempt whose workspace was never recorded: the first
        // attempt's workspace is not taken for it.
        finished.push(event(4, "resume_started", json!({"attempt": 2})));
        assert_eq!(resume_in_progress(&finished), None);
        finished.push(event(
            5,
            "workspace_created",
            json!({"workspace_id": "w2", "resume_attempt": 2}),
        ));
        assert_eq!(
            resume_in_progress(&finished),
            Some((EventId::new(4), 2, "w2".to_owned()))
        );
        assert_eq!(resume_in_progress(&[]), None);
    }
}
