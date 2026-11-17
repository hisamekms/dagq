//! The free disk space a claim and a landing need (ADR-0047 decision 44,
//! ADR-t4473-1, ADR-t4542-1): on every pass the supervisor reads the free
//! and total bytes of the queue's directory and, from the recent runs'
//! samples, unfinished disk-owning runs, their sizes measured off the loop
//! ([`super::moving_sizes`]) and phase properties, one decision for claims and
//! landing candidates ([`crate::domain::disk::DiskState::decide`]). Short of either, it cleans what the ended runs
//! left ([`HostOpsState::request_cleanup`] and `git worktree prune`,
//! off the loop: task 405) and reads again once that is done, recording
//! `auto_repaired` (`repair: disk_cleanup`) when it freed something; the
//! claims and landings wait for it without a hold, and for the rest of it
//! when another job took it on (task 1478). While one waits or runs no
//! other is asked for, and the next waits [`CLEANUP_INTERVAL`] from
//! the end of the last ([`asks_for_cleanup`], task 1627): the pass after
//! it judges the reading after it. Still short, it opens
//! the queue's one `cost` ask about the disk (`subject: disk`) once, and
//! the runs whose landing waits for the disk join it; but only below the
//! reserve after cleanup, with no run in flight, and the runtime
//! closes it only once there is room with a margin ([`disk_ask_condition`]). The claims are held through
//! the claim pass's hold (`claim_held`, reason `disk_space`), the
//! landings through `landing_held` / `landing_resumed`; a run waiting to
//! land stays awaiting integration and starts no verification.

use super::{cleanup::DiskRequest, *};
use crate::domain::EventKind;
use crate::domain::{
    Ask,
    disk::{
        DISK_CLEANUP, DISK_OPTIONS, DISK_SUBJECT, DiskDecision, DiskNeeds, DiskRelaxed, DiskRun,
        DiskStage, DiskState, DiskUsage, LandingCandidate, RELAX_KINDS, RelaxState, RunSample,
        StageBudgets, StageObservation, StagePeaks, StageSamples, gib, relax_state,
        size_exceeded_expected, stage_budgets,
    },
};

/// How long the samples read from the recent runs' records are kept.
const SAMPLES_INTERVAL: Duration = Duration::from_secs(60);
/// Least time from the end of a cleanup for room (the rest of one another
/// job took on included) to the next one (task 1627).
pub const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

/// The answer the runtime closes the disk ask with once there is room.
const DISK_ENOUGH_CLOSED: &str = "the free disk space is enough again; closed by the runtime";

/// The question of the disk ask; the runs whose landing waits follow it.
const DISK_QUESTION: &str = "With no run moving, the free disk space of the queue's directory is below the reserve, and cleaning what the ended runs left (their build outputs and those of the runs nobody works on that wait for an answer, a landing or a resume, the worktrees, the Claude Code scratchpads and the run TMPDIRs of completed and canceled tasks' runs, `git worktree prune`, and the target directories of the automatic update and the landing recheck when neither runs) did not free enough: {free} free, a new run needs {claim} and a landing's verification {landing} (the reserve of {reserve}, [disk] reserve of dagq.toml as a share of the volume and at least {least}, plus what a run is expected to take, {expected} ({source}), with unused reservations for each stage). No new run is claimed and no run lands below the reserve. Free disk space (for example the worktrees of failed runs nobody looks at any more, or other files on that disk) and answer `done`, or answer `wait` to leave the queue waiting: the supervisor resumes by itself once there is room.";

/// Activity in this pass's unfinished-run registry, independent of slots.
/// `in_flight` counts entries whose `DiskUsage::active` is true; `landing`
/// counts the INTEGRATOR entries. `landing_waiting` says a queued candidate
/// could advance but for the disk. Activity controls idle progress and asks.
#[derive(Debug, Clone, Copy)]
pub(in crate::application::supervise) struct DiskMoves {
    pub(in crate::application::supervise) parallel: usize,
    pub(in crate::application::supervise) in_flight: usize,
    pub(in crate::application::supervise) landing: usize,
    pub(in crate::application::supervise) landing_waiting: bool,
}

impl DiskMoves {
    /// Other active runs used by the shared landing requirement. The
    /// candidate itself is excluded at start and retry alike, preserving
    /// reserve-only progress when no other run is active.
    pub(in crate::application::supervise) const fn besides_the_landing(self) -> usize {
        self.in_flight.saturating_sub(self.landing)
    }
}

/// Registry entry, independent of slot ownership or approval source.
pub(in crate::application::supervise) struct DiskTarget {
    pub target: super::moving_sizes::MovingTarget,
    pub usage: DiskUsage,
    pub task: TaskId,
    pub phase: String,
    pub container: bool,
    pub candidate: bool,
    pub waiting: bool,
}

/// What the supervisor knows of the disk between passes.
#[derive(Debug, Default)]
pub(in crate::application::supervise) struct DiskWatch {
    /// The recent runs' samples and stage peaks, and when they were read.
    samples: Option<(Instant, Vec<RunSample>, StageSamples)>,
    /// The WORKER and INTEGRATOR peaks of the registered runs in this
    /// supervisor process, recorded `run_stage_peak` as each stage is left.
    stage_peaks: StagePeaks<Instant>,
    /// Successfully recorded exceedances for this supervisor process.
    /// Retained across waits/resumes; restart allows one further event.
    exceeded: std::collections::HashSet<RunId>,
    /// Immutable pass result shared with landing/retry gates and status.
    pub(super) decision: std::sync::Arc<std::sync::Mutex<Option<DiskDecision>>>,
    /// What this pass read of the disk: what a claim and a landing need.
    pub(super) needs: DiskNeeds,
    /// How many new runs this pass may claim ([`DiskState::claims`]);
    /// `None` for no limit.
    pub(super) claims: Option<usize>,
    /// How many runs moved at the latest pass's reading besides the
    /// landings under way ([`DiskMoves::besides_the_landing`]), shared with
    /// the landings' retries.
    pub(super) moving: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// When a cleanup for room was last asked for, then when it (or the
    /// rest of it another job took on) ended; `None` while there is room.
    pub(super) cleaned: Option<Instant>,
    /// The disk ask of this shortage was opened (or answered `wait`): it is
    /// not opened again until there is room, or a person answers `done`.
    asked: bool,
    /// The runs added to the disk ask.
    joined: Vec<RunId>,
    /// A cleanup for room runs or waits to: nothing is held or asked for
    /// until it is done.
    pub(super) cleaning: bool,
    /// This pass's reading is short of what a claim or a landing needs.
    short: bool,
    /// The latest completed cleanup's largest directories; `None` until
    /// a cleanup for room finishes. Measured only by the job thread.
    pub(super) breakdown: Option<Vec<(std::path::PathBuf, u64)>>,
    /// A cleanup can justify an ask only within the same idle epoch.
    pub(super) cleanup_epoch: CleanupEpoch,
}

/// A change between moving and idle invalidates a cleanup started before
/// it. A job finishing after the last moving run ends must not certify
/// that run's leftovers as cleaned.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct CleanupEpoch {
    pub(super) generation: u64,
    moving: bool,
}

struct CleanupTransition {
    next: CleanupEpoch,
    discard_breakdown: bool,
    retry_now: bool,
}

pub(super) struct CleanupCompletion {
    pub(super) ready: bool,
    pub(super) cooldown: bool,
}

