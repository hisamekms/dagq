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
//!
//! A run's moves between phases, and their records (`run_phase_changed`),
//! go through [`Slot::transition`], [`Slot::note_phase`], [`record_rest`]
//! and [`record_claimed`] only.

use super::*;
use crate::domain::pre_claim::{self, SlotClass};
use crate::domain::run_phase::{self, Attempt, PhaseChange};
use std::collections::BTreeSet;

/// How many of the queue's latest records of each `slots_full_*` a process
/// reads to find what its token left open.
const SLOTS_FULL_READ: usize = 256;

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

    /// [`Self::admit`] a slot, recording the phase its run enters, which
    /// `cause` moved it to ([`Slot::note_phase`]).
    pub(super) fn admit_noting<L: PhaseLog + ?Sized>(
        &mut self,
        mut slot: Slot,
        cause: &str,
        log: &L,
    ) {
        slot.note_phase(cause, log);
        self.admit(slot);
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

    /// Abandon the headless jobs of every slot ([`stop_job`]): nothing
    /// watches them once the loop ended, and their ends are written with
    /// the Execution of their agents.
    pub(super) fn abandon_jobs(&mut self) {
        for slot in &mut self.slots {
            stop_job(slot);
        }
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

    /// Record the phase of the run at `index` when it moved
    /// ([`Slot::note_phase`]): into a wait or back from one.
    pub(super) fn note_phase<L: PhaseLog + ?Sized>(&mut self, index: usize, cause: &str, log: &L) {
        self.slots[index].note_phase(cause, log);
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

/// Where the phases of the runs are recorded (`run_phase_changed`,
/// ADR-t1662-1 decision 2), and read back when a process takes a run up.
pub(super) trait PhaseLog {
    fn phase_events(&self, run: &RunId) -> Result<Vec<RunEvent>>;
    fn record_phase(&self, run: &RunId, change: &PhaseChange) -> Result<()>;
}

impl<T: RunLog + ?Sized> PhaseLog for T {
    fn phase_events(&self, run: &RunId) -> Result<Vec<RunEvent>> {
        self.run_events(run)
    }

    fn record_phase(&self, run: &RunId, change: &PhaseChange) -> Result<()> {
        self.record_runtime_event(run, EventKind::RunPhaseChanged, change.payload())
    }
}

/// A run's moves between phases go through these, and only they record
/// `run_phase_changed`: each record closes the phase before it, so that
/// the records cover the run from its claim to its end. Which phase the
/// run is in is read from its slot ([`Slot::recorded_phase`]) and, out of
/// one, from its status ([`run_phase::Phase::at_rest`]). A record that
/// fails is warned of and tried again on the next move: the run's own
/// steps never wait for it.
impl Slot {
    /// Move the slot's run to `phase`, which `cause` (the kind of the
    /// event that moved it, or a reason code) moved it to.
    pub(super) fn transition<L: PhaseLog + ?Sized>(&mut self, phase: Phase, cause: &str, log: &L) {
        self.phase = phase;
        self.note_phase(cause, log);
    }

    /// The phase the slot's run is in: a wait outside the slots, the
    /// landing queue (ADR-t1591-1), or the slot's own phase.
    pub(super) fn recorded_phase(&self) -> run_phase::Phase {
        match &self.waiting {
            Some(waiting) if waiting.ended.is_some() => run_phase::Phase::Returning,
            Some(_) => run_phase::Phase::Waiting,
            None if self.in_landing_queue() => run_phase::Phase::LandingQueue,
            None => self.phase.recorded(),
        }
    }

    /// Record the phase the slot's run is in when it is not the one last
    /// recorded ([`PhaseTrack::note`]).
    pub(super) fn note_phase<L: PhaseLog + ?Sized>(&mut self, cause: &str, log: &L) {
        let phase = self.recorded_phase();
        let started = self.phase.attempt();
        let blocked_by = self.blocked_by.as_ref().map(ToString::to_string);
        self.track
            .note(self.run.id(), phase, started, blocked_by, cause, log);
    }
}

/// What a slot keeps of its run's records: the attempt its phases are
/// recorded in, and the phase last recorded, read once from the run's
/// events when this process first notes it (another process's records,
/// its attempt).
#[derive(Debug, Default)]
pub(super) struct PhaseTrack {
    attempt: Option<Attempt>,
    last: Option<run_phase::Recorded>,
    seeded: bool,
}

impl PhaseTrack {
    /// The attempt the run's phases are recorded in; `None` until the
    /// track read the run's records.
    pub(super) fn attempt(&self) -> Option<Attempt> {
        self.seeded.then(|| self.attempt.unwrap_or(Attempt::FIRST))
    }

    /// Record that run `id` is in `phase`, which `cause` moved it to,
    /// when it is not the phase and attempt last recorded: `started` is
    /// the attempt the phase starts (a revise, a resume), else the run
    /// goes on in its attempt.
    pub(super) fn note<L: PhaseLog + ?Sized>(
        &mut self,
        id: &RunId,
        phase: run_phase::Phase,
        started: Option<Attempt>,
        blocked_by: Option<String>,
        cause: &str,
        log: &L,
    ) {
        if !self.seeded {
            let events = match log.phase_events(id) {
                Ok(events) => events,
                Err(error) => {
                    warn!(run_id = %id, error = %format_args!("{error:#}"), "run {id}: its recorded phases could not be read: {error:#}");
                    return;
                }
            };
            self.last = run_phase::last_recorded(&events);
            self.attempt = self.last.as_ref().map(|last| last.attempt);
            self.seeded = true;
        }
        if started.is_some() {
            self.attempt = started;
        }
        let change = PhaseChange::new(phase, self.attempt.unwrap_or(Attempt::FIRST), cause)
            .blocked_by(blocked_by);
        if change.repeats(self.last.as_ref()) {
            return;
        }
        match log.record_phase(id, &change) {
            Ok(()) => {
                self.last = Some(change.recorded());
            }
            Err(error) => {
                warn!(run_id = %id, error = %format_args!("{error:#}"), "run {id}: could not record its phase {}: {error:#}", phase.name());
            }
        }
    }
}

/// Record the phase a run is in that no slot holds (or whose slot is let
/// go), from its status: `attempt` is its slot's, `None` the one last
/// recorded. Nothing is recorded for a run on its way or landed
/// ([`run_phase::Phase::at_rest`]), nor when it repeats the last record.
pub(super) fn record_rest<L: PhaseLog + ?Sized>(
    log: &L,
    run: &TaskRun,
    attempt: Option<Attempt>,
    cause: &str,
) {
    let recorded = log.phase_events(run.id()).and_then(|events| {
        let queued = RunHistory::from_events(&events).queued_approval().is_some();
        let Some(phase) = run_phase::Phase::at_rest(run.status(), queued) else {
            return Ok(());
        };
        let last = run_phase::last_recorded(&events);
        let attempt = attempt
            .or_else(|| last.as_ref().map(|last| last.attempt))
            .unwrap_or(Attempt::FIRST);
        let change = PhaseChange::new(phase, attempt, cause);
        if change.repeats(last.as_ref()) {
            return Ok(());
        }
        log.record_phase(run.id(), &change)
    });
    if let Err(error) = recorded {
        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not record its phase: {error:#}", run.id());
    }
}

/// Record that a run just claimed is being provisioned: its first phase.
pub(super) fn record_claimed<L: PhaseLog + ?Sized>(log: &L, run: &RunId) {
    let change = PhaseChange::new(
        run_phase::Phase::Provisioning,
        Attempt::FIRST,
        EventKind::RunClaimed.as_str(),
    );
    if let Err(error) = log.record_phase(run, &change) {
        warn!(run_id = %run, error = %format_args!("{error:#}"), "run {run}: could not record its phase: {error:#}");
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
    /// The classes whose slots-full interval this supervisor's token has
    /// open (ADR-t1662-1 decision 6); `None` until read from its records.
    pub(super) slots_full: Option<BTreeSet<SlotClass>>,
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

    /// Record where the slots of a class fill up or free again, as the
    /// supervisor `token` with `parallel` slots finds them at the end of
    /// its claim pass ([`pre_claim::slots_full_changes`]); the classes it
    /// left open are read once from its records, which a process that took
    /// its token over goes on from. A record that fails is warned of and
    /// tried again on the next pass.
    pub(super) fn note_slots_full(
        &mut self,
        log: &dyn RunLog,
        token: &LeaseToken,
        parallel: usize,
    ) {
        let mut open = match self.slots_full.take() {
            Some(open) => open,
            None => {
                let records = [
                    crate::domain::event_kind::SLOTS_FULL_STARTED,
                    crate::domain::event_kind::SLOTS_FULL_ENDED,
                ]
                .into_iter()
                .map(|kind| log.latest_events_of(kind, SLOTS_FULL_READ))
                .collect::<Result<Vec<_>>>();
                match records {
                    Ok(records) => pre_claim::open_slots_full(&records.concat(), token.as_str()),
                    Err(error) => {
                        warn!(error = %format_args!("{error:#}"), "the supervisor's slots-full records could not be read: {error:#}");
                        BTreeSet::new()
                    }
                }
            }
        };
        let full = pre_claim::full_classes(self.room(parallel), !self.light_changes.is_empty());
        for (kind, class) in pre_claim::slots_full_changes(&open, &full) {
            let payload = pre_claim::slots_full_payload(token, class, self.slots.used(), parallel);
            match log.record_queue_event(kind, payload) {
                Ok(_) if kind == EventKind::SlotsFullStarted => {
                    open.insert(class);
                }
                Ok(_) => {
                    open.remove(&class);
                }
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "could not record {kind} for the {} slots: {error:#}", class.as_str());
                }
            }
        }
        self.slots_full = Some(open);
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
        Slot::new(run_of(id, RunStatus::Running), Phase::AwaitingSlot)
    }

    fn run_of(id: &str, status: RunStatus) -> TaskRun {
        TaskRun::restore(crate::domain::RunRecord {
            id: RunId::new(id).unwrap(),
            task_id: TaskId::new(1),
            status,
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
        .unwrap()
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
            slots_full: None,
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

    /// The phases a fake log records, and the run's other events that
    /// [`record_rest`] reads.
    #[derive(Default)]
    struct Records {
        events: std::cell::RefCell<Vec<RunEvent>>,
    }

    impl Records {
        fn push(&self, kind: EventKind, payload: Value) {
            let mut events = self.events.borrow_mut();
            let id = crate::domain::EventId::new(events.len() as i64 + 1);
            events.push(RunEvent {
                id,
                task_id: None,
                goal_id: None,
                run_id: None,
                kind: kind.as_str().to_owned(),
                payload,
                created_at: String::new(),
                actor: None,
            });
        }

        /// The recorded phases, each with its attempt and cause.
        fn phases(&self) -> Vec<(String, String, String)> {
            self.events
                .borrow()
                .iter()
                .filter(|event| event.kind == event_kind::RUN_PHASE_CHANGED)
                .map(|event| {
                    let p = &event.payload;
                    let attempt = format!(
                        "{}{}",
                        p["attempt"]["kind"].as_str().unwrap(),
                        p["attempt"]["n"]
                    );
                    (
                        p["phase"].as_str().unwrap().to_owned(),
                        attempt,
                        p["cause"].as_str().unwrap().to_owned(),
                    )
                })
                .collect()
        }

        /// The names of the recorded phases, after checking that they
        /// cover the run from its claim with no gap: the first is its
        /// claim, and each record moves it to another phase or attempt
        /// than the one it closes.
        fn covered(&self) -> Vec<String> {
            let phases = self.phases();
            assert_eq!(phases[0].0, "provisioning", "{phases:?}");
            assert_eq!(phases[0].2, "run_claimed", "{phases:?}");
            for pair in phases.windows(2) {
                assert_ne!(
                    (&pair[0].0, &pair[0].1),
                    (&pair[1].0, &pair[1].1),
                    "{phases:?}"
                );
            }
            phases.into_iter().map(|(phase, _, _)| phase).collect()
        }

        /// The landing's records: the push, in the transaction of
        /// `run_integrated`, and the push's outcome `kind`.
        fn land(&self, id: &RunId, outcome: EventKind) {
            let attempt = run_phase::last_recorded(&self.events.borrow())
                .map_or(Attempt::FIRST, |last| last.attempt);
            self.push(EventKind::RunIntegrated, json!({}));
            self.record_phase(
                id,
                &PhaseChange::new(
                    run_phase::Phase::Push { in_slot: true },
                    attempt,
                    "run_integrated",
                ),
            )
            .unwrap();
            self.push(outcome, json!({}));
            let (phase, cause) = run_phase::Phase::after_push(outcome);
            self.record_phase(id, &PhaseChange::new(phase, attempt, cause))
                .unwrap();
        }
    }

    impl PhaseLog for Records {
        fn phase_events(&self, _: &RunId) -> Result<Vec<RunEvent>> {
            Ok(self.events.borrow().clone())
        }

        fn record_phase(&self, _: &RunId, change: &PhaseChange) -> Result<()> {
            self.push(EventKind::RunPhaseChanged, change.payload());
            Ok(())
        }
    }

    fn with_status(slot: &Slot, status: RunStatus) -> TaskRun {
        run_of(slot.run.id().as_str(), status)
    }

    use crate::domain::run_phase::{AttemptKind, Phase as P};

    /// The supervisor's moves of one run, in the phases they record: a
    /// slot admitted at the claim, and each move noted as the loop notes
    /// it.
    struct Flow {
        records: Records,
        slot: Slot,
    }

    impl Flow {
        fn claimed() -> Self {
            let records = Records::default();
            let slot = slot("r1");
            records.push(EventKind::RunClaimed, json!({}));
            record_claimed(&records, slot.run.id());
            let mut flow = Self { records, slot };
            flow.moves(P::Worker, None);
            flow
        }

        fn moves(&mut self, phase: P, started: Option<Attempt>) {
            let id = self.slot.run.id().clone();
            self.slot
                .track
                .note(&id, phase, started, None, "test", &self.records);
        }

        /// Through validation, review and the exit to the landing queue.
        fn queue_to_land(&mut self) {
            for phase in [
                P::Validating,
                P::Review,
                P::Exiting,
                P::AwaitingSlot,
                P::LandingQueue,
            ] {
                self.moves(phase, None);
            }
        }

        fn lands(&mut self, outcome: EventKind) {
            self.queue_to_land();
            self.moves(P::Landing, None);
            let id = self.slot.run.id().clone();
            self.records.land(&id, outcome);
        }

        /// The slot is let go with the run at rest in `status`.
        fn rests(&mut self, status: RunStatus, cause: &str) {
            let run = with_status(&self.slot, status);
            record_rest(&self.records, &run, self.slot.track.attempt(), cause);
        }
    }

    const LANDED: [&str; 10] = [
        "provisioning",
        "worker",
        "validating",
        "review",
        "exiting",
        "awaiting_slot",
        "landing_queue",
        "landing",
        "push",
        "ended",
    ];

    /// A first attempt lands and its push ends the run, whether main was
    /// pushed or the push skipped; a failed push leaves it pending.
    #[test]
    fn a_landed_run_is_covered_until_its_push_ends() {
        for (outcome, last, cause) in [
            (EventKind::PushFinished, "ended", "pushed"),
            (EventKind::PushSkipped, "ended", "push_skipped"),
            (EventKind::PushFailed, "push_pending", "push_failed"),
        ] {
            let mut flow = Flow::claimed();
            flow.lands(outcome);
            let mut expected = LANDED.to_vec();
            *expected.last_mut().unwrap() = last;
            assert_eq!(flow.records.covered(), expected);
            let phases = flow.records.phases();
            assert_eq!(phases.last().unwrap().2, cause);
            assert!(phases.iter().all(|(_, attempt, _)| attempt == "first1"));
        }
    }

    /// A revise is an attempt of its own, which the phases after it go on
    /// in, through the landing and the push.
    #[test]
    fn a_revise_records_its_attempt_until_the_run_ends() {
        let mut flow = Flow::claimed();
        flow.moves(P::Validating, None);
        flow.moves(P::Review, None);
        flow.moves(P::Revise, Some(Attempt::of(AttemptKind::Revise, 1)));
        flow.lands(EventKind::PushFinished);
        let phases = flow.records.phases();
        assert_eq!(
            flow.records.covered(),
            [
                &["provisioning", "worker", "validating", "review", "revise"][..],
                &LANDED[2..]
            ]
            .concat()
        );
        let revise = phases
            .iter()
            .position(|(phase, _, _)| phase == "revise")
            .unwrap();
        assert!(
            phases[..revise]
                .iter()
                .all(|(_, attempt, _)| attempt == "first1")
        );
        assert!(
            phases[revise..]
                .iter()
                .all(|(_, attempt, _)| attempt == "revise1"),
            "{phases:?}"
        );
    }

    /// A run parked for a resume waits for it at rest, and its resumed
    /// session's slot goes on in the resume's attempt.
    #[test]
    fn a_resume_closes_the_wait_for_it_and_records_its_attempt() {
        let mut flow = Flow::claimed();
        flow.moves(P::Validating, None);
        flow.moves(P::Exiting, None);
        flow.rests(RunStatus::NeedsSession, "needs_session");
        // The resume's slot is a new one, which reads the records.
        let mut resumed = Slot::new(
            with_status(&flow.slot, RunStatus::Running),
            Phase::AwaitingSlot,
        );
        let id = resumed.run.id().clone();
        resumed.track.note(
            &id,
            P::Resume,
            Some(Attempt::of(AttemptKind::Resume, 1)),
            None,
            "resume_started",
            &flow.records,
        );
        flow.slot = resumed;
        flow.lands(EventKind::PushFinished);
        assert_eq!(
            flow.records.covered(),
            [
                &[
                    "provisioning",
                    "worker",
                    "validating",
                    "exiting",
                    "needs_session",
                    "resume"
                ][..],
                &LANDED[2..]
            ]
            .concat()
        );
        let phases = flow.records.phases();
        assert_eq!(phases[4].1, "first1");
        assert_eq!(phases.last().unwrap().1, "resume1");
    }

    /// The answer to an `approve_landing` ask is a person's wait, which
    /// holds no slot; a `land` queues the run, and its landing follows.
    #[test]
    fn an_approve_landing_ask_is_a_wait_for_a_person() {
        let mut flow = Flow::claimed();
        flow.moves(P::Validating, None);
        flow.moves(P::Review, None);
        flow.moves(P::Exiting, None);
        flow.rests(RunStatus::AwaitingIntegration, "awaiting_integration");
        let waits = flow.records.events.borrow().last().unwrap().payload.clone();
        assert_eq!(
            (&waits["phase"], &waits["blocker"], &waits["holds"]),
            (&json!("landing_answer"), &json!("human"), &json!("none"))
        );
        flow.records
            .push(EventKind::IntegrationApproved, json!({"ask_id": 3}));
        flow.records.push(
            EventKind::LandingQueued,
            json!({"via": "approve", "ask_id": 3}),
        );
        let run = with_status(&flow.slot, RunStatus::AwaitingIntegration);
        record_rest(&flow.records, &run, None, "integration_approved");
        // The landing's own slot.
        flow.slot = Slot::new(run, Phase::Landing(None));
        let id = flow.slot.run.id().clone();
        flow.slot.track.note(
            &id,
            P::Landing,
            None,
            None,
            "integration_started",
            &flow.records,
        );
        flow.records.land(&id, EventKind::PushFinished);
        assert_eq!(
            flow.records.covered(),
            [
                "provisioning",
                "worker",
                "validating",
                "review",
                "exiting",
                "landing_answer",
                "landing_queue",
                "landing",
                "push",
                "ended",
            ]
        );
    }

    /// A worker's question takes the run out of the slots while a person
    /// answers, and back.
    #[test]
    fn a_question_is_a_wait_outside_the_slots() {
        let mut flow = Flow::claimed();
        flow.slot.waiting = Some(waiting(None));
        let phase = flow.slot.recorded_phase();
        flow.moves(phase, None);
        flow.slot.waiting = Some(waiting(Some(1)));
        let phase = flow.slot.recorded_phase();
        flow.moves(phase, None);
        flow.slot.waiting = None;
        flow.moves(P::Worker, None);
        assert_eq!(
            flow.records.covered(),
            ["provisioning", "worker", "waiting", "returning", "worker"]
        );
        assert_eq!(P::Waiting.tags().blocker, run_phase::Blocker::Human);
    }

    /// A run that does not land ends failed, or canceled by the answer to
    /// its ask; a repeated end records nothing more.
    #[test]
    fn a_run_that_does_not_land_ends_failed_or_canceled() {
        let mut failed = Flow::claimed();
        failed.rests(RunStatus::Failed, "failed");
        failed.rests(RunStatus::Failed, "runtime_error");
        assert_eq!(
            failed.records.covered(),
            ["provisioning", "worker", "ended"]
        );
        assert_eq!(failed.records.phases().last().unwrap().2, "failed");

        let mut canceled = Flow::claimed();
        canceled.moves(P::Validating, None);
        canceled.moves(P::Review, None);
        canceled.moves(P::Exiting, None);
        canceled.rests(RunStatus::AwaitingIntegration, "awaiting_integration");
        canceled.rests(RunStatus::Failed, "canceled");
        assert_eq!(
            canceled.records.covered(),
            [
                "provisioning",
                "worker",
                "validating",
                "review",
                "exiting",
                "landing_answer",
                "ended"
            ]
        );
        assert_eq!(canceled.records.phases().last().unwrap().2, "canceled");
    }

    /// A slot taken up by another process reads what was recorded: it
    /// records nothing while the run stays in the recorded phase, and
    /// goes on in the recorded attempt.
    #[test]
    fn a_slot_taken_up_goes_on_from_the_records() {
        let mut flow = Flow::claimed();
        flow.moves(P::Revise, Some(Attempt::of(AttemptKind::Conflict, 2)));
        let before = flow.records.phases().len();
        let mut adopted = Slot::new(
            with_status(&flow.slot, RunStatus::Running),
            Phase::AwaitingSlot,
        );
        let id = adopted.run.id().clone();
        adopted
            .track
            .note(&id, P::Revise, None, None, "run_adopted", &flow.records);
        assert_eq!(flow.records.phases().len(), before);
        adopted.track.note(
            &id,
            P::Validating,
            None,
            None,
            "conflict_resolved",
            &flow.records,
        );
        assert_eq!(flow.records.phases().last().unwrap().1, "conflict2");
        assert_eq!(
            adopted.track.attempt(),
            Some(Attempt::of(AttemptKind::Conflict, 2))
        );
    }

    /// The phase of a slot's run: a wait outside the slots or the landing
    /// queue before the slot's own phase.
    #[test]
    fn a_slots_phase_is_its_wait_or_the_landing_queue_before_its_own() {
        let mut slot = slot("r1");
        assert_eq!(slot.recorded_phase(), P::AwaitingSlot);
        slot.landing_turn = true;
        assert_eq!(slot.recorded_phase(), P::LandingQueue);
        slot.waiting = Some(waiting(None));
        assert_eq!(slot.recorded_phase(), P::Waiting);
        slot.waiting = Some(waiting(Some(1)));
        assert_eq!(slot.recorded_phase(), P::Returning);
    }

    /// A run in the landing queue records the run its look found holding
    /// the landing slot, and again when another one holds it.
    #[test]
    fn the_landing_queue_names_the_run_that_held_the_landing_slot() {
        let records = Records::default();
        let mut slot = slot("r1");
        slot.landing_turn = true;
        let holders = || -> Vec<Option<String>> {
            records
                .events
                .borrow()
                .iter()
                .map(|event| event.payload["blocked_by"].as_str().map(str::to_owned))
                .collect()
        };
        for holder in ["r0", "r0", "r2"] {
            slot.blocked_by = Some(RunId::new(holder).unwrap());
            slot.note_phase("landing_turn", &records);
        }
        assert_eq!(holders(), [Some("r0".to_owned()), Some("r2".to_owned())]);
        slot.landing_turn = false;
        slot.note_phase("observed", &records);
        assert_eq!(holders().last(), Some(&None));
    }
}
