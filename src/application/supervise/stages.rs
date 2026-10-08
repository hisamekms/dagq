//! The state of 実行と着地 on the supervisor, by the stage that owns it
//! (docs/design/architecture.md, "`Supervisor`の状態"): the slot
//! assignment ([`ClaimState`], with the slots in a [`SlotTable`]), the
//! landing and its review ([`LandingState`]), the resume ([`ResumeState`]),
//! the triage and recovery ([`TriageState`]) and the providers
//! ([`ProviderState`]); the e2e's is [`super::e2e::E2eWaits`]. Each stage's
//! submodules change their own state, and the loop (`mod.rs`) the claim
//! pass's and the landing branch's holds and the moved workers; the slots
//! are changed only through [`SlotTable`]'s operations, which every stage
//! and the handoff use to take and give back a slot.

use super::*;

/// The slots, each holding one run and its phase. Which slots there are
/// changes only through these operations: a stage admits a run, releases
/// it or puts it back where it was; the loop takes every slot out for a
/// handoff. A slot's own phase is its stage's.
#[derive(Default)]
pub(super) struct SlotTable {
    slots: Vec<Slot>,
}

impl SlotTable {
    /// Give `slot`'s run a slot, after the others.
    pub(super) fn admit(&mut self, slot: Slot) {
        self.slots.push(slot);
    }

    /// Take the slot at `index` out: for good, or for a step that puts it
    /// back ([`Self::put_back`]).
    pub(super) fn release(&mut self, index: usize) -> Slot {
        self.slots.remove(index)
    }

    /// Put a slot taken out by [`Self::release`] back at `index`, keeping
    /// the order of the slots.
    pub(super) fn put_back(&mut self, index: usize, slot: Slot) {
        self.slots.insert(index, slot);
    }

    /// Take every slot out (a handoff, which keeps those it rebuilds).
    pub(super) fn take_all(&mut self) -> Vec<Slot> {
        std::mem::take(&mut self.slots)
    }

    pub(super) fn iter(&self) -> std::slice::Iter<'_, Slot> {
        self.slots.iter()
    }

    pub(super) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Whether a slot holds the run `id`.
    pub(super) fn holds(&self, id: &RunId) -> bool {
        self.slots.iter().any(|slot| slot.run.id() == id)
    }

    /// The runs the slots hold.
    pub(super) fn runs(&self) -> Vec<RunId> {
        self.slots
            .iter()
            .map(|slot| slot.run.id().clone())
            .collect()
    }

    /// The slots in use: every run but the waiting ones and those waiting
    /// to go back (ADR-0062 decision 5).
    pub(super) fn used(&self) -> usize {
        self.slots.iter().filter(|slot| !slot.out_of_slot()).count()
    }

    /// The runs that wait only for their landing turn (ADR-t1591-1).
    pub(super) fn landing_queue(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.in_landing_queue())
            .count()
    }

    /// The runs whose wait ended and that wait to go back to a slot.
    pub(super) fn returning(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.waiting.as_ref().is_some_and(|w| w.ended.is_some()))
            .count()
    }

    /// The runs held against `--max-waiting`: the waiting ones and those
    /// waiting to go back.
    pub(super) fn waiting(&self) -> usize {
        crate::domain::waiting::WaitCount::of(
            self.slots
                .iter()
                .filter_map(|slot| slot.waiting.as_ref())
                .map(|waiting| waiting.ended.is_some()),
        )
        .count()
    }

    /// Move the run at `index` out of the slots into `waiting`.
    pub(super) fn leave_for(&mut self, index: usize, waiting: Waiting) {
        self.slots[index].waiting = Some(waiting);
    }

    /// Note that the run at `index` waits in its slot for `ask`; whether
    /// it was not noted yet.
    pub(super) fn defer_wait(&mut self, index: usize, ask: AskId) -> bool {
        let deferred = &mut self.slots[index].deferred;
        if deferred.contains(&ask) {
            return false;
        }
        deferred.push(ask);
        true
    }

    /// Put the run at `index`, whose wait ended, back in its slot,
    /// starting its stage's clocks again at `now`; the wait it had, `None`
    /// when it had none.
    pub(super) fn regain(
        &mut self,
        index: usize,
        files: &dyn RunFiles,
        now: Instant,
    ) -> Option<Waiting> {
        let slot = &mut self.slots[index];
        let waiting = slot.waiting.take()?;
        slot.phase.restart_stage_clocks(files, now);
        Some(waiting)
    }
}

