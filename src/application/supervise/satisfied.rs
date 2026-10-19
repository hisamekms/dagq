//! A run whose worker reported `already_satisfied` (ADR-t1818-1): the
//! runtime lands nothing, verifies the claim on a snapshot of main, asks a
//! person, and applies the answer. The run stays `validating` and rests
//! unleased between the ask and its answer; everything the answer needs
//! (the snapshot, what its verification found) is read from the run's
//! events, so a supervisor started after the ask applies it the same way.

use super::*;
use crate::domain::Ask;
use crate::domain::satisfied::{
    self, CloseStep, SatisfiedAnswer, SnapshotFailure, SnapshotVerification, Verified,
};

/// The verification of a claim on one snapshot, off the loop: the head it
/// runs on, and the answer to close it runs again for.
pub(super) struct SatisfiedWatch {
    pub(super) handle: Option<thread::JoinHandle<Result<SnapshotVerification>>>,
    pub(super) head: CommitSha,
    pub(super) ask: Option<AskId>,
}

impl Supervisor<'_> {
    /// Once the session of a run whose claim validation found sound
    /// exited: the verification starts on main's head, unless an
    /// `approve_close` ask of the run is open already (a run adopted after
    /// its ask), which keeps waiting for its answer, or the container the
    /// verification is set to cannot be used, which nothing runs on the
    /// host instead of (ADR-t2113-1 decision 4): the run then rests
    /// (`None`) and [`Self::start_awaiting_claims`] starts it later.
    pub(super) fn start_satisfied(&mut self, run: &TaskRun) -> Result<Option<Phase>> {
        if self
            .queue
            .has_unclosed_ask(run.id(), AskKind::ApproveClose)?
            || self.verification_container_held()
        {
            return Ok(None);
        }
        self.verify_satisfied(run, None).map(Some)
    }

    /// Start the verification of the claims that rest accepted, before
    /// any verification ([`satisfied::awaits_verification`]), with no
    /// `approve_close` ask open, nobody leasing them, once the verification
    /// may run (a free slot, its runtime usable): those a held container
    /// kept waiting.
    pub(super) fn start_awaiting_claims(&mut self, parallel: usize) -> Result<()> {
        if self.verification_container_held() {
            return Ok(());
        }
        for run in self.queue.runs_with_status(RunStatus::Validating)? {
            if self.used_slots() >= parallel {
                break;
            }
            if self.in_slot(run.id())
                || self.queue.run_lease(run.id())?.is_some()
                || self.queue.turn_fence(run.task_id())?.is_some()
                || !satisfied::awaits_verification(&self.queue.run_events(run.id())?)
                || self
                    .queue
                    .has_unclosed_ask(run.id(), AskKind::ApproveClose)?
            {
                continue;
            }
            let Some(leased) = self
                .queue
                .lease_satisfied(run.id(), &self.registration.token)?
            else {
                continue;
            };
            info!(run_id = %run.id(), "run {}: its already_satisfied claim is verified now that the verification may run", run.id());
            let phase = self.verify_satisfied(&leased, None)?;
            self.claim.slots.admit_noting(
                Slot::new(leased, phase),
                EventKind::RunSatisfiedSnapshot.as_str(),
                &*self.queue,
            );
        }
        Ok(())
    }

    /// Stop what an earlier verification of `run` left working in the
    /// scratch worktree `scratch` (its supervisor died while it ran, and
    /// its commands outlived it in a process group of their own) before
    /// the worktree is checked out again: by pid, SIGTERM and after a grace
    /// SIGKILL, recorded as `auto_repaired` (`repair:
    /// satisfied_verification_stopped`). `Err` says why the worktree may
    /// not be used: the processes could not be listed, or one outlived
    /// its SIGKILL.
    fn stop_stale_verification(
        &self,
        run: &TaskRun,
        scratch: &Path,
    ) -> std::result::Result<(), String> {
        if !self.files.exists(scratch) {
            return Ok(());
        }
        let all = self.processes.list().map_err(|error| {
            format!(
                "the processes of an earlier verification in {} could not be listed: {error:#}",
                scratch.display()
            )
        })?;
        let working: Vec<crate::domain::recovery::ProcessInfo> =
            crate::domain::recovery::run_processes(&all, scratch, None, None, std::process::id())
                .into_iter()
                .cloned()
                .collect();
        if working.is_empty() {
            return Ok(());
        }
        let pids: Vec<u32> = working.iter().map(|p| p.pid).collect();
        let alive = |sv: &Self| -> Vec<u32> {
            pids.iter()
                .copied()
                .filter(|pid| {
                    sv.processes.reap(*pid);
                    sv.processes.alive(*pid)
                })
                .collect()
        };
        for pid in &pids {
            let _ = self.processes.terminate(*pid);
        }
        let started = self.generators.clock.monotonic();
        while !crate::application::waited_out(
            self.generators.clock.since(started),
            super::recovery::STOP_GRACE,
        ) && !alive(self).is_empty()
        {
            thread::sleep(Duration::from_millis(50));
        }
        for pid in alive(self) {
            let _ = self.processes.kill(pid);
        }
        let started = self.generators.clock.monotonic();
        while !crate::application::waited_out(
            self.generators.clock.since(started),
            super::recovery::STOP_GRACE,
        ) && !alive(self).is_empty()
        {
            thread::sleep(Duration::from_millis(50));
        }
        let left = alive(self);
        if !left.is_empty() {
            return Err(format!(
                "processes {left:?} of an earlier verification in {} outlived SIGKILL",
                scratch.display()
            ));
        }
        if let Err(error) = self.queue.record_runtime_event(
            run.id(),
            EventKind::AutoRepaired,
            json!({
                "layer": "runtime",
                "repair": "satisfied_verification_stopped",
                "conditions": {"pids": pids},
                "detail": {"stopped": working},
            }),
        ) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "auto_repaired of {} could not be recorded: {error:#}", run.id());
        }
        info!(run_id = %run.id(), "run {}: stopped processes {pids:?} an earlier verification left in {}", run.id(), scratch.display());
        Ok(())
    }

    /// Stop and remove what an earlier verification of `run` in the
    /// container left (its supervisor died while it ran): its container,
    /// its build output volume and its directory, recorded as
    /// `auto_repaired` (`repair: satisfied_verification_stopped`). `Err`
    /// says why the next may not start.
    fn stop_stale_container(&self, run: &TaskRun) -> std::result::Result<(), String> {
        let removed = self
            .verification_containers
            .remove_left(run.id())
            .map_err(|error| {
                format!("what an earlier verification in the container left could not be removed: {error:#}")
            })?;
        if removed.is_empty() {
            return Ok(());
        }
        if let Err(error) = self.queue.record_runtime_event(
            run.id(),
            EventKind::AutoRepaired,
            json!({
                "layer": "runtime",
                "repair": "satisfied_verification_stopped",
                "conditions": {"verifications": removed},
                "detail": {"runtime": "container"},
            }),
        ) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "auto_repaired of {} could not be recorded: {error:#}", run.id());
        }
        info!(run_id = %run.id(), "run {}: removed the verifications {removed:?} an earlier one left in the container", run.id());
        Ok(())
    }

    /// Take main's head as the snapshot, record it, and run the task's
    /// verification commands on it off the loop (for the answer `ask` when
    /// it runs again for one): on the host or, set so by `dagq.toml`, in
    /// the container, where the commands run without the sccache wrapper,
    /// recorded as `sccache_wrapper_removed` (`job` `satisfied`).
    pub(super) fn verify_satisfied(&mut self, run: &TaskRun, ask: Option<AskId>) -> Result<Phase> {
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        // The task is read here, so the thread takes no connection.
        let task = self.queue.show(run.task_id())?.task;
        let in_container = !task.verification_commands().is_empty()
            && self.verifier.runtimes().is_ok_and(|runtimes| {
                runtimes.verification == crate::domain::container_runtime::Runtime::Container
            });
        // One verification of the run at a time in its scratch worktree,
        // so the files verified are the snapshot's.
        let scratches = self.layout.satisfied_dir.clone();
        let scratch = integration::satisfied_scratch(&scratches, run.id());
        let stale = self.stop_stale_verification(run, &scratch).and_then(|()| {
            if in_container {
                self.stop_stale_container(run)
            } else {
                Ok(())
            }
        });
        let head = self.repository.main_head()?;
        self.queue.record_runtime_event(
            run.id(),
            EventKind::RunSatisfiedSnapshot,
            satisfied::snapshot_payload(&head, ask),
        )?;
        info!(run_id = %run.id(), "run {}: verifying its already_satisfied claim on main {head}", run.id());
        if in_container
            && let Ok(mut env) = self.verifier.run_env(run_dir)
            && let Some(target) = crate::domain::run_container::without_sccache_wrapper(&mut env)
        {
            self.with_env(|env| {
                crate::application::sccache::record_wrapper_removed(
                    &*env.queue,
                    run.id(),
                    env.generators.clock.now(),
                    "supervisor",
                    target.port,
                    "the verification of an already_satisfied claim runs in a container, which has no protected cache to compile with",
                    json!({"job": "satisfied", "runtime": "container"}),
                );
            });
        }
        let repository = self.repository.clone();
        let verifier = self.verifier.clone();
        let files = self.files.clone();
        let containers = self.verification_containers.clone();
        let tokens = self.verification_tokens.clone();
        let (target, snapshot) = (run.clone(), head.clone());
        let handle = spawn_traced(move || {
            if let Err(why) = stale {
                anyhow::bail!("{why}; the verification did not start");
            }
            integration::verify_snapshot(
                &*repository,
                &*verifier,
                &*files,
                &task,
                &target,
                (&snapshot, &scratches),
                integration::SnapshotContainers {
                    containers: &*containers,
                    tokens: &*tokens,
                },
            )
        });
        Ok(Phase::Satisfied(SatisfiedWatch {
            handle: Some(handle),
            head,
            ask,
        }))
    }

    /// The verification of `watch` ended with `outcome` (an error is a
    /// verification that could not run, a failure like any other): it is
    /// recorded with its snapshot. One run again for an answer to close
    /// closes the task when it passed (ADR-t1818-1 decision 3: even if
    /// main moved meanwhile); otherwise the `approve_close` ask opens on
    /// the snapshot (closing the answered one, whose close could not be
    /// applied), and the run rests unleased. Not called while a replanning
    /// guards the task (ADR-t1521-2): the run's slot holds the result for
    /// the park, which withholds it, or for the operation's end.
    pub(super) fn satisfied_verified(
        &mut self,
        run: &TaskRun,
        watch: &SatisfiedWatch,
        outcome: Result<SnapshotVerification>,
    ) -> Result<TaskRun> {
        let verification = outcome.unwrap_or_else(|error| SnapshotVerification {
            head: watch.head.clone(),
            commands: 0,
            failure: Some(SnapshotFailure {
                command: "(the verification could not run)".to_owned(),
                exit_code: -1,
                log_path: String::new(),
                output_tail: format!("{error:#}"),
            }),
        });
        self.queue.record_runtime_event(
            run.id(),
            EventKind::RunSatisfiedVerified,
            verification.payload(watch.ask),
        )?;
        if let (true, Some(ask)) = (verification.passed(), watch.ask) {
            let verified = satisfied::latest_verified(&self.queue.run_events(run.id())?)
                .context("the verification just recorded is not read back")?;
            return self.close_satisfied(
                run,
                Some(&self.registration.token.clone()),
                &verified,
                ask,
            );
        }
        if let Some(answered) = watch.ask {
            self.queue.close_ask(answered)?;
        }
        let task = self.queue.show(run.task_id())?.task;
        let summary = self
            .files
            .read_to_string(Path::new(run.receipt_path().unwrap_or_default()))
            .ok()
            .and_then(|text| Receipt::parse(&text).ok())
            .map(|receipt| receipt.summary().to_owned())
            .unwrap_or_default();
        let options = satisfied::options(&verification);
        let outcome = ask::ask(
            &mut *self.queue,
            NewAsk {
                kind: AskKind::ApproveClose,
                task_id: None,
                run_id: Some(run.id().clone()),
                question: satisfied::question(
                    &task.id().to_string(),
                    &summary,
                    &verification,
                    watch.ask,
                ),
                options: options.iter().map(|o| (*o).to_owned()).collect(),
                asked_by: "supervisor".to_owned(),
                reason_category: AskReason::Scope,
                topics: Vec::new(),
                recommendation: None,
                confidence: None,
                finding_id: None,
                request_id: None,
            },
        )?;
        info!(run_id = %run.id(), "run {}: its already_satisfied claim {} on main {}; it waits for a person in ask {}", run.id(), if verification.passed() { "passed" } else { "failed" }, verification.head, outcome["id"]);
        self.queue
            .release_lease(run.id(), &self.registration.token)?;
        self.queue.run(run.id())
    }

    /// Close the task of `run` on `verified` for the answer `ask`, giving
    /// back `token` when the run holds it, and close the ask.
    fn close_satisfied(
        &mut self,
        run: &TaskRun,
        token: Option<&LeaseToken>,
        verified: &Verified,
        ask: AskId,
    ) -> Result<TaskRun> {
        let (_, closed) = self.queue.close_satisfied(
            run.id(),
            token,
            satisfied::closed_payload(verified, ask),
        )?;
        self.queue.close_ask(ask)?;
        info!(run_id = %run.id(), task_id = %run.task_id(), "task {} is completed without a commit: its already_satisfied claim passed on main {} and ask {ask} closed it", run.task_id(), verified.head);
        Ok(closed)
    }

    /// Apply the answered `approve_close` asks of validating runs nobody
    /// leases, on every pass (ADR-t1818-1): `close` closes the task when
    /// main is still the verified snapshot, else verifies the claim again
    /// on main's head in a free slot ([`satisfied::close_step`]; a `close`
    /// given to the ask of a failed verification, which does not offer
    /// it, likewise only closes once a verification passes); `retry`
    /// fails the run and puts the task back to `ready`; `cancel` fails the
    /// run and cancels the task. Any other answer is left to the inbox; an
    /// error is noted and the ask tried again on a later pass. No answer is
    /// applied while a replanning guards the task.
    pub(super) fn apply_satisfied_answers(&mut self, parallel: usize) -> Result<()> {
        let asks: Vec<Ask> = self
            .queue
            .asks(AskQuery::default())?
            .into_iter()
            .filter(|ask| ask.kind == AskKind::ApproveClose && ask.answer.is_some())
            .collect();
        for ask in asks {
            let Some(run_id) = ask.run_id.clone() else {
                continue;
            };
            let Some(answer) = ask.answer.as_deref().and_then(SatisfiedAnswer::parse) else {
                continue;
            };
            let run = self.queue.run(&run_id)?;
            // A task being replanned or held takes no answer: the
            // replanning's park closes the ask, and the close's transaction
            // refuses it besides; a held one keeps it for the release.
            if run.status() != RunStatus::Validating
                || self.queue.run_lease(&run_id)?.is_some()
                || self.queue.turn_fence(run.task_id())?.is_some()
            {
                continue;
            }
            if let Err(error) = self.apply_satisfied_answer(&run, ask.id, answer, parallel) {
                warn!(run_id = %run.id(), ask_id = %ask.id, error = %format_args!("{error:#}"), "run {}: the answer {} of ask {} could not be applied: {error:#}", run.id(), answer.as_str(), ask.id);
            }
        }
        Ok(())
    }

    fn apply_satisfied_answer(
        &mut self,
        run: &TaskRun,
        ask_id: AskId,
        answer: SatisfiedAnswer,
        parallel: usize,
    ) -> Result<()> {
        // A retry or a cancel decides the run at once; a close is checked
        // against main first ([`satisfied::close_step`]).
        if let Some(decision) = answer.decision(ask_id) {
            let payload = json!({"ask_id": ask_id, "answer": answer.as_str()});
            self.queue.decide_satisfied(
                run.id(),
                &decision.reason,
                Reason::new(decision.code).on(payload),
                &decision.task,
            )?;
            self.queue.close_ask(ask_id)?;
            info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {}: {}", run.id(), run.task_id(), decision.reason);
            self.record_rest(run.id(), None, decision.cause);
            self.request_cleanup(Some(run.task_id()));
            return Ok(());
        }
        let main = self.repository.main_head()?;
        let latest = satisfied::latest_verified(&self.queue.run_events(run.id())?);
        match satisfied::close_step(latest.as_ref(), ask_id, &main) {
            CloseStep::Close(verified) => {
                self.close_satisfied(run, None, &verified, ask_id)?;
                self.record_rest(run.id(), None, EventKind::RunSatisfiedClosed.as_str());
                self.request_cleanup(Some(run.task_id()));
            }
            CloseStep::Verify => {
                // Like a landing, it waits while the container its
                // verification is set to cannot be used; the answer is in,
                // so it waits for no person.
                if self.used_slots() >= parallel || self.verification_container_held() {
                    self.record_rest(run.id(), None, EventKind::AskAnswered.as_str());
                    return Ok(());
                }
                let Some(leased) = self
                    .queue
                    .lease_satisfied(run.id(), &self.registration.token)?
                else {
                    return Ok(());
                };
                info!(run_id = %run.id(), "run {}: main moved from its snapshot before the close answer of ask {ask_id}; verifying it again on {main}", run.id());
                let phase = self.verify_satisfied(&leased, Some(ask_id))?;
                self.claim.slots.admit_noting(
                    Slot::new(leased, phase),
                    EventKind::RunSatisfiedSnapshot.as_str(),
                    &*self.queue,
                );
            }
        }
        Ok(())
    }
}
