//! The free disk space a claim and a landing need (ADR-0047 decision 44,
//! ADR-t4473-1): on every pass the supervisor reads the free and total
//! bytes of the queue's directory and, from the recent runs' samples,
//! the runs moving and whether a landing waits, what a claim and a landing
//! need ([`crate::domain::disk::DiskState::needs`]). Short of either, it cleans what the ended runs
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
        DISK_CLEANUP, DISK_OPTIONS, DISK_SUBJECT, DiskNeeds, DiskState, RunSample,
        expected_run_size, gib,
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
const DISK_QUESTION: &str = "With no run moving, the free disk space of the queue's directory is below the reserve, and cleaning what the ended runs left (their build outputs and those of the runs nobody works on that wait for an answer, a landing or a resume, the worktrees, the Claude Code scratchpads and the run TMPDIRs of completed and canceled tasks' runs, `git worktree prune`, and the target directories of the automatic update and the landing recheck when neither runs) did not free enough: {free} free, a new run needs {claim} and a landing's verification {landing} (the reserve of {reserve}, [disk] reserve of dagq.toml as a share of the volume and at least {least}, plus what a run is expected to take, {expected}, from the sizes of the recent runs, for each run while others move). No new run is claimed and no run lands below the reserve. Free disk space (for example the worktrees of failed runs nobody looks at any more, or other files on that disk) and answer `done`, or answer `wait` to leave the queue waiting: the supervisor resumes by itself once there is room.";

/// What moves at a pass's reading of the disk: `in_flight`, the runs the
/// slots hold that go on by themselves, `landing`, how many of them are
/// landings under way, and `landing_waiting`, whether a run awaits its
/// landing that nothing but the disk holds.
#[derive(Debug, Clone, Copy)]
pub(in crate::application::supervise) struct DiskMoves {
    pub(in crate::application::supervise) in_flight: usize,
    pub(in crate::application::supervise) landing: usize,
    pub(in crate::application::supervise) landing_waiting: bool,
}

impl DiskMoves {
    /// The runs a landing's retry of its verification counts as moving:
    /// those besides the landings under way, so that the retry needs what
    /// the landing needed at its start, when its slot was not counted.
    pub(in crate::application::supervise) const fn besides_the_landing(self) -> usize {
        self.in_flight.saturating_sub(self.landing)
    }
}