impl std::ops::Index<usize> for SlotTable {
    type Output = Slot;

    fn index(&self, index: usize) -> &Slot {
        &self.slots[index]
    }
}

/// The slot assignment: the slots and their limits, the workers a claim
/// may run on, whether this process claims, and the claims deferred on
/// conflict hotspots.
pub(super) struct ClaimState {
    pub(super) slots: SlotTable,
    /// The workers this supervisor runs: a candidate whose worker is not
    /// one of them is not claimed (ADR-t813-2).
    pub(super) workers: Vec<Worker>,
    /// `--parallel`: the slots in use (the runs not waiting) are held under
    /// it, apart from a run a person moved (ADR-0062 decision 10).
    pub(super) parallel: usize,
    /// `--max-waiting` (ADR-0062 decision 7).
    pub(super) max_waiting: usize,
    /// `parallel`, `max_waiting` and `runtime_planners` with where each
    /// comes from, as last resolved.
    pub(super) limits: SlotLimits,
    /// The flags given; a value not given follows `[supervisor]`.
    pub(super) slot_flags: SlotFlags,
    /// Reads `[supervisor]` again each pass; `None` keeps `limits`.
    pub(super) supervisor_file: Option<SupervisorFile>,
    /// The error the last read of `[supervisor]` failed with, warned of
    /// once until it changes or a read succeeds.
    pub(super) supervisor_error: Option<String>,
    /// `[supervisor] light_changes` as last read (ADR-t1591-1): the tasks
    /// claimed in the room the landing queue leaves.
    pub(super) light_changes: crate::domain::light_slots::LightChanges,
    /// Since when, in Unix milliseconds, a claim this process would make
    /// waits for the spacing after the queue's latest claim (ADR-t1479-1);
    /// `None` while none waits.
    pub(super) spaced_since: Option<i64>,
    /// Cleared after a provisioning failure so an unavailable cmux or Git
    /// does not burn through every candidate.
    pub(super) claiming: bool,
    pub(super) provisioning_error: Option<String>,
    /// The load samples of each held run's current interval.
    pub(super) loads: HashMap<RunId, LoadWindow>,
    /// The claims deferred on conflict hotspots (ADR-0069).
    pub(super) defer: claim_defer::DeferWatch,
    /// The `[conflicts]` thresholds the plan review's hotspots and the
    /// claims deferred on them are judged by, as last read (ADR-0080).
    pub(super) conflicts: crate::domain::stats::ConflictConfigReport,
    /// Reads `[conflicts]` again each pass (ADR-0080); `None` keeps
    /// `conflicts` as the options set it.
    pub(super) conflicts_file: Option<ConflictsFile>,
    /// The error the last read of `[conflicts]` failed with, warned of
    /// once until it changes or a read succeeds.
    pub(super) conflicts_error: Option<String>,
}

impl ClaimState {
    /// The room for the next claim under `parallel` (ADR-t1591-1).
    pub(super) fn room(&self, parallel: usize) -> light_slots::ClaimRoom {
        light_slots::claim_room(
            self.slots.used(),
            self.slots.landing_queue(),
            parallel,
            self.slots.returning(),
            !self.light_changes.is_empty(),
        )
    }

    /// Start the load window of `run`, claimed now, with its sample.
    pub(super) fn start_load(&mut self, run: &RunId, load: Option<f64>) {
        let mut window = LoadWindow::default();
        window.add(load);
        self.loads.insert(run.clone(), window);
    }

