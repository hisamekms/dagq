//! Filling the slots: a fill pass adopts and recovers the runs other
//! supervisors left, then starts the candidates of one line by effective
//! priority (ADR-t1850-1): resumes, recovery jobs and the claim loop of
//! each priority. Here are that pass, the claims and their provisioning,
//! and what holds them before a claim starts: the claim hold (disk, load),
//! the landing branch and its recheck, and the `[run.env]` programs; also
//! the attributes a claim records and the load sampled per slot.
//!
//! The methods are part of the one `impl Supervisor<'_>` split across the
//! submodules of [`super`]; they read and change the supervisor's state
//! through the fields their module already sees, so splitting them out
//! widens no field's visibility (ADR-t2391-1 decision 1, C3). The ones
//! the rest of [`super`] calls are `pub(super)`.

use super::*;

/// What a candidate in the line of a fill pass starts (ADR-t1850-1).
enum LineItem {
    Resume(Box<ResumeCandidate>),
    Recovery(Box<TaskRun>),
    /// The claim loop of the candidate's priority (it reads its own order).
    Claim,
}

/// What the claims of one fill pass carry from one priority to the next.
struct ClaimPass {
    /// Since when a claim waits for the claim spacing (ADR-t1479-1).
    spaced_since: Option<i64>,
    host: Option<HostVersions>,
    /// The hash of the plugin a Claude worker loads, read once per pass.
    plugin: Option<String>,
    trial: WorkerTrial,
}

/// How a claim loop of a fill pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClaimStep {
    /// The candidates of its priority ran out; a lower one may claim.
    Next,
    /// No further claim may start in this pass.
    Stop,
}