/// What the supervisor knows of the disk between passes.
#[derive(Debug, Default)]
pub(in crate::application::supervise) struct DiskWatch {
    /// The recent runs' samples and when they were read.
    samples: Option<(Instant, Vec<RunSample>)>,
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
    /// There is not room for a landing's verification.
    pub(super) landing_short: bool,
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
    /// Check the free disk space for this pass (see the module): clean
    /// and ask when short, close the disk ask when there is room, and
    /// record the landings' hold. `landings` are the runs the slots hold
    /// waiting for their landing turn, `held` every run a slot holds,
    /// which a cleanup leaves alone, and `in_flight` how many of them go
    /// on by themselves (neither waiting for a person nor for their
    /// landing turn), whose end may make room. The claims' hold is judged
    /// when the supervisor claims, from the same reading.
    pub(in crate::application::supervise) fn check_disk(
        &mut self,
        env: &mut HostEnv<'_>,
        interval: Duration,
        landings: &[RunId],
        held: &super::cleanup::Held,
        moves: DiskMoves,
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
        let expected = self.expected_run_size(env)?;
        let free = self.free_bytes(env);
        // The moving runs' remaining counts 0: their size now is not
        // measured, and an assumed one held claims far longer than the
        // records (ADR-t4473-1).
        let state = DiskState {
            free,
            total: self.total_bytes(env).unwrap_or(0),
            expected,
            moving: in_flight,
            remaining: 0,
            landing_waiting,
        };
        let needs = state.needs(self.disk_config.reserve);
        self.disk.needs = needs;
        self.disk.claims = state.claims(self.disk_config.reserve);
        self.disk.moving.store(
            moves.besides_the_landing(),
            std::sync::atomic::Ordering::SeqCst,
        );
        let cleanup = self.disk.cleanup_epoch.read(in_flight);
        self.disk.cleanup_epoch = cleanup.next;
        if cleanup.discard_breakdown {
            self.disk.breakdown = None;
        }
        if cleanup.retry_now {
            self.disk.cleaned = None;
        }
        let most = Some(needs.claim.max(needs.landing));
        if short_of(free, most) {
            self.clean_for_disk(env, held, free, most, interval);
        }
        self.free = free;
        let reading = read_disk(free, &needs, self.cleanup.for_disk());
        self.disk.short = reading.short;
        self.disk.cleaning = reading.cleaning;
        self.disk.landing_short = reading.landing_short;
        let judged = judge_reading(reading, free, &needs, !landings.is_empty());
        let waiting: &[RunId] = if self.disk.landing_short {
            landings
        } else {
            &[]
        };
        let open = unclosed.iter().any(|ask| ask.is_open());
        let condition = disk_ask_condition(&ShortageReading {
            free,
            need: Some(needs.reserve),
            cleaned: self.disk.cleanup_epoch.ready(
                self.disk.breakdown.is_some(),
                self.cleanup.for_disk(),
                self.cleanup.running(),
            ),
            in_flight,
            open,
            cleaning: reading.cleaning,
        });
        let step = disk_ask_step(condition, self.disk.asked, open, &self.disk.joined, waiting);
        if forgets_cleanup(&step, reading.short) {
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
        if let Judged::Recorded { landings: hold, .. } = judged {
            env.record_hold(claim_hold::LANDINGS, hold.as_ref())?;
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
    /// What a new run is expected to take now, from the recent runs'
    /// samples read again every [`SAMPLES_INTERVAL`]
    /// ([`crate::application::recent_run_samples`]).
    fn expected_run_size(&mut self, env: &mut HostEnv<'_>) -> Result<Option<u64>> {
        let fresh = self
            .disk
            .samples
            .as_ref()
            .is_some_and(|(at, _)| samples_fresh(env.generators.clock.since(*at)));
        if !fresh {
            let samples = crate::application::recent_run_samples(&*env.queue)?;
            self.disk.samples = Some((env.generators.clock.monotonic(), samples));
        }
        let now = crate::application::unix_millis(env.generators.clock.system_time());
        Ok(self
            .disk
            .samples
            .as_ref()
            .and_then(|(_, samples)| expected_run_size(samples, now)))
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
    /// freed something, with the free bytes read again.
    pub(in crate::application::supervise) fn cleaned_for_disk(
        &mut self,
        env: &mut HostEnv<'_>,
        request: DiskRequest,
        removed: &Cleaned,
    ) {
        if removed.bytes == 0 {
            return;
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

/// Free bytes short of a need; neither unknown is short.
fn short_of(free: Option<u64>, need: Option<u64>) -> bool {
    matches!((free, need), (Some(free), Some(need)) if free < need)
}

/// What a pass reads of the disk ([`read_disk`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::application::supervise) struct Reading {
    /// Short of what a claim or a landing needs.
    pub(super) short: bool,
    /// Short of what a landing's verification needs.
    pub(super) landing_short: bool,
    /// Short while a cleanup for room runs or waits to ([`cleaning`]):
    /// nothing is held or asked for yet.
    pub(super) cleaning: bool,
}

/// Read the free bytes against what a claim and a landing need, with
/// `for_disk` whether a cleanup for room (or the rest of one another job
/// took on) runs or waits to. Short and not cleaning, the pass asks (the
/// disk ask) and records the landings' hold; with room it closes the ask.
pub(in crate::application::supervise) fn read_disk(
    free: Option<u64>,
    needs: &DiskNeeds,
    for_disk: bool,
) -> Reading {
    let short = short_of(free, Some(needs.claim.max(needs.landing)));
    Reading {
        short,
        landing_short: short_of(free, Some(needs.landing)),
        cleaning: cleaning(short, for_disk),
    }
}

/// Short while the cleanup for room runs, or the rest of one another job
/// took on (task 1478): the claims and landings short of room wait for
/// it, and nothing is held or asked for until it is done.
const fn cleaning(short: bool, for_disk: bool) -> bool {
    short && for_disk
}

/// What a pass records of its [`Reading`] ([`judge_reading`]).
#[derive(Debug, Clone, PartialEq)]
pub(in crate::application::supervise) enum Judged {
    /// Short while a cleanup for room runs or waits to: nothing is asked
    /// for or held until it is done (task 1478).
    Cleaning,
    /// Judged: the disk ask is opened (or joined) when `ask`, and closed
    /// once there is room otherwise; `landings` is the hold recorded on the
    /// landings (`landing_held` / `landing_resumed`).
    Recorded {
        ask: bool,
        landings: Option<ClaimHold>,
    },
}

/// Judge a pass's reading: nothing while cleaning; otherwise ask when
/// short, and hold the landings that wait (`landings_waiting`) while the
/// free bytes are short of what a landing needs.
pub(in crate::application::supervise) fn judge_reading(
    reading: Reading,
    free: Option<u64>,
    needs: &DiskNeeds,
    landings_waiting: bool,
) -> Judged {
    if reading.cleaning {
        return Judged::Cleaning;
    }
    let landings = if landings_waiting {
        ClaimHold::judge(&HoldInputs {
            free_bytes: free,
            needed_bytes: Some(needs.landing),
            ..HoldInputs::default()
        })
    } else {
        None
    };
    Judged::Recorded {
        ask: reading.short,
        landings,
    }
}

/// Whether the claims wait for a cleanup for room without a hold (task
/// 405): it runs (`cleaning`) and the free bytes are short of what a claim
/// needs. A claim there is room for goes on without waiting for it (task
/// 1478).
pub(in crate::application::supervise) fn claims_wait_for_cleanup(
    cleaning: bool,
    free: Option<u64>,
    need: Option<u64>,
) -> bool {
    cleaning && short_of(free, need)
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
    pub(super) free: Option<u64>,
    /// The reserve, independent of what claims and landings need.
    pub(super) need: Option<u64>,
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
    let short = short_of(reading.free, reading.need);
    if reading.open
        && clear_above(
            reading.free,
            reading.need,
            crate::domain::disk::DISK_CLOSE_MARGIN,
        )
    {
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

/// Free bytes above `need` by `margin` of it; either unknown is room, as
/// it is no shortage.
fn clear_above(free: Option<u64>, need: Option<u64>, margin: f64) -> bool {
    match (free, need) {
        (Some(free), Some(need)) => free >= need && (free - need) as f64 >= need as f64 * margin,
        _ => true,
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
    let question = DISK_QUESTION
        .replace("{free}", &show(free))
        .replace("{claim}", &show(Some(needs.claim)))
        .replace("{landing}", &show(Some(needs.landing)))
        .replace("{reserve}", &show(Some(needs.reserve)))
        .replace("{expected}", &expected)
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
    format!("{question}\n\nLargest directories after cleanup:\n{details}")
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
    use crate::domain::claim_hold::HoldReason;

    /// A landing's retry counts the runs moving besides the landing under
    /// way: with the landing alone in flight, none, as at its start.
    #[test]
    fn a_landings_retry_counts_the_runs_moving_besides_it() {
        let moves = |in_flight, landing| DiskMoves {
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
    use crate::domain::landing_hold::{self, LandingHold, LandingHoldInputs};

    const GIB: u64 = 1 << 30;

    /// A claim needs far more than a landing (a measured build), as the
    /// cases below read.
    const NEEDS: DiskNeeds = DiskNeeds {
        reserve: GIB,
        expected: Some(GIB),
        claim: 100 * GIB,
        landing: GIB,
        claims_wait: false,
    };

    /// What a pass does with `free` free bytes while a cleanup for room
    /// runs or waits (`for_disk`) or not, through the functions
    /// `check_disk` and `hold_claims` call: whether the claims wait for it
    /// without a hold, whether a claim is held (`claim_held`), how a
    /// landing waiting outside a drain goes, whether its hold is recorded
    /// (`landing_held`), and whether the disk ask is opened.
    fn pass(free: u64, for_disk: bool) -> (bool, bool, LandingHold, bool, bool) {
        let reading = read_disk(Some(free), &NEEDS, for_disk);
        let waits = claims_wait_for_cleanup(reading.cleaning, Some(free), Some(NEEDS.claim));
        let claim_held = !waits
            && ClaimHold::judge(&HoldInputs {
                free_bytes: Some(free),
                needed_bytes: Some(NEEDS.claim),
                ..HoldInputs::default()
            })
            .is_some();
        let landing = landing_hold::judge(LandingHoldInputs {
            landing_short: reading.landing_short,
            disk_cleaning: reading.cleaning,
            ..LandingHoldInputs::default()
        });
        let (landing_held, asks) = match judge_reading(reading, Some(free), &NEEDS, true) {
            Judged::Cleaning => (false, false),
            Judged::Recorded { ask, landings } => (landings.is_some(), ask),
        };
        (waits, claim_held, landing, landing_held, asks)
    }

    /// Task 1478: while the rest of a cleanup for room another job took on
    /// waits or runs, a disk short of what a claim or a landing needs opens
    /// no disk ask and records no `claim_held` or `landing_held`; a claim
    /// waits only while short of what a claim needs, a landing only while
    /// short of what a landing needs, and the side there is room for goes
    /// on. Once it is done the next reading decides: with room, nothing is
    /// held or asked; still short, the disk ask opens and the side short of
    /// room is held.
    #[test]
    fn the_rest_of_a_cleanup_for_room_holds_and_asks_nothing_until_it_is_done() {
        use LandingHold::{Proceed, Wait};
        let (short, landing_room, room) = (1, 4 * GIB, 1 << 40);
        // While it runs.
        assert_eq!(pass(short, true), (true, false, Wait, false, false));
        assert_eq!(
            pass(landing_room, true),
            (true, false, Proceed, false, false)
        );
        assert_eq!(pass(room, true), (false, false, Proceed, false, false));
        // Once done: enough, or still short.
        assert_eq!(pass(room, false), (false, false, Proceed, false, false));
        assert_eq!(pass(short, false), (false, true, Wait, true, true));
        assert_eq!(
            pass(landing_room, false),
            (false, true, Proceed, false, true)
        );
    }

    #[test]
    fn a_reading_is_short_of_the_larger_need_and_cleaning_only_while_short() {
        for (free, for_disk, short, landing_short, cleaning) in [
            (None, true, false, false, false),
            (Some(1), false, true, true, false),
            (Some(1), true, true, true, true),
            (Some(GIB - 1), true, true, true, true),
            (Some(GIB), true, true, false, true),
            (Some(100 * GIB - 1), false, true, false, false),
            (Some(100 * GIB), true, false, false, false),
        ] {
            assert_eq!(
                read_disk(free, &NEEDS, for_disk),
                Reading {
                    short,
                    landing_short,
                    cleaning
                },
                "{free:?} {for_disk}"
            );
        }
        assert!(!claims_wait_for_cleanup(true, Some(1), None));
        assert!(!claims_wait_for_cleanup(true, None, Some(1)));
        assert!(!claims_wait_for_cleanup(false, Some(1), Some(2)));
        assert!(claims_wait_for_cleanup(true, Some(1), Some(2)));
    }

    /// While cleaning nothing is recorded; judged, a short reading asks,
    /// and the landings are held only while some wait and the free bytes
    /// are short of what a landing needs.
    #[test]
    fn a_reading_is_recorded_only_once_no_cleanup_for_room_runs() {
        let short = read_disk(Some(1), &NEEDS, true);
        assert_eq!(
            judge_reading(short, Some(1), &NEEDS, true),
            Judged::Cleaning
        );
        let short = read_disk(Some(1), &NEEDS, false);
        let Judged::Recorded { ask, landings } = judge_reading(short, Some(1), &NEEDS, true) else {
            panic!("judged while cleaning");
        };
        assert!(ask);
        assert_eq!(
            landings.map(|hold| hold.reason),
            Some(HoldReason::DiskSpace)
        );
        assert_eq!(
            judge_reading(short, Some(1), &NEEDS, false),
            Judged::Recorded {
                ask: true,
                landings: None
            }
        );
        let landing_room = read_disk(Some(4 * GIB), &NEEDS, false);
        assert_eq!(
            judge_reading(landing_room, Some(4 * GIB), &NEEDS, true),
            Judged::Recorded {
                ask: true,
                landings: None
            }
        );
        let room = read_disk(Some(1 << 40), &NEEDS, true);
        assert_eq!(
            judge_reading(room, Some(1 << 40), &NEEDS, true),
            Judged::Recorded {
                ask: false,
                landings: None
            }
        );
    }

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
            free: Some(free),
            need: Some(100 * GIB),
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
            free: Some(99 * GIB),
            need: Some(100 * GIB),
            cleaned: false,
            in_flight: 1,
            open: true,
            cleaning: true,
        };
        assert_eq!(disk_ask_condition(&cleaning), AskCondition::Keep);
        cleaning.free = Some(105 * GIB);
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
                    free,
                    need,
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
            question.contains("what a run is expected to take, 8.0 GiB,"),
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
        assert!(unread.contains("expected to take, none yet,"), "{unread}");
    }
}