    /// The load over `run`'s interval that ends now, and start the next.
    pub(super) fn take_load(&mut self, run: &RunId) -> LoadSummary {
        self.loads.entry(run.clone()).or_default().take()
    }

    /// Drop the load window of `run`, which left its slot.
    pub(super) fn drop_load(&mut self, run: &RunId) {
        self.loads.remove(run);
    }

    /// Sample `load` once for every slot's current interval; the windows
    /// of runs no slot holds any more are dropped.
    pub(super) fn sample_load(&mut self, load: Option<f64>) {
        let held: HashSet<RunId> = self.slots.runs().into_iter().collect();
        self.loads.retain(|id, _| held.contains(id));
        for id in held {
            self.loads.entry(id).or_default().add(load);
        }
    }
}

/// The landing and the review before it: the holds of the landing branch
/// and of `[run.env]` (which stop the claims too), the landing rechecks,
/// and whether an unreadable review verdict is reviewed again.
#[derive(Default)]
pub(super) struct LandingState {
    /// A program `[run.env]` names did not resolve on this process's PATH
    /// at the last claim pass (ADR-0049 decision 9): nothing is claimed and
    /// no passed run lands until it does.
    pub(super) run_env_missing: bool,
    /// The landing branch did not resolve at the top of this pass
    /// (ADR-t615-1): nothing is claimed and no passed run lands until it
    /// does.
    pub(super) unresolved: bool,
    /// The reason of the landing branch's hold this process last recorded
    /// or found recorded (`Some(None)`: none); `None` before its first look
    /// at the queue, or after a record that failed.
    pub(super) recorded: Option<Option<&'static str>>,
    /// The stamp of the landing branch's inputs taken before its last
    /// resolution, and when: a pass whose stamp is the same, within
    /// [`LANDING_BRANCH_RECHECK`], keeps that resolution without starting
    /// Git.
    pub(super) stamp: Option<(crate::application::LandingBranchStamp, Instant)>,
    /// The landing recheck running and the one due (ADR-0068).
    pub(super) rechecks: recheck::Rechecks,
    pub(super) retry_unreadable_review: bool,
}

/// The resume, and the headless sessions opened again during a wait:
/// `supervise::reopen` changes the reopens, the session and the wait end
/// or note one through [`Self::forget_reopen`] and [`Self::reopen_registered`].
pub(super) struct ResumeState {
    /// `[resume]`: the limit of a run's conflict-only attempts (ADR-0047
    /// decision 24).
    pub(super) config: ResumeConfig,
    /// The headless sessions lost during a wait that are opened again, by
    /// run.
    pub(super) reopens: HashMap<RunId, reopen::ReopenWatch>,
}

impl ResumeState {
    /// The run's session is no longer opened again.
    pub(super) fn forget_reopen(&mut self, run: &RunId) {
        self.reopens.remove(run);
    }

    /// The wrapper of the run's session opened again registered.
    pub(super) fn reopen_registered(&mut self, run: &RunId) {
        if let Some(reopen) = self.reopens.get_mut(run) {
            reopen.registered();
        }
    }
}

/// The triage of failed runs and the recovery of live sessions.
#[derive(Default)]
pub(super) struct TriageState {
    /// The runs this process triaged, with where each one went.
    pub(super) triaged: Vec<Value>,
    /// The ends of the live sessions' recovery jobs whose verdict a person
    /// is asked about (run, alert, attempt), until the escalation records
    /// its `recovery_finished` ([`recovery::Escalation::record`]).
    pub(super) live_job_ends: Vec<(RunId, RecoveryAlert, usize, recovery::JobEnd)>,
    /// The latest listing of this user's processes for the `idle_process`
    /// alert: when it was taken, and the listing with its wall time, `None`
    /// when it failed. One listing serves every run.
    pub(super) process_sample: Option<(Instant, Option<ProcessSample>)>,
}