impl CleanupEpoch {
    fn read(self, in_flight: usize) -> CleanupTransition {
        let moving = in_flight > 0;
        let changed = moving != self.moving;
        CleanupTransition {
            next: Self {
                generation: self.generation.wrapping_add(u64::from(changed)),
                moving,
            },
            discard_breakdown: moving || changed,
            retry_now: self.moving && !moving,
        }
    }

    /// Only an idle cleanup for room can supply the breakdown for an ask.
    /// Moving-era results are discarded, so avoid those heavy walks.
    pub(super) fn measures_breakdown(self, for_disk: bool) -> bool {
        for_disk && !self.moving
    }

    /// A stale completion while idle must not postpone the fresh cleanup;
    /// while runs move, ordinary cleanup throttling still applies.
    pub(super) fn completed(self, generation: u64) -> CleanupCompletion {
        let current = generation == self.generation;
        CleanupCompletion {
            ready: current && !self.moving,
            cooldown: current || self.moving,
        }
    }

    /// Neither a pending cleanup for room nor an ordinary job still
    /// running permits a new ask, even with an earlier measurement.
    fn ready(self, measured: bool, for_disk: bool, running: bool) -> bool {
        !self.moving && measured && !for_disk && !running
    }
}

impl HostOpsState {
    /// Freeze one `DiskDecision` from this pass's free space, expectations,
    /// cached trusted sizes and `moving`, the unfinished-run registry with
    /// stage facts regardless of slots or approval source.
    /// `moves` summarizes registry activity and landing priority; `held`
    /// carries cleanup protection and eligible waiting-run candidates.
    /// The stored result supplies claim and candidate landing/retry gates;
    /// its blocked candidates drive landing holds and disk ask membership.
    /// Cleanup and ask transitions use the same pass's reading.
    pub(in crate::application::supervise) fn check_disk(
        &mut self,
        env: &mut HostEnv<'_>,
        interval: Duration,
        held: &super::cleanup::Held,
        moves: DiskMoves,
        moving: &[DiskTarget],
    ) -> Result<()> {
        let DiskMoves {
            in_flight,
            landing_waiting,
            ..
        } = moves;
        let unclosed: Vec<Ask> = env
            .queue
            .asks(AskQuery::default())?
            .into_iter()
            .filter(is_disk_ask)
            .collect();
        self.apply_disk_answers(env, &unclosed)?;
        let estimate_time = crate::application::unix_millis(env.generators.clock.system_time());
        let StageBudgets {
            worker: expected,
            integrator: integrator_expected,
            sample_count,
        } = self.expected_run_size(env, estimate_time)?;
        let relaxed = self.relaxation(env)?;
        let free = self.free_bytes(env);
        // Stage reservations and exceedances share trusted sizes measured
        // off the loop. An unknown size reserves the stage's whole budget.
        // This pass's registered targets are walked for the next.
        let now = env.generators.clock.monotonic();
        let measured = self.moving_sizes.frozen();
        let sizes = measured.trusted_sizes(
            &moving.iter().map(|r| r.target.clone()).collect::<Vec<_>>(),
            now,
        );
        let runs = moving
            .iter()
            .zip(&sizes)
            .map(|(r, (_, current))| DiskRun {
                stage: r.usage.stage,
                current: current.map(|(bytes, _)| bytes),
            })
            .collect();
        self.record_stage_peaks(env, moving, &sizes)?;
        for (r, (run_id, size)) in moving.iter().zip(&sizes).filter(|(r, _)| r.usage.active) {
            let size = size.map(|(bytes, _)| bytes);
            if let Some((bytes, expected)) =
                size_exceeded_expected(size, expected, self.disk.exceeded.contains(run_id))
            {
                let task_id = env.queue.run(run_id)?.task_id();
                env.queue.record_runtime_event(run_id, EventKind::RunSizeExceededExpected,
                    serde_json::json!({"run_id":run_id,"task_id":task_id,"bytes":bytes,"expected":expected,"estimate":self.disk_config.estimate}))?;
                self.disk.exceeded.insert(r.target.run.clone());
            }
        }
        let candidates: Vec<_> = moving
            .iter()
            .zip(&sizes)
            .filter(|(r, _)| r.candidate)
            .map(|(r, (_, current))| LandingCandidate {
                run: r.target.run.clone(),
                current: current.map(|(bytes, _)| bytes),
                integrating: r.usage.stage == DiskStage::Integrator,
                waiting: r.waiting,
            })
            .collect();
        let state = DiskState {
            free,
            total: self.total_bytes(env).unwrap_or(0),
            expected,
            integrator_expected,
            runs,
            moving: in_flight,
            landing_current: None,
            landing_waiting,
            relaxed,
        };
        let mut decision = state.decide(
            self.disk_config.reserve,
            &candidates,
            self.disk_config.estimate,
            sample_count,
        );
        let held_interval = self
            .host_metrics_port
            .as_ref()
            .and_then(|p| p.held_targets.as_ref())
            .map_or_else(
                || {
                    Duration::from_secs(
                        crate::domain::host_metrics::DEFAULT_HELD_TARGETS_INTERVAL_SECS,
                    )
                },
                |p| p.interval,
            );
        let run_views = moving
            .iter()
            .map(|r| {
                let measurement = if r.usage.stage == DiskStage::Waiting {
                    self.host_metrics.snapshot(
                        &r.target.run,
                        r.target.worktree.as_deref(),
                        now,
                        held_interval,
                    )
                } else {
                    measured.snapshot(&r.target, now)
                };
                crate::application::claim_view::run_disk_status(
                    &decision.needs,
                    crate::application::claim_view::RunDiskInput {
                        run: r.target.run.clone(),
                        task: r.task,
                        stage: r.usage.stage,
                        phase: r.phase.clone(),
                        container: r.container,
                        measurement,
                    },
                )
            })
            .collect();
        let sources = self.disk.samples.as_ref().map_or(
            ["single run estimate fallback"; 2],
            |(_, runs, stages)| {
                crate::application::claim_view::disk_sources(
                    self.disk_config.estimate,
                    runs,
                    stages,
                    estimate_time,
                )
            },
        );
        decision.status = Some(crate::application::claim_view::disk_status(
            &decision,
            run_views,
            moves.parallel,
            sources,
        ));
        env.queue.set_supervisor_disk_status(
            env.token,
            serde_json::to_value(decision.status.as_ref().expect("disk explanation"))?,
        )?;
        let needs = decision.needs;
        self.disk.needs = needs;
        self.disk.claims = decision.claims;
        self.disk.moving.store(
            moves.besides_the_landing(),
            std::sync::atomic::Ordering::SeqCst,
        );
        *self
            .disk
            .decision
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(decision.clone());
        self.moving_sizes.pass(
            &moving.iter().map(|r| r.target.clone()).collect::<Vec<_>>(),
            &self.measure_tree,
            &env.generators.clock,
            now,
        );
        let cleanup = self.disk.cleanup_epoch.read(in_flight);
        self.disk.cleanup_epoch = cleanup.next;
        if cleanup.discard_breakdown {
            self.disk.breakdown = None;
        }
        if cleanup.retry_now {
            self.disk.cleaned = None;
        }
        let most = Some(
            decision
                .landings
                .iter()
                .filter(|d| d.waiting)
                .fold(needs.claim, |n, d| n.max(d.needed)),
        );
        if decision.short {
            self.clean_for_disk(env, held, free, most, interval);
        }
        self.disk.short = decision.short;
        self.disk.cleaning = cleaning(decision.short, self.cleanup.for_disk());
        if self.disk.cleaning {
            info!("waiting for disk cleanup; {}", needs.message());
        }
        let waiting: Vec<RunId> = decision.blocked().map(|d| d.run.clone()).collect();
        let open = unclosed.iter().any(|ask| ask.is_open());
        let condition = disk_ask_condition(&ShortageReading {
            reserve: decision.reserve,
            cleaned: self.disk.cleanup_epoch.ready(
                self.disk.breakdown.is_some(),
                self.cleanup.for_disk(),
                self.cleanup.running(),
            ),
            in_flight,
            open,
            cleaning: self.disk.cleaning,
        });
        let step = disk_ask_step(
            condition,
            self.disk.asked,
            open,
            &self.disk.joined,
            &waiting,
        );
        if forgets_cleanup(&step, decision.short) {
            self.disk.cleaned = None;
        }
        match step {
            AskStep::Open(joining) => self.ask_for_disk(env, free, &needs, joining)?,
            AskStep::Nothing => {}
            AskStep::Close => {
                self.disk.asked = false;
                self.disk.joined.clear();
                for ask in env.queue.close_hold_asks(
                    AskReason::Cost,
                    Some(DISK_SUBJECT),
                    DISK_ENOUGH_CLOSED,
                )? {
                    info!(ask_id = %ask.id, "the free disk space is enough again: closed the disk ask {}", ask.id);
                }
            }
            AskStep::Forget => {}
        }
        if !self.disk.cleaning {
            let hold = decision.landing_hold();
            env.record_disk_landings(hold.as_ref(), &decision.landings)?;
        }
        Ok(())
    }
    /// A handoff found after this pass polled the cleanup and read the
    /// disk: the cleanup ends now, and the rest of a cleanup for room it
    /// started this pass counts as one for this pass's reading, so the
    /// drain's landings wait for it too (task 1426). A job that finished
    /// since the reading is not joined here but on the next pass, whose
    /// reading after it decides the landings.
    pub(in crate::application::supervise) fn end_cleanup_for_handoff(&mut self) {
        self.end_cleanup();
        self.disk.cleaning = cleaning(self.disk.short, self.cleanup.for_disk());
    }
    /// The free bytes of the queue's directory, where the run worktrees
    /// are; `None` when they cannot be read.
    pub(in crate::application::supervise) fn free_bytes(&self, env: &HostEnv<'_>) -> Option<u64> {
        (self.free_space)(&env.layout.runs_dir)
            .or_else(|| env.layout.db.parent().and_then(self.free_space))
    }
    /// The size of the volume of the queue's directory; `None` when it
    /// cannot be read, when the reserve is its least.
    fn total_bytes(&self, env: &HostEnv<'_>) -> Option<u64> {
        (self.total_space)(&env.layout.runs_dir)
            .or_else(|| env.layout.db.parent().and_then(self.total_space))
    }
    /// W and I under the configured method ([`stage_budgets`]); the recent
    /// samples and stage peaks are read again every [`SAMPLES_INTERVAL`]
    /// ([`crate::application::recent_run_samples`],
    /// [`crate::application::recent_stage_samples`]).
    fn expected_run_size(&mut self, env: &mut HostEnv<'_>, now: i64) -> Result<StageBudgets> {
        let fresh = self
            .disk
            .samples
            .as_ref()
            .is_some_and(|(at, _, _)| samples_fresh(env.generators.clock.since(*at)));
        if !fresh {
            let samples = crate::application::recent_run_samples(&*env.queue)?;
            let stages = match self.disk_config.estimate {
                crate::domain::disk::DiskEstimate::Recent => {
                    crate::application::recent_stage_samples(&*env.queue)?
                }
                crate::domain::disk::DiskEstimate::Fixed => StageSamples::default(),
            };
            self.disk.samples = Some((env.generators.clock.monotonic(), samples, stages));
        }
        Ok(self
            .disk
            .samples
            .as_ref()
            .map_or_else(StageBudgets::default, |(_, samples, stages)| {
                stage_budgets(&self.disk_config, samples, stages, now)
            }))
    }

