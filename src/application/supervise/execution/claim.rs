//! Filling the slots: a fill pass adopts and recovers the runs other
//! supervisors left, then starts the candidates of one line by effective
//! priority (ADR-t5292-1): returns, resumes, recovery jobs and the claim loop of
//! each priority. The queued landing shares the disk decision with claims.
//! Here are that pass, the claims and their provisioning,
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
use crate::domain::admission::{self, Blocked, Candidate, Gates, Kind, Resources};
use crate::domain::slot_order::SlotKind;

/// What a candidate in the line of a fill pass starts (ADR-t5292-1).
enum LineItem {
    Return(usize),
    Landing(Box<super::landing::QueuedLanding>),
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
    /// The pass's gates and its remaining disk budget for new claims.
    resources: Resources,
    end: ClaimPassEnd,
    started: bool,
}

impl ClaimPass {
    fn no_candidate(&self, any_candidates: bool) -> ClaimPassEnd {
        if self.started {
            ClaimPassEnd::default()
        } else {
            ClaimPassEnd::blocked(if any_candidates {
                Blocked::CandidatesDeferred
            } else {
                Blocked::NoCandidates
            })
        }
    }
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
    /// claimable. Records the decision for status and KPI.
    /// `main` is reread per claim so a task released by
    /// `integrate` starts from the main that contains its predecessor.
    /// Whether the source update was polled before landing admission.
    pub(super) fn fill_slots(&mut self, parallel: usize, options: &LoopSettings) -> Result<bool> {
        const CALLER: &str = "claim.fill_slots";
        let _caller = crate::application::supervisor_service::caller::scope(CALLER);
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
        // Whatever the slots: a `land` answer queues the run for the same
        // start decision as new claims (task 949, ADR-t5292-1).
        self.apply_landing_answers()?;
        // An answer to an `already_satisfied` claim's ask; a close that
        // verifies the claim again takes a free slot (ADR-t1818-1).
        self.apply_satisfied_answers(parallel)?;
        // A claim a held container kept from its verification starts it
        // once the verification may run.
        self.start_awaiting_claims(parallel)?;
        let (landing, update_polled) = match self.queued_landing(false, false, Some(options)) {
            Ok(admission) => admission,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the landing queue could not be read: {error:#}");
                // Without a readiness snapshot, keep yielding until
                // the wait limit instead of treating the queue as empty.
                self.on_host(|host, env| host.auto_update_pass(env, options, true));
                (None, true)
            }
        };
        // A run recovered from a landing it may not land again (task 1118),
        // or queued by a `retry_review` answer (ADR-t2521-1), is reviewed as
        // one just validated.
        if let Err(error) = self.review_recovered_runs(parallel) {
            warn!(error = %format_args!("{error:#}"), "a run to be reviewed again could not start its review: {error:#}");
        }
        // A run a supervisor that stopped left awaiting integration, its
        // review passed and nobody asked, goes back to its landing's line
        // (ADR-t3049-1).
        if let Err(error) = self.take_up_stranded_landings(
            parallel,
            landing.as_ref().map(|landing| &landing.entry.run_id),
        ) {
            warn!(error = %format_args!("{error:#}"), "a run left awaiting integration could not be taken up: {error:#}");
        }
        // Before the answers and the recovery jobs: a result a hold kept
        // is applied or discarded once the hold is released (ADR-t1879-1).
        self.settle_kept_results()?;
        self.apply_triage_answers()?;
        // A slot the runs freed since the last pass ends the wait for one
        // before anything takes it again (ADR-t1662-1 decision 6).
        self.note_admission_slots_full(parallel);
        // A session that ended on a full disk is resumed once there is
        // room, before the resumes below take it into a free slot (goal
        // 260).
        if self.used_slots() < parallel
            && let Err(error) = self.resume_full_disks()
        {
            warn!(error = %format_args!("{error:#}"), "the runs that ended on a full disk could not all be resumed: {error:#}");
        }
        // Resumes and triage read the landing branch: they wait with the
        // claims until it resolves (ADR-t615-1). A resumed session gets
        // `[run.env]` like a claimed run, so it waits with the claims for a
        // missing program too (task 303).
        let gates = self.admission_gates();
        let gates_only = Resources {
            free_slots: 1,
            gates,
            ..Resources::default()
        };
        let resumes = if admission::probe(SlotKind::Resume, gates_only).is_none() {
            self.resume_candidates(self.used_slots() < parallel)?
        } else {
            for candidate in self.queue.runs_needing_session()? {
                self.end_resume_slot_wait(&candidate.run, "resume_gate")?;
            }
            Vec::new()
        };
        let recoveries = if admission::probe(
            SlotKind::Recovery,
            Resources {
                free_slots: parallel.saturating_sub(self.used_slots()),
                ..gates_only
            },
        )
        .is_none()
        {
            self.triage_candidates()?
        } else {
            Vec::new()
        };
        // Returns, resumes, recovery jobs and claims stand in one line
        // by the effective priority of their task (ADR-t5292-1); a held
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
        // A batch uses one landing turn. Budget claims against its most
        // demanding member so the reservation also covers later stages.
        let landing_disk = landing.as_ref().and_then(|landing| {
            landing
                .run_ids()
                .filter_map(|run| self.host.landing_claims(run))
                .min_by_key(|disk| disk.claims.unwrap_or(usize::MAX))
        });
        let landings_leaving_queue = landing
            .as_ref()
            .filter(|landing| !landing.entry.e2e)
            .map_or(0, |landing| landing.run_ids().count());
        if let Some(landing) = landing {
            line.push(Candidate {
                kind: Kind::Landing,
                priority: landing.entry.priority,
                progress: Default::default(),
                item: LineItem::Landing(Box::new(landing)),
            });
        }
        for candidate in resumes {
            let priority = self.line_priority(&effective, candidate.run.task_id())?;
            line.push(Candidate::slot(
                SlotKind::Resume,
                priority,
                LineItem::Resume(Box::new(candidate)),
            ));
        }
        for run in recoveries {
            let priority = self.line_priority(&effective, run.task_id())?;
            line.push(Candidate::slot(
                SlotKind::Recovery,
                priority,
                LineItem::Recovery(Box::new(run)),
            ));
        }
        // Takes no slot: only closes and frees what ended runs left.
        if let Err(error) = self.sweep_ended_runs(options.sweep_interval) {
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
        let claim_gates = Resources {
            repository_config_missing: self.claim.repository_config_missing,
            service_down: !self.host.service_up(),
            ..gates_only
        };
        let mut end = admission::probe(SlotKind::Claim, claim_gates)
            .map_or_else(ClaimPassEnd::default, ClaimPassEnd::blocked);
        let mut hold_details = None;
        if end.reason.is_none() {
            // Read once per pass, so turning the trial on or off takes
            // effect without a restart; a file that cannot be read
            // claims outside it (provisioning reports the file's error).
            // The deferrals judge a candidate by the worker the trial of the
            // provider pair requests for it, as the claim does
            // (ADR-t4005-1).
            let trial = self.verifier.worker_trial().unwrap_or_else(|error| {
                warn!(error = %format_args!("{error:#}"), "[worker.trial] could not be read; claiming without the trial: {error:#}");
                WorkerTrial::default()
            });
            // The runs in flight go on; only new claims wait (task 327).
            let (resources, details) = self.read_claim_resources(Resources {
                landing_disk,
                ..claim_gates
            });
            hold_details = details;
            let reason = admission::probe(SlotKind::Claim, resources)
                .filter(|_| admission::probe(SlotKind::Claim, resources.after_landing()).is_some());
            if let Some(reason) = reason {
                end = ClaimPassEnd::blocked(reason);
                // Neither provider can take a worker: the candidates are
                // deferred for it too (ADR-t813-2 decision 6).
                if self.routes().is_empty() {
                    self.claimable(&graph, None, &trial.providers)?;
                }
            } else {
                // Only while there is room for a claim, as the claim loop
                // reads the candidates only then.
                let capacity = self.admission_capacity(parallel);
                let reason = admission::probe(SlotKind::Claim, capacity).filter(|reason| {
                    !(*reason == Blocked::WorkerSlot
                        && landings_leaving_queue > 0
                        && self
                            .claim
                            .slots
                            .projected_landing_room(parallel, landings_leaving_queue))
                });
                let mut pass = ClaimPass {
                    spaced_since,
                    host: None,
                    plugin: None,
                    trial,
                    resources,
                    end: ClaimPassEnd::default(),
                    started: false,
                };
                if let Some(reason) = reason {
                    pass.end = ClaimPassEnd::blocked(reason);
                } else {
                    // A held task is no candidate (the ready query leaves
                    // it out); its record says why (ADR-t1879-1).
                    self.note_held_claims()?;
                    free_slot = true;
                    let claimable = self.claimable(&graph, None, &pass.trial.providers)?;
                    pass.end = pass.no_candidate(!graph.candidates.is_empty());
                    // One transaction loop per effective-priority band.
                    // The ready order, including aging, is reread in it.
                    let mut priorities = HashSet::new();
                    for id in claimable.order {
                        let priority = self.line_priority(&effective, id)?;
                        if priorities.insert(priority) {
                            line.push(Candidate::slot(SlotKind::Claim, priority, LineItem::Claim));
                        }
                    }
                }
                claims = Some(pass);
            }
        }
        if !free_slot {
            self.claim.defer.no_free_slot();
        }
        // The claims of one priority run together: the claim loop takes
        // every candidate of that priority or higher, in its own order.
        let mut preceding_priority = None;
        loop {
            // Read returns again after each start. A resume may change the
            // slot table, and a hold or its release may have arrived.
            line.extend(
                self.return_candidates(&effective)?
                    .into_iter()
                    .map(|candidate| Candidate {
                        item: LineItem::Return(candidate.item),
                        kind: candidate.kind,
                        priority: candidate.priority,
                        progress: candidate.progress,
                    }),
            );
            if line.is_empty() {
                break;
            }
            let has_landing = line
                .iter()
                .any(|candidate| matches!(candidate.item, LineItem::Landing(_)));
            let mut resources = Resources {
                free_slots: self.admission_capacity(parallel).free_slots,
                gates: self.admission_gates(),
                ..claims.as_ref().map_or(gates_only, |pass| pass.resources)
            };
            if has_landing && claims.is_some() {
                self.read_claim_spacing(&mut resources)?;
            }
            let mut decision = admission::admit(line, resources);
            self.claim.admission_pass.observe(&decision);
            let landing_waits_for_claim = decision.blocked.iter().any(|(candidate, reason)| {
                matches!(candidate.item, LineItem::Landing(_))
                    && *reason == Blocked::DiskCompetition
            });
            let mut starts = decision.starts.into_iter();
            let Some(candidate) = starts.next() else {
                for (candidate, reason) in decision.blocked {
                    if matches!(candidate.item, LineItem::Landing(_)) {
                        continue;
                    }
                    if matches!(candidate.item, LineItem::Claim)
                        && let Some(pass) = claims.as_mut()
                        && !pass.started
                    {
                        pass.end = ClaimPassEnd::blocked(reason);
                        if reason == Blocked::ClaimSpacing {
                            pass.end.spacing_secs = resources.spacing_secs;
                        }
                    }
                    if let LineItem::Resume(resume) = candidate.item
                        && reason == Blocked::WorkerSlot
                    {
                        self.note_resume_slot_wait(
                            &resume,
                            true,
                            parallel,
                            candidate.priority,
                            preceding_priority,
                        )?;
                    }
                    preceding_priority = Some(candidate.priority);
                }
                break;
            };
            line = starts
                .chain(decision.blocked.drain(..).map(|(candidate, _)| candidate))
                .filter(|candidate| !matches!(candidate.item, LineItem::Return(_)))
                .collect();
            match candidate.item {
                LineItem::Return(index) => self.regain_waiting_run(index),
                LineItem::Landing(landing) => {
                    let run = landing.entry.run_id.clone();
                    let started = match self.start_queued_landing(*landing) {
                        Ok(started) => started,
                        Err(error) => {
                            warn!(error = %format_args!("{error:#}"), "an approved run could not start its landing: {error:#}");
                            self.queue.run(&run)?.status() == RunStatus::Integrating
                        }
                    };
                    self.note_landing_admission(started);
                    if let Some(pass) = claims.as_mut() {
                        if started {
                            pass.resources = pass.resources.after_landing();
                        }
                        pass.resources.landing_disk = None;
                    }
                    continue;
                }
                LineItem::Resume(resume) => {
                    self.note_resume_slot_wait(
                        &resume,
                        false,
                        parallel,
                        candidate.priority,
                        preceding_priority,
                    )?;
                    let before = self.claim.slots.len();
                    self.resume_run(*resume)?;
                    if self.claim.slots.len() > before {
                        self.claim.admission_pass.started();
                    } else {
                        self.claim.admission_pass.declined("ResumeStartDeclined");
                    }
                }
                LineItem::Recovery(run) => {
                    let before = self.claim.slots.len();
                    self.triage_run(*run)?;
                    if self.claim.slots.len() > before {
                        self.claim.admission_pass.started();
                    } else {
                        self.claim.admission_pass.declined("RecoveryStartDeclined");
                    }
                }
                LineItem::Claim => {
                    let pass = claims.as_mut().expect("admitted claim has a pass");
                    let budget = pass.resources.disk_claims;
                    if landing_waits_for_claim {
                        pass.resources.disk_claims = Some(1);
                    }
                    let step = self.claim_tasks(
                        parallel,
                        candidate.priority,
                        landing_waits_for_claim,
                        pass,
                    )?;
                    if landing_waits_for_claim {
                        if pass.resources.disk_claims == Some(0) {
                            pass.resources.landing_disk = None;
                            if let Some(index) = line.iter().position(|candidate| {
                                matches!(candidate.item, LineItem::Landing(_))
                            }) && let LineItem::Landing(landing) = line.remove(index).item
                            {
                                self.record_landing_queue_wait(
                                    &landing.entry,
                                    Some(crate::domain::landing_queue::Wait::Disk),
                                )?;
                            }
                        } else {
                            pass.resources.disk_claims = budget;
                        }
                    }
                    if step == ClaimStep::Stop {
                        line.retain(|candidate| !matches!(candidate.item, LineItem::Claim));
                    }
                }
            }
            preceding_priority = Some(candidate.priority);
        }
        // The slots as the pass leaves them.
        self.note_admission_slots_full(parallel);
        if let Some(pass) = claims {
            end = pass.end;
            if !self.claim.claiming {
                end = ClaimPassEnd::blocked(Blocked::ProvisioningFailed);
            } else if self.routes().is_empty() && !end.claims_held() {
                end = ClaimPassEnd::blocked(Blocked::NoProvider);
            }
        }
        // A cleanup already reclaiming space may relieve this wait before
        // it becomes a hold interval. Its reason is still in the sample.
        let hold = if end.reason == Some(Blocked::Disk(None))
            && self.host.disk_cleaning()
            && hold_details
                .as_ref()
                .is_some_and(|hold| hold.reason == claim_hold::HoldReason::DiskSpace)
        {
            None
        } else {
            self.with_env(|env| env.claim_hold_transition(end, hold_details.as_ref()))?
        };
        self.sample_candidates(parallel, end, hold);
        Ok(update_polled)
    }

    pub(super) fn admission_gates(&self) -> Gates {
        Gates {
            ci: self.observation.ci().held(),
            env: self.landing.run_env_missing,
            branch: self.landing.unresolved,
        }
    }

    fn admission_capacity(&self, parallel: usize) -> Resources {
        Resources {
            free_slots: parallel.saturating_sub(self.used_slots()),
            ..Resources::default()
        }
    }

    fn note_admission_slots_full(&mut self, parallel: usize) {
        let room = admission::claim_room(self.admission_capacity(parallel));
        self.claim
            .note_slots_full(&*self.queue, self.registration.token(), parallel, room);
    }

    /// The effective priority of `task_id` in the line of the fill pass
    /// (ADR-t1850-1): the graph's, else (a task the graph read before it
    /// does not hold) its own.
    pub(super) fn line_priority(
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
        landing_waits_for_claim: bool,
        pass: &mut ClaimPass,
    ) -> Result<ClaimStep> {
        loop {
            let resources = Resources {
                free_slots: self.admission_capacity(parallel).free_slots,
                gates: self.admission_gates(),
                ..pass.resources
            };
            if let Some(reason) = admission::probe(SlotKind::Claim, resources) {
                if !pass.started {
                    pass.end = ClaimPassEnd::blocked(reason);
                }
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
            let mut claimable = self.claimable(&graph, None, &pass.trial.providers)?;
            // The candidates of a lower priority wait for the resumes and
            // recovery jobs before them in the line (ADR-t1850-1).
            let effective: HashMap<TaskId, Priority> = graph
                .tasks
                .iter()
                .map(|node| (node.id, node.effective_priority))
                .collect();
            let highest_return = self
                .return_candidates(&effective)?
                .into_iter()
                .filter(|candidate| candidate.progress.can_progress())
                .map(|candidate| candidate.priority)
                .max();
            let mut order = claimable.order;
            order.retain(|id| effective.get(id).is_some_and(|&priority| priority >= floor));
            let had_candidates_at_floor = !order.is_empty();
            order.retain(|id| !admission::claim_waits_for_return(effective[id], highest_return));
            // If the named claims disappear in the transaction, a fallback
            // could take a task behind a return or the competing landing.
            claimable.held.ordered_only = highest_return.is_some() || landing_waits_for_claim;
            if order.is_empty() {
                pass.end = if had_candidates_at_floor && !pass.started {
                    ClaimPassEnd::blocked(Blocked::Returning)
                } else {
                    pass.no_candidate(!graph.candidates.is_empty())
                };
                return Ok(ClaimStep::Next);
            }
            // While the load hold is on, a claim waits for the spacing
            // after the queue's latest claim, a light one too, and the next
            // pass judges the load again before it (ADR-t1479-1). The hold
            // itself returned above, before any claim.
            let mut resources = resources;
            self.read_claim_spacing(&mut resources)?;
            let spacing = resources.spacing_secs;
            let now_ms = resources.now_ms;
            let decision = admission::admit(
                vec![Candidate::slot(SlotKind::Claim, floor, order)],
                resources,
            );
            self.claim.admission_pass.observe(&decision);
            if let Some((_, reason)) = decision.blocked.first() {
                pass.end = ClaimPassEnd::blocked(*reason);
                if *reason == Blocked::ClaimSpacing {
                    pass.end.spacing_secs = spacing;
                    if pass.spaced_since.is_none() {
                        info!(
                            event = "claim_spaced",
                            "the next claim waits for the claim spacing of {}s after the latest claim",
                            self.claim.limits.claim_spacing.value
                        );
                    }
                    self.claim.spaced_since = Some(pass.spaced_since.unwrap_or(now_ms));
                }
                return Ok(ClaimStep::Stop);
            }
            let Some(candidate) = decision.starts.into_iter().next() else {
                // The pass already recorded the inconsistent decision.
                pass.end = ClaimPassEnd::blocked(Blocked::CandidatesDeferred);
                return Ok(ClaimStep::Stop);
            };
            let order = candidate.item;
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
                        pass.end = ClaimPassEnd::blocked(Blocked::LandingBranch);
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
                ClaimOutcome::NoReadyTask => {
                    self.claim
                        .admission_pass
                        .declined(Blocked::NoCandidates.name());
                    pass.end = pass.no_candidate(false);
                    return Ok(ClaimStep::Next);
                }
            };
            if let Some(left) = pass.resources.disk_claims.as_mut() {
                *left = left.saturating_sub(1);
            }
            self.claim.admission_pass.started();
            pass.started = true;
            pass.end = ClaimPassEnd::default();
            self.claim.defer.claimed();
            if self.provision_claimed(&run, attributes.load_avg)? == ClaimStep::Stop {
                return Ok(ClaimStep::Stop);
            }
        }
    }
    fn read_claim_spacing(&self, resources: &mut Resources) -> Result<()> {
        resources.spacing_secs =
            claim_spacing::in_effect(self.host.max_load(), self.claim.limits.claim_spacing.value);
        resources.now_ms = crate::application::unix_millis(self.generators.clock.system_time());
        resources.last_claim_ms = if resources.spacing_secs.is_some() {
            self.queue
                .latest_event_of(crate::domain::event_kind::RUN_CLAIMED)?
                .and_then(|event| crate::domain::stats::timestamp_millis(&event.created_at))
        } else {
            None
        };
        Ok(())
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
    /// tasks or the admission decision differ from the sample this process
    /// recorded last, a draining or handing-off pass included, and on its
    /// first pass ([`ObservationState::record_candidates`]).
    pub(super) fn sample_candidates(
        &mut self,
        parallel: usize,
        end: ClaimPassEnd,
        hold: Option<(EventKind, Value)>,
    ) {
        let sample = self.queue.graph_input().map(|input| {
            let graph = dependency_graph(input, None);
            CandidatesSample {
                end,
                candidates: graph.candidates.len(),
                free_slots: parallel.saturating_sub(self.used_slots()),
                ready: graph
                    .tasks
                    .iter()
                    .filter(|node| node.status == TaskStatus::Ready)
                    .count(),
            }
        });
        self.on_observation(|observation, env| observation.record_candidates(env, sample, hold));
    }
    /// Whether the free disk space this pass read lets no claim go
    /// ([`crate::domain::disk::DiskState::claims`]): short of what a claim
    /// needs, or a claim that waits for the one run
    /// of a project with no samples (a space that could not be read is not
    /// short).
    pub(super) fn claims_short_of_disk(&mut self) -> Result<bool> {
        Ok(self.host.disk_claims() == Some(0))
    }
    /// Carry the measurements for a hold alongside Admission's inputs.
    fn read_claim_resources(&self, mut resources: Resources) -> (Resources, Option<ClaimHold>) {
        let decision = self.host.disk_decision();
        let short = decision.as_ref().and_then(|d| d.claim_hold());
        let can_share = resources
            .landing_disk
            .is_some_and(|disk| disk.claims != Some(0));
        let mut standalone_short = short;
        let disk = if can_share {
            None
        } else {
            standalone_short.take()
        };
        let hold = if self.host.disk_cleaning() && disk.is_some() {
            // Cleanup is already acting on the disk shortage.
            disk
        } else {
            admission::claim_hold(
                disk,
                &HoldInputs {
                    queue_hold: self.queue_hold.filter(|_| self.routes().is_empty()),
                    ..HoldInputs::default()
                },
                &HoldInputs {
                    load_average: (self.host.load_average())(),
                    max_load: self.host.max_load(),
                    ..HoldInputs::default()
                },
            )
        };
        resources.claim_hold = hold
            .as_ref()
            .map(|hold| admission::hold_reason(hold.reason));
        if let Some(decision) = decision {
            resources = resources.with_disk(&decision);
        }
        (resources, hold.or(standalone_short))
    }

    /// Check the programs `[run.env]` names on this process's PATH
    /// (ADR-0049 decision 9) and record `run_env_program_missing` or
    /// `run_env_program_found` when the answer differs from the latest one
    /// on the queue, so a restarted supervisor does not repeat it. A
    /// `dagq.toml` that cannot be read leaves the last answer: provisioning
    /// and `integrate` report that file themselves.
    /// Each fill pass checks before claims, also during drain or handoff.
    /// A missing program holds new claims, needs_session resumes and landing
    /// starts; live runs, reviews, triage and already-started resumes continue.
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