/// The providers the runs and jobs move between.
pub(super) struct ProviderState {
    /// `[provider_fallback]` as last read (ADR-t1857-1): whether a worker
    /// moves off a provider it cannot use.
    pub(super) fallback: crate::domain::provider_switch::ProviderFallback,
    /// Reads `[provider_fallback]` again each pass; `None` keeps
    /// `fallback`.
    pub(super) fallback_file: Option<ProviderFallbackFile>,
    /// The error the last read of `[provider_fallback]` failed with,
    /// warned of once until it changes or a read succeeds.
    pub(super) fallback_error: Option<String>,
    /// The providers held for the workers without an ask (ADR-t813-2
    /// decision 6): Codex for any reason, Claude for an agent that did not
    /// start. Their tasks run on the other provider meanwhile.
    pub(super) holds: Vec<crate::domain::provider_switch::ProviderHold>,
    /// The latest finishes of jobs on 観測と分析's timer whose provider
    /// this supervisor held, so that none is held twice
    /// ([`crate::domain::throughput_review::finishes_to_hold`]).
    pub(super) timer_finishes_held: Vec<EventId>,
    /// The runs whose worker moved to the other provider in this step: the
    /// slot's copy takes the new worker once the step returns.
    pub(super) moved: HashMap<RunId, Worker>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::change::{ChangeSet, TaskChange};
    use crate::domain::light_slots::{ClaimRoom, LightChanges};
    use crate::domain::slot_limits::SupervisorConfig;
    use crate::domain::waiting::WaitCause;