    /// Record `run_stage_peak` for each WORKER or INTEGRATOR stage a
    /// registered run left since the last pass ([`StagePeaks`]), from this
    /// pass's trusted `sizes` of `moving`, whatever the method. A record
    /// that fails ends the pass with its error before the disk decision;
    /// that peak and the others left this pass are not tried again.
    fn record_stage_peaks(
        &mut self,
        env: &mut HostEnv<'_>,
        moving: &[DiskTarget],
        sizes: &[(RunId, Option<(u64, Instant)>)],
    ) -> Result<()> {
        let registry: Vec<_> = moving
            .iter()
            .zip(sizes)
            .map(|(r, (run, measure))| StageObservation {
                run: run.clone(),
                stage: r.usage.stage,
                measure: *measure,
            })
            .collect();
        for left in self.disk.stage_peaks.pass(&registry) {
            let task_id = env.queue.run(&left.run)?.task_id();
            env.queue.record_runtime_event(
                &left.run,
                EventKind::RunStagePeak,
                serde_json::json!({
                    "run_id": left.run,
                    "task_id": task_id,
                    "phase": left.stage,
                    "peak_bytes": left.peak_bytes,
                    "measures": left.measures,
                    "estimate": self.disk_config.estimate,
                }),
            )?;
        }
        Ok(())
    }
    /// The relaxation of the needs in effect this pass (ADR-t4727-1), from
    /// the newest of its records alone ([`crate::domain::disk::relax_state`]):
    /// a person's `disk relax` takes effect on the next pass, with no
    /// restart, and one past its time is recorded `disk_relax_expired` by
    /// the first pass that finds it, which reckons as with none. The queue
    /// judges the end again in its write's own transaction, at its own
    /// clock ([`crate::application::EventStore::expire_disk_relax`]): a
    /// relaxation recorded since, or the end another supervisor recorded
    /// first, leaves it unwritten. No other operation of the supervisor
    /// writes the relaxation's records ([`crate::domain::disk::RELAX_KINDS`]).
    fn relaxation(&self, env: &mut HostEnv<'_>) -> Result<Option<DiskRelaxed>> {
        let kinds = RELAX_KINDS.map(EventKind::as_str);
        let latest = env.queue.latest_queue_event(&kinds)?;
        let now = crate::application::unix_millis(env.generators.clock.system_time());
        let state = relax_state(latest.as_ref(), now);
        if let RelaxState::Expired(relaxed) = state {
            match env
                .queue
                .expire_disk_relax(crate::domain::EventId::new(relaxed.event_id))
            {
                Ok(None) => {}
                Ok(Some(_)) => info!(
                    "the relaxation of the disk's needs (event {}) reached its time: the needs are reckoned as before it",
                    relaxed.event_id
                ),
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the end of the disk's relaxation could not be recorded: {error:#}")
                }
            }
        }
        Ok(state.in_effect())
    }
    /// Ask for what the ended runs left to be cleaned for room, when
    /// [`asks_for_cleanup`] says so; the job does it off the loop, and
    /// [`Self::cleaned_for_disk`] follows.
    fn clean_for_disk(
        &mut self,
        env: &mut HostEnv<'_>,
        held: &super::cleanup::Held,
        free: Option<u64>,
        needed: Option<u64>,
        interval: Duration,
    ) {
        if !asks_for_cleanup(
            self.disk.cleaned.map(|at| env.generators.clock.since(at)),
            self.cleanup.for_disk(),
            interval,
        ) {
            return;
        }
        // A request a drain does not take is not a cleanup: the next one
        // after the drain ends is not kept waiting for it (task 1427).
        if self.request_cleanup(env, held, None, Some(DiskRequest { free, needed })) {
            self.disk.cleaned = Some(env.generators.clock.monotonic());
            self.disk.breakdown = None;
        }
    }
    /// Once a cleanup for room is done: record `auto_repaired` when it
    /// freed something, with the free bytes read again. Return that same
    /// reading so a counted remainder need not spend the old shortage.
    pub(in crate::application::supervise) fn cleaned_for_disk(
        &mut self,
        env: &mut HostEnv<'_>,
        request: DiskRequest,
        removed: &Cleaned,
    ) -> Option<u64> {
        if removed.bytes == 0 {
            return None;
        }
        let after = self.free_bytes(env);
        info!(
            "the free disk space was short: removed {} bytes of what {} ended run(s) left and {} build cache(s)",
            removed.bytes,
            removed.runs.len(),
            removed.caches.len()
        );
        if let Err(error) = env.queue.record_queue_event(
            EventKind::AutoRepaired,
            disk_cleanup_record(request, removed, after, env.token),
        ) {
            warn!(error = %format_args!("{error:#}"), "the cleanup for disk space could not be recorded: {error:#}");
        }
        after
    }
    /// Open the disk ask of this shortage, or add to the open one the
    /// runs whose landing waits for the disk (`joining`, from
    /// [`disk_ask_step`]; `None` opens it with no run).
    fn ask_for_disk(
        &mut self,
        env: &mut HostEnv<'_>,
        free: Option<u64>,
        needs: &DiskNeeds,
        joining: Vec<Option<RunId>>,
    ) -> Result<()> {
        let question = disk_question(
            free,
            needs,
            self.disk.breakdown.as_deref().unwrap_or_default(),
        );
        for run in joining {
            let (outcome, _) = ask::hold(
                &mut *env.queue,
                NewHold {
                    reason_category: AskReason::Cost,
                    subject: Some(DISK_SUBJECT.into()),
                    run_id: run.clone(),
                    job: None,
                    question: question.clone(),
                    options: DISK_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                    asked_by: SessionRole::Supervisor.as_str().into(),
                },
            )?;
            if outcome.created {
                warn!(ask_id = %outcome.ask.id, "the free disk space stays short after the cleanup: opened the disk ask {}", outcome.ask.id);
            }
            if let Some(run) = run {
                self.disk.joined.push(run);
            }
        }
        self.disk.asked = true;
        Ok(())
    }
    /// Close the answered disk asks: `done` (a person freed the disk)
    /// cleans and checks again at once and may ask again; `wait` leaves
    /// the queue waiting without asking again.
    fn apply_disk_answers(&mut self, env: &mut HostEnv<'_>, unclosed: &[Ask]) -> Result<()> {
        for ask in unclosed.iter().filter(|ask| ask.answered_at.is_some()) {
            let answer = ask.answer.as_deref().unwrap_or_default().trim().to_owned();
            env.queue.close_ask(ask.id)?;
            info!(ask_id = %ask.id, "applied the answer {answer:?} of the disk ask {}", ask.id);
            if asks_again_on(&answer) {
                self.disk.asked = false;
                self.disk.cleaned = None;
                self.disk.breakdown = None;
                self.disk.joined.clear();
            }
        }
        Ok(())
    }
}

