//! Resumed sessions of `needs_session` runs (ADR-0019): which runs to
//! resume, the resolution request and the [`ResumeWatch`] of the session.

use super::*;
use crate::domain::language::with_instruction;
use crate::domain::{
    ParkCause, RunEvent, required_of,
    run::{RunWorkspace, run_workspaces},
};

impl Supervisor<'_> {
    /// Resume `needs_session` runs with attempts left (ADR-0019 decision 1),
    /// oldest first, while slots are free: a run with a lease that is not
    /// stale, or whose last session still runs, is someone's already.
    pub(super) fn resume_parked_runs(&mut self, parallel: usize) -> Result<()> {
        let candidates = self.queue.runs_needing_session()?;
        // A resumed session let go after the exit timeout raised a
        // stuck_exit ask; once it ended nobody needs to answer it, whether
        // or not a slot is free.
        for candidate in &candidates {
            let alive = candidate
                .wrapper
                .as_ref()
                .is_some_and(|w| w.exited_at.is_none() && self.processes.alive(w.pid));
            if !alive {
                for ask in self
                    .queue
                    .close_stuck_exit_asks(candidate.run.id(), STUCK_EXIT_CLOSED)?
                {
                    info!(run_id = %candidate.run.id(), ask_id = %ask.id, "session of {} exited; closed its stuck_exit ask {}", candidate.run.id(), ask.id);
                }
            }
        }
        for candidate in candidates {
            let ResumeCandidate {
                run,
                lease,
                wrapper,
                resumes,
            } = candidate;
            let now = self.generators.clock.now();
            let session_alive = wrapper
                .as_ref()
                .is_some_and(|w| w.exited_at.is_none() && self.processes.alive(w.pid));
            // A previous session whose wrapper process lives on, however
            // silent, is never joined by a second one on the same worktree.
            if lease.is_some_and(|lease| !self.lease_stale(&lease, now)) || session_alive {
                continue;
            }
            // Out of attempts: the run is retried with its branch carried
            // over, or a person decides, whether or not a slot is free.
            if resumes.exhausted(self.resume_config) {
                if let Err(error) = self.exhaust_resumes(&run, resumes) {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its used-up resumes could not be handed to a person: {error:#}", run.id());
                }
                continue;
            }
            if self.used_slots() >= parallel {
                break;
            }
            self.close_left_resume_workspaces(&run)?;
            let main = self.repository.main_head()?;
            // Read before the resume begins, so a failure leaves the run parked.
            let branch = self.repository.landing_branch()?.name;
            if let Some(head) = self.resolved_head(&run, &main)? {
                self.skip_resume(&run, &head, &main)?;
                continue;
            }
            // The session runs the worker's checks with the worker's
            // `[run.env]`, read before the resume begins like the branch (a
            // run without a run directory fails in `start_resume`).
            let run_env = match run.run_dir() {
                Some(run_dir) => self.verifier.run_env(Path::new(run_dir))?,
                None => Vec::new(),
            };
            let (reason, kind) = resume_reason(&*self.queue, &run)?;
            // Not while the cleanup job clears the run's worktree (task 405).
            let cleaning = self.cleanup.cleaning();
            let guard = cleanup::lock_cleaning(&cleaning);
            if guard.contains(run.id()) {
                self.cleanup.deferred = true;
                continue;
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
                continue;
            };
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
        if !RunHistory::from_events(&self.queue.run_events(run.id())?).unresolved_since_park() {
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
        if receipt.run_id() != run.id().as_str()
            || receipt.result() != ReceiptResult::Succeeded
            || !receipt
                .missing_evidence(&required_of(
                    task.required_evidence(),
                    run.actual_provider(),
                ))
                .is_empty()
        {
            return Ok(None);
        }
        let worktree = Path::new(worktree);
        let Ok(head) = self.repository.head(worktree) else {
            return Ok(None);
        };
        let resolved = receipt.names_commit(head.as_str())
            && head != *main
            && self
                .repository
                .status(worktree)
                .is_ok_and(|status| status.trim().is_empty())
            && self
                .repository
                .is_ancestor(main.as_str(), head.as_str())
                .unwrap_or(false);
        Ok(resolved.then_some(head))
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
        let Some(run) = self
            .queue
            .skip_resume(run.id(), &self.token, head, main, approved)?
        else {
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
    /// The commit of the run's branch a retry carries over: its validated
    /// commit (the one its review passed), or without one the head of its
    /// worktree when no rebase is stopped half way there, kept
    /// under `refs/dagq/runs/<run-id>` so that it outlives the branch.
    /// `None` when the branch holds nothing on top of the run's base.
    pub(super) fn inherited_head(&mut self, run: &TaskRun) -> Result<Option<CommitSha>> {
        // The reviewed commit, not whatever an unresolved session left in
        // the worktree (a rebase stopped half way, say).
        let head = match (run.result_commit(), run.worktree_path().map(Path::new)) {
            (Some(commit), _) => Some(commit.clone()),
            (None, Some(worktree))
                if self.files.is_dir(worktree)
                    && !self.repository.rebase_in_progress(worktree)? =>
            {
                Some(self.repository.head(worktree)?)
            }
            _ => None,
        };
        let Some(head) = head.filter(|head| head != run.base_commit()) else {
            return Ok(None);
        };
        self.repository
            .update_ref(&format!("refs/dagq/runs/{}", run.id()), head.as_str())?;
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
    /// Close the resume workspaces earlier attempts of this run left open
    /// (a session let go after the exit timeout, or one that might have
    /// lived when a resume failed), found by the IDs recorded in its
    /// `workspace_created` / `resume_finished` events (ADR-0026), and
    /// record each close as `workspace_closed` (`by: supervisor`). The
    /// caller checked that no session of the run is alive.
    pub(super) fn close_left_resume_workspaces(&mut self, run: &TaskRun) -> Result<()> {
        let events = self.queue.run_events(run.id())?;
        let left: Vec<RunWorkspace> = run_workspaces(run, &events)
            .into_iter()
            .filter(|w| w.resume_attempt.is_some() && !w.closed)
            .collect();
        for workspace in left {
            if self.cmux.exists(&workspace.workspace_id)? {
                info!(run_id = %run.id(), "run {}: closing resume workspace {} left by an earlier attempt; its session has ended", run.id(), workspace.workspace_id);
                self.cmux.close(&workspace.workspace_id)?;
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
    /// the worker ran may predate `session --resume`) and open the resume
    /// workspace with the same wrapper, settings and `run_env` as the
    /// worker's.
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
        let landed = landed_since(
            &mut *self.queue,
            &*self.repository,
            &*self.files,
            run,
            &request.main,
        )?;
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
            "--resume".into(),
        ]);
        // The worker's env and group (the same session of the run) and the
        // description `run <run-id> resume` (ADR-0028), then `[run.env]`
        // after the runtime's own names, as in the worker's workspace.
        let workspace = self
            .actors()
            .spawn(ActorExecutionSpec::new(
                ActorContext::worker(run.id(), run.task_id()),
                WorkspaceAccess::Write(PathBuf::from(
                    run.worktree_path().context("missing worktree")?,
                )),
                ActorProgram::RunWorkspace {
                    task: &task,
                    run,
                    wrapper: command,
                    resume: true,
                    description: resume_workspace_description(run),
                    group: self.workspace_group(),
                    run_env,
                },
            ))?
            .workspace()?;
        // Every workspace of the run is recorded, so whatever ends the run
        // finds this one to close; one that cannot be is closed now (task
        // 806).
        if let Err(error) = self.queue.record_runtime_event(
            run.id(),
            event_kind::WORKSPACE_CREATED,
            json!({"workspace_id": workspace, "resume_attempt": attempt}),
        ) {
            return Err(match self.cmux.close(&workspace) {
                Ok(()) => error.context(format!(
                    "the resume workspace {workspace} of run {} could not be recorded and was closed",
                    run.id()
                )),
                Err(close) => error.context(format!(
                    "the resume workspace {workspace} of run {} could not be recorded, and closing it failed: {close:#}",
                    run.id()
                )),
            });
        }
        Ok(ResumeWatch {
            workspace: workspace.clone(),
            attempt,
            run_dir,
            receipt_path: PathBuf::from(run.receipt_path().context("missing receipt path")?),
            idle_marker: run.idle_marker_path()?,
            started_at: self.files.now(),
            startup: Instant::now(),
            message,
            agent_seen: None,
            ready_since: None,
            not_ready_asked: false,
            message_sent: None,
            start: None,
            exit_requested: None,
            exit_typed: false,
            required_evidence: required_of(task.required_evidence(), run.actual_provider()),
            approved: self
                .queue
                .has_run_event(run.id(), event_kind::INTEGRATION_APPROVED)?,
            silent: false,
            exit_for_silence: false,
            stale: None,
            recovery: RecoveryWatch::default(),
            live: Box::new(SessionWatch::fixing(
                run,
                &workspace,
                self.files.now(),
                Stage::Resume,
            )?),
        })
    }
    /// A [`ResumeWatch`] that goes on watching a resumed session another
    /// process started: after a handoff from its `handoff.json`, after an
    /// adoption from the run's events ([`Self::adopt_resume`]). Nothing is
    /// sent or asked twice: the request only when `message_sent_at` is
    /// unknown, never a second `/exit` (its timeout restarts now), and a
    /// dialog recorded during the resume is not recorded again
    /// ([`SessionWatch::adopt`]).
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
            not_ready_asked,
            exit_requested,
            exit_for_silence,
            approved,
        } = state;
        let task = self.queue.show(run.task_id())?.task;
        let now = Instant::now();
        // Answers and dialogs are followed from the request on; an answer
        // typed since closed its ask, which moves the last input on
        // (ADR-0071 decision 17).
        let mut live = Box::new(SessionWatch::fixing(
            run,
            &workspace,
            message_sent_at.unwrap_or(started_at),
            Stage::Resume,
        )?);
        // The dialog and the recovery jobs the previous process recorded
        // during this resume carry over, as for an adopted revise: the
        // anchor is the `resume_started` of the attempt (task 743).
        if let Some(anchor) = self
            .queue
            .run_events(run.id())?
            .iter()
            .rfind(|e| {
                e.kind == event_kind::RESUME_STARTED
                    && e.payload["attempt"].as_u64() == Some(attempt as u64)
            })
            .map(|e| e.id)
        {
            live.adopt(&*self.queue, run, anchor)?;
        }
        Ok(ResumeWatch {
            live,
            stale: adopted_stale_nudge(&*self.queue, run, RESUME_PHASE, Some(attempt))?,
            workspace,
            attempt,
            run_dir: PathBuf::from(run.run_dir().context("missing run directory")?),
            receipt_path: PathBuf::from(run.receipt_path().context("missing receipt path")?),
            idle_marker: run.idle_marker_path()?,
            started_at,
            startup: now,
            message,
            agent_seen: None,
            ready_since: None,
            not_ready_asked,
            // The resume timeout runs from the send, not the takeover.
            message_sent: message_sent_at.map(|at| {
                let ago = self.files.now().duration_since(at).unwrap_or_default();
                (now.checked_sub(ago).unwrap_or(now), at)
            }),
            // Whether the session took the request is not checked again, as
            // for an adopted revise request.
            start: None,
            // Never a second /exit; its timeout restarts now.
            exit_requested: exit_requested.then_some(now),
            // Whether that /exit was typed is not carried over: its
            // "Background work is running" dialog is left to the stuck_exit
            // ask (ADR-0047 decision 29).
            exit_typed: false,
            required_evidence: required_of(task.required_evidence(), run.actual_provider()),
            approved,
            silent: false,
            exit_for_silence,
            // A recovery job the previous process ran is gone: the exit
            // timeout starts another (counted as an attempt).
            recovery: RecoveryWatch::default(),
        })
    }
    /// Rebuild the resume of an adopted `needs_session` run from its
    /// events and run files (task 356, ADR-0047 decision 24): the attempt
    /// of the last `resume_started`, the workspace its `workspace_created`
    /// recorded, the request from `resume-<attempt>.txt` (its mtime stands
    /// for the start: a receipt no newer is from before the resume), and
    /// whether the request was sent (`resume_request_sent`), the inbox asked
    /// about the input box (`input_not_ready`) and `/exit` requested
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
            .find(of_attempt);
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
                not_ready_asked: since.iter().any(|e| e.kind == event_kind::INPUT_NOT_READY),
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
        // workspace stays, and blocks the next attempt until it ends. A
        // session going on to review keeps it until the verdict.
        let closed = !verdict.exit_timed_out
            && !reviewed
            && match self.cmux.close(workspace) {
                Ok(()) => true,
                Err(error) => {
                    warn!(run_id = %slot.run.id(), error = %format_args!("{error:#}"), "run {}: resume workspace {workspace} could not be closed: {error:#}", slot.run.id());
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
                slot.phase = Phase::Validating(
                    Some(handle),
                    Some(SessionRef {
                        workspace: workspace.to_owned(),
                        resume: Some(attempt),
                    }),
                );
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

/// How often the run was resumed, for its used-up reason and ask: the
/// counted resumes against [`MAX_RESUME_ATTEMPTS`], and the conflict-only
/// ones against `config`'s limit (ADR-0047 decision 24) when there were any, with the conflict
/// precheck's requests that shared their limit.
fn resumed_text(resumes: ResumeCount, config: ResumeConfig) -> String {
    let limit = config.conflict_only_limit;
    if resumes.conflict_attempts() == 0 {
        format!(
            "resumed {} times (at most {MAX_RESUME_ATTEMPTS})",
            resumes.counted
        )
    } else if resumes.conflict_requests == 0 {
        format!(
            "resumed {} times ({} of at most {MAX_RESUME_ATTEMPTS} counted, and {} of at most {limit} for conflicts only after its review passed)",
            resumes.total(),
            resumes.counted,
            resumes.conflict_only
        )
    } else {
        format!(
            "resumed {} times ({} of at most {MAX_RESUME_ATTEMPTS} counted) and asked {} times by the conflict precheck ({} of at most {limit} attempts for conflicts only after its review passed)",
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
/// as a failed resume, may have replaced `last_error`), else `last_error`;
/// and what kind of request that makes: `evidence_missing` (or a landing
/// deferred for missing evidence, whose payload names the `checks`),
/// `scope_violation` (or a landing deferred for it, whose payload names the
/// paths), a review sent back, the triage's resume (`triage_finished`,
/// whose `instruction` is the reason, or a person's `triage_decided`), or a
/// landing.
pub(super) fn resume_reason(
    queue: &dyn Queue,
    run: &TaskRun,
) -> Result<(Option<String>, ResumeKind)> {
    let events = queue.run_events(run.id())?;
    let parked = RunHistory::from_events(&events).last_park();
    let reason = parked
        .and_then(|park| park.reason)
        .map(str::to_owned)
        .or_else(|| run.last_error().map(str::to_owned));
    let kind = match parked.map(|park| park.cause) {
        Some(ParkCause::EvidenceMissing) => ResumeKind::EvidenceMissing,
        Some(ParkCause::ScopeViolation) => ResumeKind::ScopeViolation,
        Some(ParkCause::SentBack) => ResumeKind::SentBack,
        Some(ParkCause::Triage) => ResumeKind::Triage,
        Some(ParkCause::Recheck) => ResumeKind::Recheck,
        Some(ParkCause::Landing) | None => ResumeKind::Landing,
    };
    Ok((reason, kind))
}

/// The tasks landed on `main` since the run's base, oldest first, from the
/// `Dagq-Task` trailers, each with its integrated run's receipt summary.
pub(super) fn landed_since(
    queue: &mut dyn Queue,
    repository: &dyn Repository,
    files: &dyn RunFiles,
    run: &TaskRun,
    main: &CommitSha,
) -> Result<Vec<PredecessorSummary>> {
    let mut landed = Vec::new();
    for task_id in repository.landed_task_ids(run.base_commit().as_str(), main.as_str())? {
        let Ok(detail) = queue.show(task_id) else {
            continue;
        };
        let integrated_run = detail
            .runs
            .iter()
            .rev()
            .find(|r| r.status() == RunStatus::Integrated)
            .cloned();
        landed.push(PredecessorSummary::from_predecessor(
            files,
            &Predecessor {
                task: detail.task,
                integrated_run,
            },
        ));
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
    pub(super) agent_seen: Option<Instant>,
    /// Since when every screen read showed the input box ready (task 285).
    pub(super) ready_since: Option<Instant>,
    /// `input_not_ready` is recorded and the inbox asked.
    pub(super) not_ready_asked: bool,
    /// When the resolution request was sent (for its timeout, and for the
    /// idle marker of the response to it).
    pub(super) message_sent: Option<(Instant, SystemTime)>,
    /// Whether the session took the request (task 285).
    pub(super) start: Option<StartCheck>,
    pub(super) exit_requested: Option<Instant>,
    /// The `/exit` of `exit_requested` was typed: past the resume timeout
    /// it is not typed over a dialog, which is then not answered either.
    pub(super) exit_typed: bool,
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
    /// The recovery job of a session that holds the `/exit` back past the
    /// exit timeout (`stuck_exit`, ADR-0047 decision 39).
    pub(super) recovery: RecoveryWatch,
    /// The answers of the session's `worker_question`s and the dialogs it
    /// stops at once the request is sent, followed as a revise's are
    /// (ADR-0071 decision 17); its `input_at` is the last input typed.
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
pub const RESUME_REQUEST_SENT: &str = "resume_request_sent";

/// What a [`ResumeWatch`] taken over from another process starts from.
pub(super) struct ResumeState {
    pub(super) workspace: String,
    pub(super) attempt: usize,
    pub(super) started_at: SystemTime,
    pub(super) message: String,
    pub(super) message_sent_at: Option<SystemTime>,
    pub(super) not_ready_asked: bool,
    pub(super) exit_requested: bool,
    pub(super) exit_for_silence: bool,
    pub(super) approved: bool,
}

/// What a resumed session left behind when it exited.
pub(super) enum ResumeOutcome {
    /// A rewritten `succeeded` receipt names the worktree head.
    Resolved,
    /// A rewritten receipt reports `failed`; the reason for `last_error`.
    Failed(String),
    /// Anything else: no rewritten receipt, or one for another commit.
    Unresolved,
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

impl ResumeWatch {
    /// Record `exit_requested` (before the `/exit` is typed: the session
    /// may exit before the send returns) and type the `/exit` unless
    /// `typed` is false (a dialog is up). The stage ends here: a dialog it
    /// recorded is no attention any more, and its answers are no longer
    /// typed (ADR-0071 decision 17).
    fn request_exit(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun, typed: bool) -> Result<()> {
        sv.queue.record_runtime_event(
            run.id(),
            event_kind::EXIT_REQUESTED,
            json!({
                "workspace_id": self.workspace,
                "timeout_secs": sv.cmux.exit_timeout().as_secs(),
                "resume_attempt": self.attempt,
            }),
        )?;
        self.end_live(sv, run)?;
        if typed {
            submit(sv, run, &self.workspace, Input::Exit, "/exit")?;
            self.exit_typed = true;
        }
        self.exit_requested = Some(Instant::now());
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
            exit_typed: false,
            at_prompt: false,
            lands: false,
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
        self.live.clear_prompt(sv, run)?;
        self.live.recovery.stop(sv, run);
        // A `stalled` ask of a send it did not take ends with the stage,
        // and so does the send's detection.
        self.live.stall.ended(sv, run)?;
        self.live.end_sends(sv, run)
    }

    /// Start the stage's clocks again (ADR-0071 decision 15): the resume
    /// timeout of the request, or before it the wait for a ready input
    /// box, and the timeout of a stale-receipt request not settled yet,
    /// with the idle that answers it. Nothing is carried over.
    pub(super) fn restart_clocks(&mut self, files: &dyn RunFiles) {
        let now = Instant::now();
        match &mut self.message_sent {
            Some((sent, _)) => *sent = now,
            None => {
                if self.agent_seen.is_some() {
                    self.agent_seen = Some(now);
                }
                self.ready_since = None;
            }
        }
        if let Some(nudge) = &mut self.stale
            && !nudge.settled
        {
            nudge.at = files.now();
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

    /// The receipt the session rewrote during this resume, if any.
    pub(super) fn rewritten_receipt(&self, files: &dyn RunFiles) -> Option<Receipt> {
        let modified = files.modified(&self.receipt_path).ok()?;
        if modified <= self.started_at {
            return None;
        }
        Receipt::parse(&files.read_to_string(&self.receipt_path).ok()?).ok()
    }

    /// `head` is the worktree's HEAD when the worktree is clean, `None`
    /// otherwise: a resolved receipt must name a clean head.
    pub(super) fn verdict(
        &self,
        files: &dyn RunFiles,
        run: &TaskRun,
        head: Option<&CommitSha>,
    ) -> ResumeOutcome {
        match self.rewritten_receipt(files) {
            Some(receipt) if receipt.run_id() != run.id().as_str() => ResumeOutcome::Unresolved,
            Some(receipt) if receipt.result() == ReceiptResult::Failed => ResumeOutcome::Failed(
                format!("session reported the run as failed: {}", receipt.summary()),
            ),
            Some(receipt)
                if head.is_some_and(|head| receipt.names_commit(head.as_str()))
                    && receipt.missing_evidence(&self.required_evidence).is_empty() =>
            {
                ResumeOutcome::Resolved
            }
            _ => ResumeOutcome::Unresolved,
        }
    }

    /// Send the resolution request once the agent's input box has shown
    /// ready on every screen read for `resume_prompt_delay` (task 285): a
    /// request typed while Claude Code boots is lost. A box not ready
    /// within the registration timeout of the agent is recorded as
    /// `input_not_ready` and raised to the inbox once; the request still
    /// goes when it gets ready, and past the resume timeout the session is
    /// asked to exit like one that did not finish.
    fn send_when_ready(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        // A headless session takes its request as the prompt of its next
        // turn: nothing to be ready for (ADR-t813-1).
        if headless(run) {
            return self.send_request(sv, run);
        }
        let seen = *self.agent_seen.get_or_insert_with(Instant::now);
        let timed_out = seen.elapsed() >= sv.cmux.resume_timeout();
        let screen = match sv.cmux.capture(&self.workspace) {
            Ok(screen) => screen,
            // Past the resume timeout an unreadable screen still ends it.
            Err(_) if timed_out => String::new(),
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "screen of {} could not be read for its input box: {error:#}", run.id());
                return Ok(());
            }
        };
        if timed_out {
            // The Enter of a /exit typed over a dialog would pick its
            // option: then nothing is typed, and the exit timeout lets the
            // session go with a stuck_exit ask.
            let typed = sv.signals.detect_prompt(&screen).is_none();
            self.request_exit(sv, run, typed)?;
            info!(run_id = %run.id(), "resumed session of {} did not get ready for the resolution request within the resume timeout; exit requested", run.id());
            return Ok(());
        }
        if !sv.signals.input_ready(&screen) {
            self.ready_since = None;
            let timeout = sv.cmux.registration_timeout();
            if !self.not_ready_asked && seen.elapsed() >= timeout {
                self.not_ready_asked = true;
                let excerpt = sv.signals.screen_excerpt(&screen);
                let prompt = sv.signals.detect_prompt(&screen);
                sv.queue.record_runtime_event(
                    run.id(),
                    event_kind::INPUT_NOT_READY,
                    json!({
                        "workspace_id": self.workspace,
                        "waited_secs": timeout.as_secs(),
                        "prompt": prompt,
                        "excerpt": excerpt,
                    }),
                )?;
                warn!(run_id = %run.id(), "resumed session of {} shows no ready input box {}s after its agent registered; asking the inbox", run.id(), timeout.as_secs());
                let situation = match prompt {
                    Some(kind) => format!(
                        "a {kind} dialog holds the resumed session, so the resolution request is not sent"
                    ),
                    None => format!(
                        "the resumed session's input box is not ready {}s after its agent registered, so the resolution request is not sent yet",
                        timeout.as_secs()
                    ),
                };
                ask_unsubmitted(sv, run, &self.workspace, &situation, &excerpt);
            }
            return Ok(());
        }
        let ready = *self.ready_since.get_or_insert_with(Instant::now);
        if ready.elapsed() < sv.cmux.resume_prompt_delay() {
            return Ok(());
        }
        // The ask of a box that was not ready is answered by its getting
        // ready (before the send, which may ask anew).
        if self.not_ready_asked {
            close_answer_prompt_asks(sv, run, INPUT_READY_CLOSED)?;
        }
        self.send_request(sv, run)
    }

    /// Send the resolution request, record it and watch whether the
    /// session took it.
    fn send_request(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        let sent_at = sv.files.now();
        let message = self.message.clone();
        let submission = submit(
            sv,
            run,
            &self.workspace,
            Input::Text(&message),
            "resolution request",
        )?;
        self.message_sent = Some((Instant::now(), sent_at));
        self.live.input_at = Some(sent_at);
        // A supervisor that adopts this resume does not send it again; a
        // record that fails is only noted.
        if let Err(error) = sv.queue.record_runtime_event(
            run.id(),
            RESUME_REQUEST_SENT,
            json!({
                "resume_attempt": self.attempt,
                "workspace_id": self.workspace,
                "sent_at": sent_at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64(),
            }),
        ) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{RESUME_REQUEST_SENT} of {} could not be recorded: {error:#}", run.id());
        }
        self.start = Some(StartCheck::new(
            "resolution request",
            &message,
            sent_at,
            &submission,
        ));
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
                self.startup.elapsed() < timeout,
                "resumed session's wrapper did not register within {} seconds",
                timeout.as_secs()
            );
            return Ok(None);
        };
        let worktree = Path::new(run.worktree_path().context("missing worktree")?);
        if wrapper.exited_at.is_some() {
            // Nobody needs to send anything to a session that exited.
            self.recovery.stop(sv, run);
            self.live.recovery.stop(sv, run);
            self.live.stall.ended(sv, run)?;
            self.live.end_sends(sv, run)?;
            self.live.prompt_hash = None;
            close_answer_prompt_asks(sv, run, PROMPT_EXITED_CLOSED)?;
            if let Some(nudge) = &mut self.stale {
                nudge.settle(sv, run, RESUME_PHASE, Some(self.attempt), "run_ended")?;
            }
            match sv.cmux.capture(&self.workspace) {
                Ok(screen) => sv.files.write(
                    &self
                        .run_dir
                        .join(format!("terminal-resume-{}.txt", self.attempt)),
                    screen.as_bytes(),
                )?,
                Err(error) => sv.queue.record_runtime_event(
                    run.id(),
                    event_kind::SCREEN_CAPTURE_FAILED,
                    reason_of_error(&error, ReasonCode::BackendFailed)
                        .on(json!({"error": format!("{error:#}")})),
                )?,
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
            self.request_exit(sv, run, true)?;
            warn!(run_id = %run.id(), "resumed session of {} lost its wrapper heartbeat; exit requested", run.id());
            self.exit_for_silence = true;
        }
        if let Some(requested) = self.exit_requested {
            let workspace = self.workspace.clone();
            if requested.elapsed() >= sv.cmux.exit_timeout()
                && answer_exit_dialog(sv, run, &workspace, self.exit_typed)?
            {
                // A known dialog answered by rule gets the exit timeout
                // again (ADR-0047 decision 29).
                self.exit_requested = Some(Instant::now());
            } else if requested.elapsed() >= sv.cmux.exit_timeout() {
                // /exit is not resent (it could pick a dialog's option): its
                // recovery job looks at the session first (ADR-0047
                // decision 39).
                let live = Live {
                    workspace: &self.workspace,
                    run_dir: &self.run_dir,
                    allowed: &STUCK_EXIT_HELD_ACTIONS,
                    exit_typed: self.exit_typed,
                    at_prompt: false,
                    lands: false,
                    park: false,
                };
                let (timeout, attempt) = (sv.cmux.exit_timeout().as_secs(), self.attempt);
                let step = self
                    .recovery
                    .follow(sv, run, &live, RecoveryAlert::StuckExit, || {
                        json!({"timeout_secs": timeout, "exit_typed": self.exit_typed, "resume_attempt": attempt})
                    })?;
                let (attempt, escalation) = match step {
                    LiveStep::Pending => return Ok(None),
                    LiveStep::Repaired(applied) => {
                        if applied.exit_again {
                            self.exit_requested = Some(Instant::now());
                        }
                        return Ok(None);
                    }
                    LiveStep::Escalate(attempt, escalation) => (attempt, escalation),
                };
                warn!(run_id = %run.id(), "resumed session of {} did not exit within {}s of the exit request; letting it go as unresolved (its workspace {} is kept)", run.id(), sv.cmux.exit_timeout().as_secs(), self.workspace);
                // Its dialog stays until someone answers it: raise it to
                // the inbox, as for the worker's session (task 104). The
                // next pass closes the ask once the session ended. A failed
                // ask is only noted: the verdict stands without it.
                let after = stuck_exit_after(
                    self.exit_for_silence,
                    if resumes_exhausted(&*sv.queue, run.id(), sv.resume_config) {
                        "The run stays needs_session after its last resume attempt, and goes to its recovery job once the session exits"
                    } else {
                        "The run stays needs_session, and the supervisor resumes it again once the session exits"
                    },
                );
                let note = escalation.note(run, RecoveryAlert::StuckExit, attempt);
                let workspace = self.workspace.clone();
                let alert = RecoveryAlert::StuckExit;
                sv.for_escalation(run, alert, attempt, &escalation, |sv| {
                    match ask_stuck_exit(sv, run, &workspace, &after, Some(&note)) {
                        Ok(id) => {
                            escalation.record(sv, run, alert, attempt, &note, Some(id), json!({}))
                        }
                        Err(error) => {
                            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "stuck_exit ask for {} could not be opened: {error:#}", run.id());
                            Ok(())
                        }
                    }
                })?;
                return Ok(Some(ResumeVerdict {
                    kind: ResumeOutcome::Unresolved,
                    head: sv.repository.head(worktree).ok(),
                    exit_timed_out: true,
                    live: false,
                }));
            }
            return Ok(None);
        }
        let Some((_, sent_at)) = self.message_sent else {
            // A headless session starts no agent before its first request.
            if headless(run) || processes.iter().any(|p| p.role == "agent") {
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
            self.restart_clocks(&*sv.files);
            self.start = self.live.answer_start.take();
        }
        if let Some(typed) = self.live.deliver_answers(sv, run)? {
            self.live.input_at = Some(typed);
            self.restart_clocks(&*sv.files);
            self.start = self.live.answer_start.take();
        }
        if let Some(agent) = processes
            .iter()
            .find(|p| p.role == "agent" && p.exited_at.is_none())
        {
            self.live.watch_prompt(sv, run, agent)?;
        }
        if let Some(start) = &mut self.start {
            start.poll(sv, run, &self.workspace, &self.idle_marker)?;
        }
        // A request or an answer it did not take goes to its recovery job
        // (ADR-0047 decision 31); an instruction the job typed is input.
        if let Some((typed, start)) = self.live.watch_sends(sv, run)? {
            self.live.input_at = Some(typed);
            self.restart_clocks(&*sv.files);
            self.start = Some(start);
        }
        // A session stopped at its own question waits for its answer,
        // however long a person takes: it neither went idle without a
        // resolving receipt nor ran out of time, and is not asked to
        // rewrite a stale receipt (ADR-0071 decision 16).
        if sv.queue.has_unclosed_worker_question(run.id())? {
            return Ok(None);
        }
        self.watch_idle_processes(sv, run)?;
        // An answer delivered by hand (or by the supervisor this one took
        // the run over from) is input too: its close, in a later second
        // than the last input, moves the last input there.
        let input_at = self.live.input_at.unwrap_or(sent_at);
        if let Some(closed) = sv.queue.last_worker_question_closed(run.id())?
            && closed > unix_seconds(input_at)
        {
            self.live.input_at = Some(UNIX_EPOCH + Duration::from_secs(closed.max(0) as u64));
            self.restart_clocks(&*sv.files);
        }
        let input_at = self.live.input_at.unwrap_or(sent_at).max(sent_at);
        let sent = self
            .message_sent
            .map_or_else(Instant::now, |(sent, _)| sent);
        // The idle marker is read before the receipt and the
        // worktree: a receipt rewritten after this read is judged
        // at the next poll, never as idle without it. Without a marker
        // newer than the last input (or the receipt it rewrote), the
        // screen stands in for it (ADR-t803-1).
        let after = [
            Some(input_at),
            self.stale.map(|nudge| nudge.at),
            sv.files.modified(&self.receipt_path).ok(),
        ]
        .into_iter()
        .flatten()
        .fold(UNIX_EPOCH, SystemTime::max);
        let idle = sv.session_idle(run, &self.workspace, &self.idle_marker, after, RESUME_PHASE)?;
        // An input the session took since its marker (read after it: a
        // notice that its background work ended, or a prompt) started a
        // turn that is still running: the stage ends at the idle that ends
        // it, and no /exit is sent before (task 672).
        let input = InputMarker::read(&*sv.files, sv.signals, &self.idle_marker)?;
        let idle = idle.filter(|idle| !idle.turn_open_after(input.as_ref()));
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
            // A request to rewrite a stale receipt gets its own timeout.
            (sent.elapsed() >= sv.cmux.resume_timeout()
                && self
                    .stale
                    .is_none_or(|n| n.settled || n.waited_out(&*sv.files, sv.cmux)))
            .then_some("did not finish within the resume timeout")
        });
        if let Some(why) = why {
            self.settle_stale(sv, run)?;
            // Ask once, the way a person would; never kill the session.
            self.request_exit(sv, run, true)?;
            info!(run_id = %run.id(), "resumed session of {} {why} (head {head}); exit requested", run.id());
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskId;

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