    fn slot(id: &str) -> Slot {
        let run = TaskRun::restore(crate::domain::RunRecord {
            id: RunId::new(id).unwrap(),
            task_id: TaskId::new(1),
            status: RunStatus::Running,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: Worker::default_mode(Provider::Claude),
            base_commit: CommitSha::parse("a".repeat(40), "commit").unwrap(),
            branch: None,
            worktree_path: None,
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: None,
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap();
        Slot::new(run, Phase::AwaitingSlot)
    }

    fn waiting(ended: Option<i64>) -> Waiting {
        Waiting {
            asks: Vec::new(),
            started_at: 0,
            since: UNIX_EPOCH,
            ended: ended.map(|at| (at, WaitCause::Answered)),
        }
    }

    /// The one monotonic time the tests pass, read once: what a slot does
    /// with it (an `AwaitingSlot` slot's clocks do not restart) does not
    /// depend on its value.
    fn origin() -> Instant {
        Instant::now()
    }

    fn ids(table: &SlotTable) -> Vec<String> {
        table
            .iter()
            .map(|slot| slot.run.id().as_str().to_owned())
            .collect()
    }

    fn claim_state(light: LightChanges) -> ClaimState {
        let limits = SlotLimits::resolve(SlotFlags::default(), &SupervisorConfig::default());
        ClaimState {
            slots: SlotTable::default(),
            workers: Vec::new(),
            parallel: limits.parallel.value,
            max_waiting: limits.max_waiting.value,
            limits,
            slot_flags: SlotFlags::default(),
            supervisor_file: None,
            supervisor_error: None,
            light_changes: light,
            spaced_since: None,
            claiming: true,
            provisioning_error: None,
            loads: HashMap::new(),
            defer: claim_defer::DeferWatch::default(),
            conflicts: crate::domain::stats::ConflictConfigReport::default(),
            conflicts_file: None,
            conflicts_error: None,
        }
    }

    /// A slot released for a step and put back keeps its place; one
    /// released for good is no longer held; a handoff takes every slot.
    #[test]
    fn slots_are_admitted_released_and_put_back_in_order() {
        let mut table = SlotTable::default();
        assert!(table.is_empty());
        for id in ["r1", "r2", "r3"] {
            table.admit(slot(id));
        }
        let taken = table.release(1);
        assert_eq!(taken.run.id().as_str(), "r2");
        assert_eq!(ids(&table), ["r1", "r3"]);
        assert!(!table.holds(taken.run.id()));
        table.put_back(1, taken);
        assert_eq!(ids(&table), ["r1", "r2", "r3"]);
        assert!(table.holds(&RunId::new("r2").unwrap()));
        let gone = table.release(0);
        assert_eq!(
            table.runs(),
            [RunId::new("r2").unwrap(), RunId::new("r3").unwrap()]
        );
        assert!(!table.holds(gone.run.id()));
        assert_eq!(table.take_all().len(), 2);
        assert!(table.is_empty());
    }

    /// A run that waits or waits to go back is out of the slots in use;
    /// both count against `--max-waiting`; only the ended one returns.
    /// Regaining a slot ends the wait once.
    #[test]
    fn a_wait_leaves_the_slots_in_use_until_the_slot_is_regained() {
        let mut table = SlotTable::default();
        for id in ["r1", "r2", "r3"] {
            table.admit(slot(id));
        }
        assert_eq!(
            (table.used(), table.waiting(), table.returning()),
            (3, 0, 0)
        );
        table.leave_for(0, waiting(None));
        table.leave_for(1, waiting(Some(10)));
        assert_eq!(
            (table.used(), table.waiting(), table.returning()),
            (1, 2, 1)
        );
        let files = crate::application::memory_files::MemoryFiles::default();
        let now = origin();
        let regained = table.regain(1, &files, now);
        assert_eq!(regained.and_then(|w| w.ended).map(|(at, _)| at), Some(10));
        assert!(table.regain(1, &files, now).is_none());
        assert_eq!(
            (table.used(), table.waiting(), table.returning()),
            (2, 1, 0)
        );
    }

    /// A deferred wait is noted once per ask.
    #[test]
    fn a_deferred_wait_is_noted_once_per_ask() {
        let mut table = SlotTable::default();
        table.admit(slot("r1"));
        assert!(table.defer_wait(0, AskId::new(7)));
        assert!(!table.defer_wait(0, AskId::new(7)));
        assert!(table.defer_wait(0, AskId::new(8)));
        assert_eq!(table[0].deferred, [AskId::new(7), AskId::new(8)]);
    }

    /// The room for a claim follows the slots the table holds: full slots
    /// leave none, a landing queue leaves room for a light task only when
    /// light changes are set, and a run waiting to go back takes the room
    /// first.
    #[test]
    fn the_claim_room_follows_the_slots_held() {
        let docs: TaskChange = "docs".parse().unwrap();
        let set = ChangeSet::new(vec![docs.clone()]).unwrap();
        let light = LightChanges::new(vec![docs], Some(&set)).unwrap();
        for (light, want) in [
            (LightChanges::default(), ClaimRoom::None),
            (light, ClaimRoom::LightOnly),
        ] {
            let mut claim = claim_state(light);
            assert_eq!(claim.room(2), ClaimRoom::Any);
            for id in ["r1", "r2"] {
                let mut queued = slot(id);
                queued.landing_turn = true;
                claim.slots.admit(queued);
            }
            assert_eq!(claim.room(2), want);
            claim.slots.admit(slot("r3"));
            claim.slots.leave_for(2, waiting(Some(1)));
            assert_eq!(claim.room(2), ClaimRoom::None);
        }
    }

    /// The load windows follow the runs the slots hold.
    #[test]
    fn the_load_windows_follow_the_slots_held() {
        let mut claim = claim_state(LightChanges::default());
        claim.slots.admit(slot("r1"));
        claim.slots.admit(slot("r2"));
        claim.sample_load(Some(1.0));
        assert_eq!(claim.loads.len(), 2);
        claim.slots.release(0);
        claim.sample_load(Some(2.0));
        assert_eq!(
            claim.loads.keys().cloned().collect::<Vec<_>>(),
            [RunId::new("r2").unwrap()]
        );
    }
}