/// Cleanup is a scheduling wait on the already computed pass result.
const fn cleaning(short: bool, for_disk: bool) -> bool {
    short && for_disk
}

/// The payload of `auto_repaired` (`repair: disk_cleanup`) for a cleanup
/// for room: the reading it was asked for at, the reading `after` it, and
/// the bytes and runs it removed, and the build caches it cleared when it
/// cleared any.
fn disk_cleanup_record(
    request: DiskRequest,
    removed: &Cleaned,
    after: Option<u64>,
    token: &LeaseToken,
) -> Value {
    let mut record = json!({
        "repair": DISK_CLEANUP,
        "layer": "runtime",
        "conditions": {
            "free_bytes": request.free,
            "needed_bytes": request.needed,
            "free_bytes_after": after,
        },
        "detail": {"bytes": removed.bytes, "runs": removed.runs},
        "bytes": removed.bytes,
        "supervisor": token,
    });
    if !removed.caches.is_empty() {
        record["detail"]["caches"] = json!(removed.caches);
    }
    record
}

/// Whether a pass short of room asks for a cleanup for room (task 1627):
/// not while one waits or runs (`for_disk`, the rest of one another job
/// took on included), and not until `interval` passed since the end of the
/// last one (`since_cleaned`; since it was asked for, until it ends). An
/// ordinary job running is no cleanup for room: the first request is taken
/// on by it, and its rest follows (task 1478). So after a cleanup for room
/// ends, the next pass judges the reading after it instead of asking for
/// another, and opens the disk ask when still short.
pub(in crate::application::supervise) fn asks_for_cleanup(
    since_cleaned: Option<Duration>,
    for_disk: bool,
    interval: Duration,
) -> bool {
    !for_disk && since_cleaned.is_none_or(|since| since >= interval)
}

/// What a judged pass's reading says of the disk ask
/// ([`disk_ask_condition`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::application::supervise) enum AskCondition {
    /// Ask: open the disk ask, or add the waiting landings to it.
    Ask,
    /// Short with no ask yet that may not open one, or above the
    /// threshold short of the margin with one: neither open nor close.
    Keep,
    /// Room: close the disk ask, or forget the shortage.
    Room,
}

/// A judged pass's reading for [`disk_ask_condition`].
#[derive(Debug, Clone, Copy)]
pub(in crate::application::supervise) struct ShortageReading {
    pub(super) reserve: crate::domain::disk::ReserveDecision,
    /// A cleanup for room started and finished in the current idle epoch,
    /// and no cleanup job is pending or running.
    pub(super) cleaned: bool,
    pub(super) in_flight: usize,
    pub(super) open: bool,
    /// A cleanup for room still holds claims and landings; even an
    /// existing ask waits before adding those landings, as before.
    pub(super) cleaning: bool,
}

/// Open only below the reserve with no moving run after cleanup. Keep an
/// open ask across the reserve until the close margin, preventing churn.
/// An answered `wait` forgets its shortage at the reserve itself. Waiting
/// landings may still join an existing ask while runs move, once no
/// cleanup for room holds them.
pub(in crate::application::supervise) fn disk_ask_condition(
    reading: &ShortageReading,
) -> AskCondition {
    let short = reading.reserve.below;
    if reading.open && reading.reserve.recovered {
        return AskCondition::Room;
    }
    if !reading.open && !short {
        return AskCondition::Room;
    }
    if short && !reading.cleaning && (reading.open || (reading.in_flight == 0 && reading.cleaned)) {
        AskCondition::Ask
    } else {
        AskCondition::Keep
    }
}