impl Supervisor<'_> {
    /// Adopt the runs other supervisors left behind, then claim and
    /// provision candidates until every slot is taken or nothing is
    /// claimable. Returns whether claims were held, for the KPI sample
    /// ([`ClaimPassEnd::claims_held`]).
    /// `main` is reread per claim so a task released by
    /// `integrate` starts from the main that contains its predecessor.
    pub(super) fn fill_slots(&mut self, parallel: usize, sweep_interval: Duration) -> Result<bool> {
        // A run that waits for a person is adopted without a free slot
        // (ADR-0062 decision 11): the check is per run.
        self.adopt_stale_runs(parallel)?;
        // Takes no slot: the runs no supervisor holds whose task is being
        // replanned are parked where they wait (ADR-t1521-2).
        if let Err(error) = self.park_idle_runs() {
            warn!(error = %format_args!("{error:#}"), "the runs waiting for a replanning's park could not all be parked: {error:#}");
        }
        // Takes no slot: a held task's run that rests waits as it is
        // (ADR-t1879-1), whoever let it rest.
        if let Err(error) = self.settle_resting_holds() {
            warn!(error = %format_args!("{error:#}"), "the holds of resting runs could not all settle: {error:#}");
        }
        // Takes no slot: a hold past its `until`, or a day on without one,
        // is told to a person once a day (ADR-t1879-1); nothing releases it.
        if let Err(error) = self.queue.remind_task_holds() {
            warn!(error = %format_args!("{error:#}"), "the holds due could not be told: {error:#}");
        }
        // Takes no slot: a dead run goes to the triage below.
        self.recover_dead_runs()?;
        // Whatever the slots: a `land` answer only queues the run, which
        // lands below, before new claims, once there is room (task 949).
        self.apply_landing_answers()?;
        // An answer to an `already_satisfied` claim's ask; a close that
        // verifies the claim again takes a free slot (ADR-t1818-1).
        self.apply_satisfied_answers(parallel)?;
        // A claim a held container kept from its verification starts it
        // once the verification may run.
        self.start_awaiting_claims(parallel)?;
        // Like a landing answer before: an error (an `integrate` that took
        // the integration slot since the check, a main that did not
        // resolve) is noted, and the run stays queued for a later pass.
        if let Err(error) = self.start_approved_landings(parallel, false) {
            warn!(error = %format_args!("{error:#}"), "an approved run could not start its landing: {error:#}");
        }
        // A run recovered from a landing it may not land again (task 1118),
        // or queued by a `retry_review` answer (ADR-t2521-1), is reviewed as
        // one just validated.
        if let Err(error) = self.review_recovered_runs(parallel) {
            warn!(error = %format_args!("{error:#}"), "a run to be reviewed again could not start its review: {error:#}");
        }
        // A run a supervisor that stopped left awaiting integration, its
        // review passed and nobody asked, goes back to its landing's line
        // (ADR-t3049-1).
        if let Err(error) = self.take_up_stranded_landings(parallel) {
            warn!(error = %format_args!("{error:#}"), "a run left awaiting integration could not be taken up: {error:#}");
        }
        // Before the answers and the recovery jobs: a result a hold kept
        // is applied or discarded once the hold is released (ADR-t1879-1).
        self.settle_kept_results()?;
        self.apply_triage_answers()?;
        // A slot the runs freed since the last pass ends the wait for one
        // before anything takes it again (ADR-t1662-1 decision 6).
        self.claim
            .note_slots_full(&*self.queue, self.registration.token(), parallel);
        // Resumes and triage read the landing branch: they wait with the
        // claims until it resolves (ADR-t615-1). A resumed session gets
        // `[run.env]` like a claimed run, so it waits with the claims for a
        // missing program too (task 303).
        let resumes = if self.used_slots() < parallel
            && !self.landing.unresolved
            && !self.landing.run_env_missing
            && !self.observation.ci().held()
        {
            self.resume_candidates()?
        } else {
            Vec::new()
        };
        let recoveries = if self.used_slots() < parallel && !self.landing.unresolved {
            self.triage_candidates()?
        } else {
            Vec::new()
        };
        // The resumes, the recovery jobs and the claims stand in one line
        // by the effective priority of their task (ADR-t1850-1); a held
        // kind only leaves the line, and holds no slot for itself. Read
        // after the candidates: a run out of resumes may have made its task
        // ready again for a claim in this pass.
        let graph = dependency_graph(self.queue.graph_input()?, None);
        let effective: HashMap<TaskId, Priority> = graph
            .tasks
            .iter()
            .map(|node| (node.id, node.effective_priority))
            .collect();
        let mut line = Vec::new();
        for candidate in resumes {
            let priority = self.line_priority(&effective, candidate.run.task_id())?;
            line.push(SlotCandidate {
                kind: SlotKind::Resume,
                priority,
                item: LineItem::Resume(Box::new(candidate)),
            });
        }
        for run in recoveries {
            let priority = self.line_priority(&effective, run.task_id())?;
            line.push(SlotCandidate {
                kind: SlotKind::Recovery,
                priority,
                item: LineItem::Recovery(Box::new(run)),
            });
        }
        // Takes no slot: only closes and frees what ended runs left.
        if let Err(error) = self.sweep_ended_runs(sweep_interval) {
            warn!(error = %format_args!("{error:#}"), "the workspaces and worktrees of ended runs could not all be swept: {error:#}");
        }
        // A wait for the claim spacing goes on only from one claim pass to
        // the next that reaches the spacing again (ADR-t1479-1): a hold or
        // a pass with no free slot or candidate ends it.
        let spaced_since = self.claim.spaced_since.take();
        // A run claimed now would fail every cargo command (ADR-0049
        // decision 9; checked at the top of the pass); the runs in flight
        // and their reviews go on (resumes wait above). A queue service
        // that is down holds the new claims; the runs in flight go on
        // (ADR-t1233-4 decision 2).
        let mut claims = None;
        // Whether this pass read the candidates with a free normal slot.
        let mut free_slot = false;
        // Nor while the CI cannot be read (ADR-t1920-1 decision 2).
        if !(self.landing.run_env_missing
            || self.observation.ci().held()
            || self.landing.unresolved)
            && self.host.service_up()
        {
            // The runs in flight go on; only new claims wait (task 327).
            if self.hold_claims()? {
                // Neither provider can take a worker: the candidates are
                // deferred for it too (ADR-t813-2 decision 6).
                if self.routes().is_empty() {
                    self.claimable(&graph, None)?;
                }
            } else {
                // Read once per pass, so turning the trial on or off takes
                // effect without a restart; a file that cannot be read
                // claims outside it (provisioning reports the file's error).
                let trial = self.verifier.worker_trial().unwrap_or_else(|error| {
                    warn!(error = %format_args!("{error:#}"), "[worker.trial] could not be read; claiming without the trial: {error:#}");
                    WorkerTrial::default()
                });
                claims = Some(ClaimPass {
                    spaced_since,
                    host: None,
                    plugin: None,
                    trial,
                });
                // Only while there is room for a claim, as the claim loop
                // reads the candidates only then.
                let room = self.claim_room(parallel);
                if room != ClaimRoom::None {
                    // A held task is no candidate (the ready query leaves
                    // it out); its record says why (ADR-t1879-1).
                    self.note_held_claims()?;
                    free_slot = room == ClaimRoom::Any;
                    let light = self.light_tasks(room)?;
                    let claimable = self.claimable(&graph, light.as_ref())?;
                    // Each claim at its own effective priority: aging lends
                    // none another's (ADR-t1489-1).
                    let mut own = HashMap::new();
                    for &id in &claimable.order {
                        own.insert(id, self.line_priority(&effective, id)?);
                    }
                    line.extend(
                        crate::domain::slot_order::claims(&claimable.order, &own)
                            .into_iter()
                            .map(|claim| SlotCandidate {
                                kind: claim.kind,
                                priority: claim.priority,
                                item: LineItem::Claim,
                            }),
                    );
                }
            }
        }
        if !free_slot {
            self.claim.defer.no_free_slot();
        }
        let may_claim = claims.is_some();
        // The claims of one priority run together: the claim loop takes
        // every candidate of that priority or higher, in its own order.
        let mut claimed_down_to: Option<Priority> = None;
        for candidate in slot_order(line) {
            match candidate.item {
                // A resume and a recovery job use the normal room only.
                LineItem::Resume(resume) => {
                    if self.used_slots() < parallel {
                        self.resume_run(*resume)?;
                    }
                }
                LineItem::Recovery(run) => {
                    if self.used_slots() < parallel {
                        self.triage_run(*run)?;
                    }
                }
                LineItem::Claim => {
                    let Some(pass) = claims.as_mut() else {
                        continue;
                    };
                    if claimed_down_to.is_some_and(|floor| floor <= candidate.priority) {
                        continue;
                    }
                    claimed_down_to = Some(candidate.priority);
                    if self.claim_tasks(parallel, candidate.priority, pass)? == ClaimStep::Stop {
                        claims = None;
                    }
                }
            }
        }
        // The slots as the pass leaves them.
        self.claim
            .note_slots_full(&*self.queue, self.registration.token(), parallel);
        Ok(ClaimPassEnd {
            may_claim,
            claiming: self.claim.claiming,
            landing_resolved: !self.landing.unresolved,
            route_left: !self.routes().is_empty(),
        }
        .claims_held())
    }
    /// The room for the next claim (ADR-t1591-1).
    fn claim_room(&self, parallel: usize) -> ClaimRoom {
        self.claim.room(parallel)
    }
    /// The candidates the light room takes when `room` is that room (the
    /// light changes that declare their paths, ADR-t1591-1); `None` when
    /// any candidate may be claimed.
    fn light_tasks(&mut self, room: ClaimRoom) -> Result<Option<HashSet<TaskId>>> {
        if room != ClaimRoom::LightOnly {
            return Ok(None);
        }
        Ok(Some(
            self.queue
                .candidates()?
                .into_iter()
                .filter(|task| self.claim.light_changes.admits(task.change(), task.paths()))
                .map(|task| task.id())
                .collect(),
        ))
    }
    /// The effective priority of `task_id` in the line of the fill pass
    /// (ADR-t1850-1): the graph's, else (a task the graph read before it
    /// does not hold) its own.
    fn line_priority(
        &mut self,
        effective: &HashMap<TaskId, Priority>,
        task_id: TaskId,
    ) -> Result<Priority> {
        match effective.get(&task_id) {
            Some(priority) => Ok(*priority),
            None => Ok(self.queue.show(task_id)?.task.priority()),
        }
    }
    /// Claim the ready tasks of effective priority `floor` or higher while
    /// there is room, in the claim order (ADR-0040 decision 4).
    /// [`ClaimStep::Stop`] when no further claim may start in this pass
    /// (no room, the claim spacing, the landing branch, a worker that could
    /// not be provisioned), [`ClaimStep::Next`] when only the candidates of
    /// this priority ran out. When reading main fails and resolving the
    /// landing branch there again does not resolve it, the claim is held and
    /// logged at INFO as the tracing event
    /// `claim_landing_branch_unresolved`: the mark of a hold by this recheck
    /// in the middle of the pass, which `check_landing_branch` at the top of
    /// the pass never logs, whether or not the top's cached stamp skipped
    /// the resolve there.
    fn claim_tasks(
        &mut self,
        parallel: usize,
        floor: Priority,
        pass: &mut ClaimPass,
    ) -> Result<ClaimStep> {
        loop {
            // A heavy task needs a slot with the landing queue in it, as
            // always; the room the landing queue leaves takes only a light
            // task (ADR-t1591-1).
            let room = self.claim_room(parallel);
            if room == ClaimRoom::None {
                return Ok(ClaimStep::Stop);
            }
            // Highest effective priority, then most-releasing, then lowest
            // ID (ADR-0040 decision 4), with the aging across the execution
            // classes within each priority (ADR-t1489-1); `candidates` and
            // `graph` show the same
            // order from the deferrals recorded, so it is not recorded.
            // Less the candidates deferred on a conflict hotspot (ADR-0069)
            // or held by a class limit.
            let graph = dependency_graph(self.queue.graph_input()?, None);
            // In the light room, only the light tasks, whatever the priority
            // of the others; the aging compares only them.
            let light = self.light_tasks(room)?;
            let claimable = self.claimable(&graph, light.as_ref())?;
            // The candidates of a lower priority wait for the resumes and
            // recovery jobs before them in the line (ADR-t1850-1).
            let effective: HashMap<TaskId, Priority> = graph
                .tasks
                .iter()
                .map(|node| (node.id, node.effective_priority))
                .collect();
            let mut order = claimable.order.clone();
            order.retain(|id| effective.get(id).is_some_and(|priority| *priority >= floor));
            if order.is_empty() {
                return Ok(ClaimStep::Next);
            }
            // While the load hold is on, a claim waits for the spacing
            // after the queue's latest claim, a light one too, and the next
            // pass judges the load again before it (ADR-t1479-1). The hold
            // itself returned above, before any claim.
            let spacing = claim_spacing::in_effect(
                self.host.max_load(),
                self.claim.limits.claim_spacing.value,
            );
            let now_ms = crate::application::unix_millis(self.generators.clock.system_time());
            if spacing.is_some() {
                let last = self
                    .queue
                    .latest_event_of(crate::domain::event_kind::RUN_CLAIMED)?
                    .and_then(|event| crate::domain::stats::timestamp_millis(&event.created_at));
                if light_slots::gated(room, false, spacing, last, now_ms) == ClaimRoom::None {
                    if pass.spaced_since.is_none() {
                        info!(
                            event = "claim_spaced",
                            "the next claim waits for the claim spacing of {}s after the latest claim",
                            self.claim.limits.claim_spacing.value
                        );
                    }
                    self.claim.spaced_since = Some(pass.spaced_since.unwrap_or(now_ms));
                    return Ok(ClaimStep::Stop);
                }
            }
            let spacing = spacing.map(|secs| ClaimSpacing {
                claim_spacing: secs,
                claim_spacing_wait_secs: pass
                    .spaced_since
                    .take()
                    .map_or(0, |since| claim_spacing::waited_secs(since, now_ms)),
            });
            // The landing branch was read at the top of the pass; one that
            // stopped resolving since holds the claims as it would have then
            // (ADR-t615-1), and any other error ends the loop as before.
            let base = match self.repository.main_head() {
                Ok(base) => base,
                Err(error) => {
                    self.resolve_landing_branch();
                    if self.landing.unresolved {
                        info!(
                            event = "claim_landing_branch_unresolved",
                            "claim held after the landing branch stopped resolving during the pass"
                        );
                        return Ok(ClaimStep::Stop);
                    }
                    return Err(error);
                }
            };
            // Read once per pass: `rustc -vV` takes a moment on a loaded host.
            let host = pass.host.get_or_insert_with(|| {
                // Codex's version only when this supervisor runs Codex.
                let codex = self
                    .claim
                    .workers
                    .iter()
                    .any(|worker| worker.provider == crate::domain::Provider::Codex)
                    .then_some(self.layout.codex.as_path());
                // The Rust toolchain only where the declared profiles record
                // it (ADR-t2268-1 decision 2); a `dagq.toml` that cannot be
                // read or resolved gives no profile, as for the other
                // readers outside the landing.
                let rustc_in = crate::domain::toolchain::Toolchains::for_measures(Some(
                    self.verifier.toolchains(),
                ))
                .records_rust_toolchain()
                .then_some(self.layout.repo_root.as_path());
                (self.host.host_versions())(&self.layout.claude, codex, rustc_in)
            });
            let host = host.clone();
            let plugin = pass
                .plugin
                .get_or_insert_with(|| (self.worker_plugin)(&self.layout.main_checkout))
                .clone();
            // The line as the claim takes it, with each candidate's
            // priority as the order compared it (ADR-t1662-1 decision 6).
            let by_task =
                crate::domain::claim_facts::candidate_facts(&order, &claimable.aged, |id| {
                    graph
                        .tasks
                        .iter()
                        .find(|node| node.id == id)
                        .map(|node| (node.priority, node.priority_source, node.effective_priority))
                });
            let attributes = ClaimAttributes {
                spacing,
                light_room: (room == ClaimRoom::LightOnly).then_some(true),
                instructions: self.instruction_versions(&base, &plugin),
                candidates: Some(order.len()),
                by_task,
                ..self.claim_attributes(parallel, host)
            };
            let run = match self.queue.claim_for_supervisor_holding(
                &base,
                self.registration.token(),
                &order,
                &claimable.held,
                Some(&serde_json::to_value(&attributes)?),
                &pass.trial,
                &self.routes(),
            )? {
                ClaimOutcome::Claimed { run } => *run,
                ClaimOutcome::NoReadyTask => return Ok(ClaimStep::Next),
            };
            self.claim.defer.claimed();
            if self.provision_claimed(&run, attributes.load_avg)? == ClaimStep::Stop {
                return Ok(ClaimStep::Stop);
            }
        }
    }
    /// Provision the run this supervisor just claimed and give it a slot,
    /// the work interval starting with the `load` sampled at the claim. A
    /// provisioning that failed stops the claims ([`ClaimStep::Stop`]),
    /// but not one cut off by the queue service out of reach: that is the
    /// pass's error, and the run, still under this supervisor's lease, is
    /// found again once the service answers ([`Self::reconcile_unanswered`]).
    pub(super) fn provision_claimed(
        &mut self,
        run: &TaskRun,
        load: Option<f64>,
    ) -> Result<ClaimStep> {
        stages::record_claimed(&*self.queue, run.id());
        // The work interval starts at the claim, with its sample.
        self.claim.start_load(run.id(), load);
        match self.provision(run) {
            Ok(watch) => {
                let run = self.queue.run(run.id())?;
                self.claim.slots.admit_noting(
                    Slot::new(run, Phase::Session(watch)),
                    EventKind::AgentStarted.as_str(),
                    &*self.queue,
                );
                Ok(ClaimStep::Next)
            }
            Err(error) if crate::application::supervisor_service::unreachable(&error) => Err(error),
            // One child's snapshot is that run's failure, not the host's:
            // the run is abandoned with why and the claims go on.
            Err(error)
                if error
                    .chain()
                    .any(|cause| cause.is::<session::ReplanCarryFailed>()) =>
            {
                let message = format!("run {} provisioning failed: {error:#}", run.id());
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{message}");
                self.abandon(run, message, &reason_of_error(&error, ReasonCode::Other));
                Ok(ClaimStep::Next)
            }
            Err(error) => {
                let message = format!("run {} provisioning failed: {error:#}", run.id());
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{message}; no further tasks will be claimed");
                self.abandon(
                    run,
                    message.clone(),
                    &reason_of_error(&error, ReasonCode::Other),
                );
                self.claim.claiming = false;
                self.claim.provisioning_error = Some(message);
                Ok(ClaimStep::Stop)
            }
        }
    }
    /// Record `candidates_sampled` (ADR-0051 decision 3) when this pass's
    /// claimable ready tasks (by the rule's order, the deferred ones
    /// included: [`CandidatesSample::candidates`]), free slots or ready
    /// tasks or whether claims are held differ from the sample this process
    /// recorded last, a draining or handing-off pass included, and on its
    /// first pass ([`ObservationState::record_candidates`]).
    pub(super) fn sample_candidates(&mut self, parallel: usize, claims_held: bool) {
        let sample = self.queue.graph_input().map(|input| {
            let graph = dependency_graph(input, None);
            CandidatesSample {
                claims_held,
                candidates: graph.candidates.len(),
                free_slots: parallel.saturating_sub(self.used_slots()),
                ready: graph
                    .tasks
                    .iter()
                    .filter(|node| node.status == TaskStatus::Ready)
                    .count(),
            }
        });
        self.on_observation(|observation, env| observation.record_candidates(env, sample));
    }
    /// Judge whether new claims are held now ([`ClaimHold::judge`]: the
    /// free disk space this pass read against what a claim needs (task
    /// 377), and the load) and record `claim_held` or `claim_resumed` when
    /// the answer differs from the hold in place on the queue (task 327).
    /// Returns whether they are held.
    pub(super) fn hold_claims(&mut self) -> Result<bool> {
        let needed = self.on_host(|host, env| host.disk_needs(env))?.claim;
        // Short while a cleanup for room runs: wait for it without a hold
        // (task 405).
        if disk::claims_wait_for_cleanup(self.host.disk_cleaning(), self.host.free(), needed) {
            return Ok(true);
        }
        let hold = ClaimHold::judge(&HoldInputs {
            load_average: (self.host.load_average())(),
            max_load: self.host.max_load(),
            free_bytes: self.host.free(),
            needed_bytes: needed,
            // Claude's hold ask holds the claims only while no worker can
            // run on the other provider either (ADR-t813-2 decision 6).
            queue_hold: self.queue_hold.filter(|_| self.routes().is_empty()),
        });
        self.with_env(|env| env.record_hold(claim_hold::CLAIMS, hold.as_ref()))
    }
    /// Check the programs `[run.env]` names on this process's PATH
    /// (ADR-0049 decision 9) and record `run_env_program_missing` or
    /// `run_env_program_found` when the answer differs from the latest one
    /// on the queue, so a restarted supervisor does not repeat it. A
    /// `dagq.toml` that cannot be read leaves the last answer: provisioning
    /// and `integrate` report that file themselves.
    pub(super) fn check_run_env_programs(&mut self) -> Result<()> {
        let check = match self.verifier.run_env_programs(None) {
            Ok(check) => check,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the programs of [run.env] could not be checked: {error:#}");
                return Ok(());
            }
        };
        let last = self.queue.latest_queue_event(&RUN_ENV_PROGRAM_KINDS)?;
        if let Some((kind, mut payload)) = check.transition(last.as_ref().map(|e| &e.kind)) {
            payload["supervisor"] = json!(self.registration.token());
            self.queue.record_queue_event(kind, payload)?;
            match check.missing_message() {
                Some(message) => {
                    warn!("{message}; no task is claimed and no run lands until it is found")
                }
                None => {
                    info!("the programs of [run.env] are found again; claiming and landing resume")
                }
            }
        }
        self.landing.run_env_missing = !check.missing().is_empty();
        Ok(())
    }
    /// Resolve the landing branch (ADR-t615-1) when it did not resolve at
    /// the last resolution, its inputs changed since then, or `recheck`
    /// passed,
    /// and hold claims and landings while it does not resolve (task 1078).
    /// The stamp is taken before resolving, so a change during the
    /// resolution differs from it at the next pass.
    pub(super) fn check_landing_branch(&mut self, recheck: Duration) {
        let stamp = self.repository.landing_branch_stamp();
        // Only a resolution that succeeded is kept: one that failed, maybe
        // for a moment (a Git that did not start), is tried again at the
        // next pass, so the claims resume there once it resolves.
        if let (Some(stamp), Some((last, at))) = (&stamp, &self.landing.stamp)
            && keeps_landing_resolution(
                self.landing.unresolved,
                stamp == last,
                self.generators.clock.since(*at),
                recheck,
            )
        {
            // A resolution kept after a record of its end that failed.
            if self.landing.recorded != Some(None) {
                self.record_landing_hold(None);
            }
            return;
        }
        self.landing.stamp = stamp.map(|stamp| (stamp, self.generators.clock.monotonic()));
        self.resolve_landing_branch();
    }
    /// Resolve the landing branch now and hold claims and landings while it
    /// does not resolve, warning when that changes, and record
    /// `landing_branch_unresolved` / `landing_branch_resolved` where the
    /// hold differs from this supervisor's latest record, so `status` and
    /// `candidates` read it, and a pass that resolves it again records
    /// nothing.
    fn resolve_landing_branch(&mut self) {
        let error = match self.repository.landing_branch() {
            Ok(branch) => {
                if self.landing.unresolved {
                    info!(branch = %branch.name, "the landing branch resolves again to {}; claiming and landing resume", branch.name);
                }
                self.landing.unresolved = false;
                None
            }
            Err(error) => {
                if !self.landing.unresolved {
                    warn!(error = %format_args!("{error:#}"), "{error:#}; no task is claimed and no run lands until it resolves");
                }
                self.landing.unresolved = true;
                Some(format!("{error:#}"))
            }
        };
        self.record_landing_hold(error);
    }
    /// Record the landing branch's hold (`error` while it does not
    /// resolve) where it differs from the one this process last recorded.
    /// What becomes of a run awaiting the landing slot now
    /// ([`crate::domain::landing_hold::judge`]): the holds are this
    /// supervisor's, not the run's.
    pub(super) fn landing_hold(&self) -> crate::domain::landing_hold::LandingHold {
        crate::domain::landing_hold::judge(crate::domain::landing_hold::LandingHoldInputs {
            run_env_missing: self.landing.run_env_missing,
            container_held: self.verification_container_held(),
            ci_held: self.observation.ci().held(),
            ci_unreadable: self.observation.ci().unreadable(),
            landing_unresolved: self.landing.unresolved,
            landing_short: self.host.landing_short(),
            disk_cleaning: self.host.disk_cleaning(),
            draining: self.registration.draining(),
            handing_off: self.registration.handing_off(),
        })
    }

    fn record_landing_hold(&mut self, error: Option<String>) {
        use crate::domain::landing_branch::{LANDING_BRANCH_HOLD, UNRESOLVED_REASON};
        let reason = error.is_some().then_some(UNRESOLVED_REASON);
        if self.landing.recorded == Some(reason) {
            return;
        }
        let hold = error.map(|error| (UNRESOLVED_REASON, json!({"error": error})));
        self.landing.recorded = self
            .with_env(|env| env.record_own_hold(LANDING_BRANCH_HOLD, hold))
            .then_some(reason);
    }
    /// Record `run_env_changed` when the normalized `[run.env]` of the main
    /// checkout hashes differently from the latest one on the queue
    /// (ADR-0051 decision 11), so a restarted supervisor does not repeat
    /// it. A `dagq.toml` that cannot be read records nothing: provisioning
    /// and `integrate` report that file themselves. Neither does a missing
    /// one, which may only be a checkout rewriting it; an empty `[run.env]`
    /// is the change that removes the table.
    pub(super) fn mark_run_env_change(&mut self) -> Result<()> {
        let table = match self.verifier.run_env_table() {
            Ok(Some(table)) => table,
            Ok(None) => return Ok(()),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "[run.env] could not be read for its change mark: {error:#}");
                return Ok(());
            }
        };
        let salt = match self.verifier.run_env_salt() {
            Ok(salt) => salt,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the salt of the [run.env] hashes could not be read: {error:#}");
                return Ok(());
            }
        };
        let last = self.queue.latest_queue_event(&[RUN_ENV_CHANGED])?;
        if let Some(mut payload) =
            run_env_digest(&table, &salt).transition(last.as_ref().map(|event| &event.payload))
        {
            info!(
                "[run.env] changed ({}): recorded as a change mark",
                payload["changed"]
            );
            payload["supervisor"] = json!(self.registration.token());
            self.queue
                .record_queue_event(EventKind::RunEnvChanged, payload)?;
        }
        Ok(())
    }
    /// What `run_claimed` records of a claim made now (task 197): this
    /// binary's build identifier, the host's versions, `parallel`, the
    /// slots held before the claim (runs waiting for a person hold none,
    /// ADR-0062) and the load average.
    fn claim_attributes(&self, parallel: usize, host: HostVersions) -> ClaimAttributes {
        ClaimAttributes {
            dagq_version: self.layout.version.clone(),
            host,
            parallel,
            slots: self.used_slots(),
            load_avg: (self.host.load_average())(),
            spacing: None,
            light_room: None,
            instructions: BTreeMap::new(),
            candidates: None,
            by_task: BTreeMap::new(),
        }
    }
    /// The versions of the instructions a worker on each of this
    /// supervisor's providers reads in a run from `base` (goal 113), with
    /// `plugin` the hash of a Claude worker's plugin
    /// ([`instructions::by_provider`]).
    fn instruction_versions(
        &self,
        base: &CommitSha,
        plugin: &str,
    ) -> BTreeMap<String, InstructionVersions> {
        instructions::by_provider(
            self.claim.workers.iter().map(|worker| worker.provider),
            |provider| crate::application::prompt::worker_template(provider).ok(),
            plugin,
            self.repository
                .blobs_in(base.as_str(), &instructions::REPOSITORY_INSTRUCTIONS)
                .ok(),
        )
    }
    /// Sample the load average once for every slot's current interval
    /// (task 197); the windows of runs no slot holds any more are dropped.
    pub(super) fn sample_load(&mut self) {
        let load = (self.host.load_average())();
        self.claim.sample_load(load);
    }
}

/// Whether the last resolution of the landing branch is kept: it
/// resolved, its inputs (the stamp) did not change, and `recheck` has not
/// passed `since` it.
fn keeps_landing_resolution(
    unresolved: bool,
    same_stamp: bool,
    since: Duration,
    recheck: Duration,
) -> bool {
    !unresolved && same_stamp && since < recheck
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolution of the landing branch is kept until 1 ms before its
    /// recheck and resolved again at it, or at once when it did not resolve
    /// or its inputs changed.
    #[test]
    fn the_landing_branch_is_resolved_again_at_its_recheck() {
        let recheck = Duration::from_secs(300);
        let ms = Duration::from_millis(1);
        assert!(keeps_landing_resolution(false, true, recheck - ms, recheck));
        assert!(!keeps_landing_resolution(false, true, recheck, recheck));
        assert!(!keeps_landing_resolution(
            true,
            true,
            Duration::ZERO,
            recheck
        ));
        assert!(!keeps_landing_resolution(
            false,
            false,
            Duration::ZERO,
            recheck
        ));
    }
}