/// What a judged pass does with the disk ask ([`disk_ask_step`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::application::supervise) enum AskStep {
    /// Open the disk ask, once per entry: `None` with no run, a run to
    /// add the run whose landing waits to it.
    Open(Vec<Option<RunId>>),
    /// Short, but the ask of this shortage was answered `wait`, or the
    /// condition keeps ([`AskCondition::Keep`]): nothing until there is
    /// room.
    Nothing,
    /// There is room again: close the disk ask and forget this shortage.
    Close,
    /// There is room above the reserve and nothing was asked.
    Forget,
}

/// What a judged pass does with the disk ask (ADR-0047 decision 44):
/// asking ([`AskCondition::Ask`]), the ask of this shortage is opened once
/// (with no run when no landing waits, else with each `waiting` run) and
/// the waiting runs not `joined` yet are added while it is `open`; once
/// `asked` and no longer open (answered `wait`), nothing is asked until
/// there is room. With room, the ask is closed when one is open or was
/// `asked`; kept, nothing changes.
pub(in crate::application::supervise) fn disk_ask_step(
    condition: AskCondition,
    asked: bool,
    open: bool,
    joined: &[RunId],
    waiting: &[RunId],
) -> AskStep {
    let ask = match condition {
        AskCondition::Ask => true,
        AskCondition::Keep => return AskStep::Nothing,
        AskCondition::Room => false,
    };
    match (ask, asked) {
        (true, true) if !open => AskStep::Nothing,
        (true, true) => AskStep::Open(
            waiting
                .iter()
                .filter(|run| !joined.contains(run))
                .cloned()
                .map(Some)
                .collect(),
        ),
        (true, false) if waiting.is_empty() => AskStep::Open(vec![None]),
        (true, false) => AskStep::Open(waiting.iter().cloned().map(Some).collect()),
        (false, _) if open || asked => AskStep::Close,
        (false, _) => AskStep::Forget,
    }
}

/// Room for the ask alone must not restart cleanup pass after pass while
/// the disk is still short of what a claim or landing needs.
fn forgets_cleanup(step: &AskStep, short: bool) -> bool {
    !short && matches!(step, AskStep::Close | AskStep::Forget)
}

/// Whether the answer of a disk ask asks again (a person freed the disk:
/// `done`), cleaning and checking at once; `wait` and any other answer
/// leave the queue waiting without asking again.
pub(in crate::application::supervise) fn asks_again_on(answer: &str) -> bool {
    answer.trim() == "done"
}

/// The question of the disk ask, with the free bytes and what a claim and
/// a landing need (`unknown` when not read). Append at most five directory
/// paths in descending size order, with sizes in GiB, or `内訳なし` if
/// none could be measured.
pub(in crate::application::supervise) fn disk_question(
    free: Option<u64>,
    needs: &DiskNeeds,
    breakdown: &[(std::path::PathBuf, u64)],
) -> String {
    let show = |bytes: Option<u64>| bytes.map_or_else(|| "unknown".into(), |b| gib(b as f64));
    let expected = needs
        .expected
        .map_or_else(|| "none yet".into(), |bytes| gib(bytes as f64));
    let source = match needs.estimate {
        crate::domain::disk::DiskEstimate::Fixed => {
            "from the [disk] configuration values in dagq.toml"
        }
        crate::domain::disk::DiskEstimate::Recent => "from the sizes of the recent runs",
    };
    let question = DISK_QUESTION
        .replace("{free}", &show(free))
        .replace("{claim}", &show(Some(needs.claim)))
        .replace("{landing}", &show(Some(needs.landing)))
        .replace("{reserve}", &show(Some(needs.reserve)))
        .replace("{expected}", &expected)
        .replace("{source}", source)
        .replace(
            "{least}",
            &gib(crate::domain::disk::MIN_RESERVE_BYTES as f64),
        );
    let mut sizes = breakdown.to_vec();
    sizes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let details = if sizes.is_empty() {
        "内訳なし".to_owned()
    } else {
        sizes
            .iter()
            .take(5)
            .map(|(path, bytes)| format!("{}: {}", path.display(), gib(*bytes as f64)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "{question}\n{}\n\nLargest directories after cleanup:\n{details}",
        needs.message()
    )
}

/// The queue's `cost` ask about the disk.
fn is_disk_ask(ask: &Ask) -> bool {
    ask.kind == AskKind::QueueHold
        && ask.reason_category == AskReason::Cost
        && ask.subject.as_deref() == Some(DISK_SUBJECT)
}

/// What a cleanup ([`HostOpsState::request_cleanup`]) removed.
#[derive(Debug, Default)]
pub(in crate::application::supervise) struct Cleaned {
    pub(super) bytes: u64,
    pub(super) runs: Vec<RunId>,
    /// The build caches of the queue's directory it cleared, by path.
    pub(super) caches: Vec<String>,
}

impl Cleaned {
    pub(in crate::application::supervise) fn add(&mut self, run: &RunId, bytes: u64) {
        if bytes > 0 {
            self.bytes += bytes;
            if !self.runs.contains(run) {
                self.runs.push(run.clone());
            }
        }
    }
    /// A build cache of no run cleared at `path`.
    pub(in crate::application::supervise) fn add_cache(&mut self, path: &str, bytes: u64) {
        if bytes > 0 {
            self.bytes += bytes;
            self.caches.push(path.to_owned());
        }
    }
}

/// Whether the samples read `since` ago are used still: until
/// [`SAMPLES_INTERVAL`], at which they are read again.
fn samples_fresh(since: Duration) -> bool {
    since < SAMPLES_INTERVAL
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raw measurements, not precomputed reservations or remainders, are
    /// the input to the same pure derivation check_disk calls.
    #[test]
    fn run_details_derive_reservations_and_unused_bytes() {
        use crate::application::claim_view::{RunDiskInput, run_disk_status};
        const G: u64 = 1 << 30;
        let needs = DiskState {
            free: Some(60 * G),
            total: 0,
            expected: Some(20 * G),
            integrator_expected: Some(30 * G),
            runs: vec![
                DiskRun {
                    stage: DiskStage::Worker,
                    current: Some(12 * G),
                },
                DiskRun {
                    stage: DiskStage::Worker,
                    current: Some(5 * G),
                },
                DiskRun {
                    stage: DiskStage::Waiting,
                    current: Some(18 * G),
                },
            ],
            moving: 2,
            landing_current: None,
            landing_waiting: false,
            relaxed: None,
        }
        .needs(0.05);
        let cases = [
            (
                DiskStage::Worker,
                Some(12),
                false,
                Some(20),
                8,
                "WORKER",
                "worktree+tmp",
            ),
            (
                DiskStage::Worker,
                Some(5),
                false,
                Some(20),
                15,
                "WORKER",
                "worktree+tmp",
            ),
            (
                DiskStage::Waiting,
                Some(18),
                false,
                Some(18),
                0,
                "landing_answer",
                "build",
            ),
            (
                DiskStage::Worker,
                None,
                false,
                Some(20),
                20,
                "WORKER",
                "worktree+tmp",
            ),
            (
                DiskStage::Worker,
                Some(12),
                true,
                Some(20),
                20,
                "WORKER",
                "worktree+tmp",
            ),
            (
                DiskStage::Worker,
                Some(25),
                false,
                Some(20),
                0,
                "WORKER",
                "worktree+tmp",
            ),
            (
                DiskStage::Worker,
                Some(0),
                false,
                Some(20),
                20,
                "WORKER",
                "worktree+tmp",
            ),
            (
                DiskStage::Integrator,
                Some(18),
                false,
                Some(30),
                12,
                "INTEGRATOR",
                "worktree+tmp",
            ),
            (
                DiskStage::Integrator,
                None,
                false,
                Some(30),
                30,
                "INTEGRATOR",
                "worktree+tmp",
            ),
            (
                DiskStage::Integrator,
                Some(18),
                true,
                Some(30),
                30,
                "INTEGRATOR",
                "worktree+tmp",
            ),
            (
                DiskStage::Waiting,
                None,
                false,
                None,
                0,
                "landing_answer",
                "build",
            ),
            (
                DiskStage::Waiting,
                Some(18),
                true,
                Some(18),
                0,
                "landing_answer",
                "build",
            ),
        ];
        let mut unused = Vec::new();
        for (stage, bytes, stale, reserved, remaining, label, target) in cases {
            let current = bytes.map(|bytes| bytes * G);
            let run = run_disk_status(
                &needs,
                RunDiskInput {
                    run: RunId::new("measured").unwrap(),
                    task: TaskId::new(7),
                    stage,
                    phase: "landing_answer".into(),
                    container: false,
                    measurement: (current, Some(1234), stale),
                },
            );
            assert_eq!(run.reservation, reserved.map(|bytes| bytes * G), "{run:?}");
            assert_eq!(run.unused, remaining * G, "{run:?}");
            assert_eq!(run.current, current);
            assert_eq!(run.stale, stale);
            assert_eq!(run.measured_at_ms, Some(1234));
            assert_eq!(run.stage, label);
            assert_eq!(run.target, target);
            unused.push(run.unused);
        }
        assert_eq!(unused[..3], [8 * G, 15 * G, 0]);
        assert_eq!(unused[..3].iter().sum::<u64>(), 23 * G);
        assert_eq!(unused[..3].iter().sum::<u64>(), needs.remaining);
    }

    #[test]
    fn container_run_details_discard_host_measurements() {
        use crate::application::claim_view::{RunDiskInput, disk_status, run_disk_status};
        const G: u64 = 1 << 30;
        for (stage, budget, target) in [
            (DiskStage::Worker, Some(20 * G), "worktree+tmp"),
            (DiskStage::Integrator, Some(30 * G), "worktree+tmp"),
            (DiskStage::Waiting, None, "build"),
        ] {
            let decision = DiskState {
                free: Some(100 * G),
                total: 0,
                expected: Some(20 * G),
                integrator_expected: Some(30 * G),
                runs: vec![DiskRun {
                    stage,
                    current: None,
                }],
                moving: 1,
                landing_current: None,
                landing_waiting: false,
                relaxed: None,
            }
            .decide(0.05, &[], crate::domain::disk::DiskEstimate::Fixed, 0);
            // Even an apparent host reading must be discarded for a container.
            let run = run_disk_status(
                &decision.needs,
                RunDiskInput {
                    run: RunId::new("container").unwrap(),
                    task: TaskId::new(8),
                    stage,
                    phase: "landing_answer".into(),
                    container: true,
                    measurement: (Some(99 * G), Some(1234), true),
                },
            );
            assert_eq!(run.current, None);
            assert_eq!(run.measured_at_ms, None);
            assert!(!run.stale);
            assert!(run.container);
            assert_eq!(run.target, target);
            assert_eq!(run.reservation, budget);
            assert_eq!(run.unused, budget.unwrap_or(0));
            assert_eq!(run.unused, decision.needs.remaining);
            let shown = disk_status(&decision, vec![run], 3, ["configuration"; 2]);
            assert!(shown.message.contains("unmeasured (container)"));
            assert!(!shown.message.contains("99.0 GiB"));
            assert!(!shown.message.contains("at 1234"));
        }
    }

    /// A landing's retry counts the runs moving besides the landing under
    /// way: with the landing alone in flight, none, as at its start.
    #[test]
    fn a_landings_retry_counts_the_runs_moving_besides_it() {
        let moves = |in_flight, landing| DiskMoves {
            parallel: 3,
            in_flight,
            landing,
            landing_waiting: false,
        };
        assert_eq!(moves(1, 1).besides_the_landing(), 0);
        assert_eq!(moves(3, 1).besides_the_landing(), 2);
        assert_eq!(moves(2, 0).besides_the_landing(), 2);
        assert_eq!(moves(0, 0).besides_the_landing(), 0);
    }

    /// The samples are kept until 1 ms before [`SAMPLES_INTERVAL`] and
    /// read again at it.
    #[test]
    fn the_disk_samples_are_read_again_at_their_interval() {
        let ms = Duration::from_millis(1);
        assert!(samples_fresh(Duration::ZERO));
        assert!(samples_fresh(SAMPLES_INTERVAL - ms));
        assert!(!samples_fresh(SAMPLES_INTERVAL));
    }

    const GIB: u64 = 1 << 30;

    /// A claim needs far more than a landing (a measured build), as the
    /// cases below read.
    const NEEDS: DiskNeeds = DiskNeeds {
        reserve: GIB,
        expected: Some(GIB),
        claim: 100 * GIB,
        landing: GIB,
        claims_wait: false,
        volume: 0,
        reserve_share: 0.05,
        sample_count: 0,
        estimate: crate::domain::disk::DiskEstimate::Recent,
        moving: 0,
        workers: 0,
        waiting: 0,
        integrators: 0,
        integrator_expected: None,
        remaining: 0,
        measured: 0,
        unmeasured: 0,
        landing_waiting: false,
        claim_expected: 0,
        claim_remaining: 0,
        landing_expected: 0,
        claims_wait_reason: None,
        relaxed: None,
    };

    /// `auto_repaired` of a cleanup for room: the reading it was asked
    /// at, the one after, and the bytes and runs it removed (each run
    /// once, whatever it removed).
    #[test]
    fn a_cleanup_for_room_records_what_it_removed_and_the_readings() {
        let run = RunId::new("idle").unwrap();
        let mut removed = Cleaned::default();
        removed.add(&run, 100);
        removed.add(&run, 28);
        let payload = disk_cleanup_record(
            DiskRequest {
                free: Some(1),
                needed: Some(GIB),
            },
            &removed,
            Some(4 * GIB),
            &LeaseToken::new("t"),
        );
        assert_eq!(
            payload,
            json!({
                "repair": "disk_cleanup",
                "layer": "runtime",
                "conditions": {"free_bytes": 1, "needed_bytes": GIB, "free_bytes_after": 4 * GIB},
                "detail": {"bytes": 128, "runs": ["idle"]},
                "bytes": 128,
                "supervisor": "t",
            })
        );
        // A cleared build cache adds its bytes and is named; one that took
        // nothing is not.
        removed.add_cache("/q/update/target", 1000);
        removed.add_cache("/q/recheck/target", 0);
        let payload = disk_cleanup_record(
            DiskRequest {
                free: Some(1),
                needed: Some(GIB),
            },
            &removed,
            None,
            &LeaseToken::new("t"),
        );
        assert_eq!(payload["bytes"], 1128);
        assert_eq!(
            payload["detail"],
            json!({"bytes": 1128, "runs": ["idle"], "caches": ["/q/update/target"]})
        );
    }

    /// Task 1627: no cleanup for room is asked for while one waits or runs,
    /// whatever the time; with none, the first is asked for at once and the
    /// next once the interval passed since the last ended.
    #[test]
    fn a_cleanup_for_room_is_asked_for_only_with_none_in_flight_and_the_interval_passed() {
        let interval = Duration::from_secs(60);
        for (since, for_disk, asks) in [
            (None, false, true),
            (None, true, false),
            (Some(Duration::ZERO), false, false),
            (Some(Duration::from_secs(59)), false, false),
            (Some(Duration::from_secs(60)), false, true),
            (Some(Duration::from_secs(600)), false, true),
            (Some(Duration::from_secs(600)), true, false),
        ] {
            assert_eq!(
                asks_for_cleanup(since, for_disk, interval),
                asks,
                "{since:?} {for_disk}"
            );
        }
    }

    /// The disk ask is opened once per shortage, with no run when no
    /// landing waits and with each waiting landing otherwise; while it is
    /// open the landings that wait since are added once; answered `wait`
    /// (closed, still asked) nothing more is asked; with room it is closed
    /// only when one is open or was asked.
    #[test]
    fn the_disk_ask_is_opened_once_joined_by_the_waiting_landings_and_closed_with_room() {
        let (a, b) = (RunId::new("a").unwrap(), RunId::new("b").unwrap());
        // Short, not asked yet: opened with no run, or with each landing.
        assert_eq!(
            disk_ask_step(AskCondition::Ask, false, false, &[], &[]),
            AskStep::Open(vec![None])
        );
        assert_eq!(
            disk_ask_step(
                AskCondition::Ask,
                false,
                false,
                &[],
                &[a.clone(), b.clone()]
            ),
            AskStep::Open(vec![Some(a.clone()), Some(b.clone())])
        );
        // Asked and open: told once, only a new landing joins.
        assert_eq!(
            disk_ask_step(AskCondition::Ask, true, true, &[], &[]),
            AskStep::Open(vec![])
        );
        assert_eq!(
            disk_ask_step(
                AskCondition::Ask,
                true,
                true,
                std::slice::from_ref(&a),
                &[a.clone(), b.clone()]
            ),
            AskStep::Open(vec![Some(b.clone())])
        );
        // Answered `wait`: asked, closed, still short.
        assert_eq!(
            disk_ask_step(
                AskCondition::Ask,
                true,
                false,
                std::slice::from_ref(&a),
                std::slice::from_ref(&b)
            ),
            AskStep::Nothing
        );
        // Room: closed when asked or open, else only forgotten.
        assert_eq!(
            disk_ask_step(AskCondition::Room, true, false, &[], &[]),
            AskStep::Close
        );
        assert_eq!(
            disk_ask_step(AskCondition::Room, false, true, &[], &[]),
            AskStep::Close
        );
        assert_eq!(
            disk_ask_step(AskCondition::Room, false, false, &[], &[a]),
            AskStep::Forget
        );
    }

    fn condition(free: u64, cleaned: bool, in_flight: usize, asked: bool) -> AskCondition {
        reading(free, cleaned, in_flight, asked)
    }

    fn reading(free: u64, cleaned: bool, in_flight: usize, open: bool) -> AskCondition {
        disk_ask_condition(&ShortageReading {
            reserve: crate::domain::disk::reserve_decision(Some(free), Some(100 * GIB)),
            cleaned,
            in_flight,
            open,
            cleaning: false,
        })
    }

    #[test]
    fn only_idle_cleanup_jobs_for_room_measure_a_breakdown() {
        for (moving, for_disk, expected) in [
            (0, true, true),
            (0, false, false),
            (1, true, false),
            (1, false, false),
        ] {
            let epoch = CleanupEpoch::default().read(moving).next;
            assert_eq!(epoch.measures_breakdown(for_disk), expected);
        }
    }

    #[test]
    fn a_cleanup_must_follow_the_transition_to_idle() {
        let initial = CleanupEpoch::default();
        let moving = initial.read(1);
        assert!(moving.discard_breakdown);
        assert!(!moving.retry_now);
        let started_while_moving = moving.next.generation;
        let done_while_moving = moving.next.completed(started_while_moving);
        assert!(!done_while_moving.ready);
        assert!(done_while_moving.cooldown);
        let idle = moving.next.read(0);
        assert!(idle.discard_breakdown);
        assert!(idle.retry_now);
        // A completed cleanup's cooldown cannot block the first idle job.
        assert!(asks_for_cleanup(None, false, CLEANUP_INTERVAL));
        // Nor may that earlier job finishing now certify the idle disk.
        let stale = idle.next.completed(started_while_moving);
        assert!(!stale.ready);
        assert!(!stale.cooldown);
        assert_eq!(
            condition(
                99 * GIB,
                idle.next.ready(stale.ready, false, false),
                0,
                false
            ),
            AskCondition::Keep
        );
        assert!(!idle.next.completed(initial.generation).ready);
        let fresh = idle.next.completed(idle.next.generation);
        assert!(fresh.ready);
        assert!(fresh.cooldown);
        assert_eq!(
            condition(
                99 * GIB,
                idle.next.ready(fresh.ready, false, false),
                0,
                false
            ),
            AskCondition::Ask
        );
        assert!(!idle.next.read(0).discard_breakdown);
        assert!(!idle.next.read(0).retry_now);
    }

    #[test]
    fn only_a_finished_idle_cleanup_with_no_job_pending_is_ready() {
        for (moving, measured, for_disk, running, expected) in [
            (0, false, false, false, AskCondition::Keep),
            (0, true, true, false, AskCondition::Keep), // pending job
            (0, true, true, true, AskCondition::Keep),  // cleanup for room
            (0, true, false, true, AskCondition::Keep), // ordinary cleanup
            (1, true, false, false, AskCondition::Keep),
            (0, true, false, false, AskCondition::Ask),
        ] {
            let epoch = CleanupEpoch::default().read(moving).next;
            assert_eq!(
                condition(
                    99 * GIB,
                    epoch.ready(measured, for_disk, running),
                    moving,
                    false
                ),
                expected
            );
        }
    }

    #[test]
    fn room_for_the_ask_keeps_the_cleanup_cooldown_while_claims_are_short() {
        for (step, cleared_with_room) in [
            (AskStep::Close, true),
            (AskStep::Forget, true),
            (AskStep::Nothing, false),
            (AskStep::Open(vec![None]), false),
        ] {
            assert!(!forgets_cleanup(&step, true));
            assert_eq!(forgets_cleanup(&step, false), cleared_with_room);
        }
    }

    #[test]
    fn an_open_ask_accepts_waiting_landings_while_runs_move() {
        let a = RunId::new("a").unwrap();
        let b = RunId::new("b").unwrap();
        let condition = condition(99 * GIB, false, 1, true);
        assert_eq!(condition, AskCondition::Ask);
        let mut cleaning = ShortageReading {
            reserve: crate::domain::disk::reserve_decision(Some(99 * GIB), Some(100 * GIB)),
            cleaned: false,
            in_flight: 1,
            open: true,
            cleaning: true,
        };
        assert_eq!(disk_ask_condition(&cleaning), AskCondition::Keep);
        cleaning.reserve = crate::domain::disk::reserve_decision(Some(105 * GIB), Some(100 * GIB));
        assert_eq!(disk_ask_condition(&cleaning), AskCondition::Room);
        assert_eq!(
            disk_ask_step(
                condition,
                true,
                true,
                std::slice::from_ref(&a),
                &[a.clone(), b.clone()]
            ),
            AskStep::Open(vec![Some(b)])
        );
    }

    #[test]
    fn asks_only_below_reserve_after_cleanup_with_no_moving_runs() {
        use AskCondition::{Ask, Keep, Room};
        for (free, cleaned, moving, expected) in [
            (99 * GIB, false, 0, Keep),
            (1, false, 0, Keep),
            (99 * GIB, true, 0, Ask),
            (1, true, 1, Keep),
            (100 * GIB, true, 0, Room),
            (101 * GIB, true, 2, Room),
        ] {
            assert_eq!(condition(free, cleaned, moving, false), expected);
        }
    }

    #[test]
    fn an_open_ask_keeps_until_five_percent_above_reserve() {
        use AskCondition::{Ask, Keep, Room};
        for (free, expected) in [(99, Ask), (100, Keep), (104, Keep), (105, Room)] {
            assert_eq!(condition(free * GIB, true, 0, true), expected);
        }
        assert_eq!(condition(105 * GIB - 1, true, 0, true), Keep);
        let mut asked = false;
        let mut opened = 0;
        for free in [99, 101, 99, 103, 98, 104, 99] {
            match disk_ask_step(
                condition(free * GIB, true, 0, asked),
                asked,
                asked,
                &[],
                &[],
            ) {
                AskStep::Open(_) if !asked => {
                    opened += 1;
                    asked = true;
                }
                AskStep::Open(joining) => assert!(joining.is_empty()),
                AskStep::Nothing => {}
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(opened, 1);
        assert_eq!(condition(99 * GIB, true, 1, true), Ask);
        for (free, need) in [(None, Some(GIB)), (Some(1), None)] {
            assert_eq!(
                disk_ask_condition(&ShortageReading {
                    reserve: crate::domain::disk::reserve_decision(free, need),
                    cleaned: true,
                    in_flight: 0,
                    open: true,
                    cleaning: false,
                }),
                Room
            );
        }
    }

    /// An ask answered `wait` (asked, closed) asks nothing more while
    /// short, whatever the depth, and the threshold itself ends that
    /// shortage, as without the margin: a shortage after it asks again
    /// (no run in flight, deep).
    #[test]
    fn a_wait_answer_holds_until_the_threshold_and_a_later_shortage_asks_again() {
        use AskCondition::{Ask, Room};
        // Short, answered `wait`: asked, so nothing is asked.
        for free in [99 * GIB, 89 * GIB, 1] {
            assert_eq!(reading(free, true, 0, false), Ask);
            assert_eq!(
                disk_ask_step(reading(free, true, 0, false), true, false, &[], &[]),
                AskStep::Nothing
            );
        }
        // Back at the threshold, within the close margin: the shortage
        // ends (the watch forgets it asked).
        assert_eq!(reading(100 * GIB, false, 0, false), Room);
        assert_eq!(
            disk_ask_step(reading(101 * GIB, false, 0, false), true, false, &[], &[]),
            AskStep::Close
        );
        // Short again, deep: a new ask opens.
        assert_eq!(
            disk_ask_step(reading(89 * GIB, true, 0, false), false, false, &[], &[]),
            AskStep::Open(vec![None])
        );
    }

    /// `done` asks again after another cleanup (the watch forgets it
    /// asked, so the next short pass opens a new ask); `wait` does not.
    #[test]
    fn a_done_answer_asks_again_and_a_wait_does_not() {
        assert!(asks_again_on("done"));
        assert!(asks_again_on(" done\n"));
        assert!(!asks_again_on("wait"));
        assert!(!asks_again_on(""));
        // After `done` the watch is not asked: a short pass opens anew.
        assert_eq!(
            disk_ask_step(AskCondition::Ask, false, false, &[], &[]),
            AskStep::Open(vec![None])
        );
        // and a cleanup for room is asked for at once.
        assert!(asks_for_cleanup(None, false, CLEANUP_INTERVAL));
    }

    /// The method carried from the disk decision determines the source named
    /// in the ask, including when both methods yield the same size.
    #[test]
    fn the_question_names_the_configured_estimation_source() {
        use crate::domain::disk::DiskEstimate;
        for (estimate, source, absent) in [
            (
                DiskEstimate::Fixed,
                "from the [disk] configuration values in dagq.toml",
                "from the sizes of the recent runs",
            ),
            (
                DiskEstimate::Recent,
                "from the sizes of the recent runs",
                "from the [disk] configuration values in dagq.toml",
            ),
        ] {
            let question = disk_question(Some(GIB), &DiskNeeds { estimate, ..NEEDS }, &[]);
            assert!(
                question.contains(&format!("expected to take, 1.0 GiB ({source})")),
                "{question}"
            );
            assert!(!question.contains(absent), "{question}");
        }
    }

    #[test]
    fn the_question_shows_the_top_five_in_gib_or_no_breakdown() {
        let sizes: Vec<_> = (1..=7)
            .map(|n| (std::path::PathBuf::from(format!("/q/{n}")), n * GIB))
            .collect();
        let question = disk_question(Some(1), &NEEDS, &sizes);
        assert!(question.ends_with(
            "/q/7: 7.0 GiB\n/q/6: 6.0 GiB\n/q/5: 5.0 GiB\n/q/4: 4.0 GiB\n/q/3: 3.0 GiB"
        ));
        assert!(!question.contains("/q/2"));
        assert!(disk_question(Some(1), &NEEDS, &[]).ends_with("内訳なし"));
    }

    /// The disk ask's question names the free bytes, what a claim and a
    /// landing need and what they are made of (the reserve and what a run
    /// is expected to take, with no factor), and the scratchpads among
    /// what was cleaned (task 1100); free bytes not read are `unknown`, no
    /// expectation `none yet`.
    #[test]
    fn the_disk_question_names_the_reading_the_needs_and_what_they_are_made_of() {
        let question = disk_question(
            Some(GIB / 2),
            &DiskNeeds {
                reserve: 5 * GIB,
                expected: Some(8 * GIB),
                claim: 13 * GIB,
                landing: 5 * GIB,
                claims_wait: false,
                ..DiskNeeds::default()
            },
            &[],
        );
        assert!(question.contains("0.5 GiB free"), "{question}");
        assert!(
            question.contains("a new run needs 13.0 GiB and a landing's verification 5.0 GiB"),
            "{question}"
        );
        assert!(
            question.contains(
                "the reserve of 5.0 GiB, [disk] reserve of dagq.toml as a share of the volume and at least 5.0 GiB,"
            ),
            "{question}"
        );
        assert!(
            question.contains(
                "what a run is expected to take, 8.0 GiB (from the sizes of the recent runs),"
            ),
            "{question}"
        );
        assert!(
            question.contains(
                "the worktrees, the Claude Code scratchpads and the run TMPDIRs of completed and canceled tasks' runs"
            ),
            "{question}"
        );
        assert!(
            question.contains("the target directories of the automatic update and the landing recheck when neither runs"),
            "{question}"
        );
        assert!(!question.contains("factor"), "{question}");
        assert!(!question.contains("Affected:"));
        let unread = disk_question(None, &DiskNeeds::default(), &[]);
        assert!(unread.contains("unknown free"), "{unread}");
        assert!(
            unread.contains("expected to take, none yet (from the sizes of the recent runs),"),
            "{unread}"
        );
    }
}
