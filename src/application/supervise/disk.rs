//! The free disk space a claim and a landing need (ADR-0047 decision 44,
//! task 377): on every pass the supervisor reads the free bytes of the
//! queue's directory and what a claim and a landing need
//! ([`DiskConfig::needs`]). Short of either, it cleans what the ended runs
//! left ([`HostOpsState::request_cleanup`] and `git worktree prune`,
//! off the loop: task 405) and reads again once that is done, recording
//! `auto_repaired` (`repair: disk_cleanup`) when it freed something; the
//! claims and landings wait for it without a hold, and for the rest of it
//! when another job took it on (task 1478). While one waits or runs no
//! other is asked for, and the next waits [`CLEANUP_INTERVAL`] from
//! the end of the last ([`asks_for_cleanup`], task 1627): the pass after
//! it judges the reading after it. Still short, it opens
//! the queue's one `cost` ask about the disk (`subject: disk`) once, and
//! the runs whose landing waits for the disk join it. The claims are held through
//! the claim pass's hold (`claim_held`, reason `disk_space`), the
//! landings through `landing_held` / `landing_resumed`; a run waiting to
//! land stays awaiting integration and starts no verification.

use super::{cleanup::DiskRequest, *};
use crate::domain::EventKind;
use crate::domain::{
    Ask,
    disk::{DISK_CLEANUP, DISK_OPTIONS, DISK_SUBJECT, DiskNeeds, gib},
};

/// How long the needs read from the recent runs' sizes are kept.
const NEEDS_INTERVAL: Duration = Duration::from_secs(60);
/// Least time from the end of a cleanup for room (the rest of one another
/// job took on included) to the next one (task 1627).
pub const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

/// The answer the runtime closes the disk ask with once there is room.
const DISK_ENOUGH_CLOSED: &str = "the free disk space is enough again; closed by the runtime";

/// The question of the disk ask; the runs whose landing waits follow it.
const DISK_QUESTION: &str = "The free disk space of the queue's directory stays below what the runs need, and cleaning what the ended runs left (their build outputs and those of the runs nobody works on that wait for an answer, a landing or a resume, the worktrees, the Claude Code scratchpads and the run TMPDIRs of completed and canceled tasks' runs, `git worktree prune`, and the target directories of the automatic update and the landing recheck when neither runs) did not free enough: {free} free, a new run needs {claim} and a landing's verification {landing} (the size of a recent run, the largest build outputs plus the largest Claude Code scratchpad and the largest run TMPDIR of the recent runs, times [disk] claim_factor / integrate_factor of dagq.toml). No new run is claimed and no run lands until there is room; the runs in flight go on. Free disk space (for example the worktrees of failed runs nobody looks at any more, or other files on that disk) and answer `done`, or answer `wait` to leave the queue waiting: the supervisor resumes by itself once there is room.";

/// What the supervisor knows of the disk between passes.
#[derive(Debug, Default)]
pub(super) struct DiskWatch {
    /// The needs and when they were read.
    needs: Option<(Instant, DiskNeeds)>,
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
}

impl HostOpsState {
    /// Check the free disk space for this pass (see the module): clean
    /// and ask when short, close the disk ask when there is room, and
    /// record the landings' hold. `landings` are the runs the slots hold
    /// waiting for their landing turn, and `held` every run a slot holds,
    /// which a cleanup leaves alone. The claims' hold is judged when the
    /// supervisor claims, from the same reading.
    pub(super) fn check_disk(
        &mut self,
        env: &mut HostEnv<'_>,
        interval: Duration,
        landings: &[RunId],
        held: &[RunId],
    ) -> Result<()> {
        let unclosed: Vec<Ask> = env
            .queue
            .asks(AskQuery::default())?
            .into_iter()
            .filter(is_disk_ask)
            .collect();
        self.apply_disk_answers(env, &unclosed)?;
        let needs = self.disk_needs(env)?;
        let free = self.free_bytes(env);
        let most = needs.claim.max(needs.landing);
        if short_of(free, most) {
            self.clean_for_disk(env, held, free, most, interval);
        }
        self.free = free;
        let reading = read_disk(free, &needs, self.cleanup.for_disk());
        self.disk.short = reading.short;
        self.disk.cleaning = reading.cleaning;
        self.disk.landing_short = reading.landing_short;
        let Judged::Recorded {
            ask,
            landings: hold,
        } = judge_reading(reading, free, &needs, !landings.is_empty())
        else {
            return Ok(());
        };
        let waiting: &[RunId] = if self.disk.landing_short {
            landings
        } else {
            &[]
        };
        let open = unclosed.iter().any(|ask| ask.is_open());
        match disk_ask_step(ask, self.disk.asked, open, &self.disk.joined, waiting) {
            AskStep::Open(joining) => self.ask_for_disk(env, free, &needs, joining)?,
            AskStep::Nothing => {}
            AskStep::Close => {
                self.disk.cleaned = None;
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
            AskStep::Forget => self.disk.cleaned = None,
        }
        env.record_hold(claim_hold::LANDINGS, hold.as_ref())?;
        Ok(())
    }
    /// A handoff found after this pass polled the cleanup and read the
    /// disk: the cleanup ends now, and the rest of a cleanup for room it
    /// started this pass counts as one for this pass's reading, so the
    /// drain's landings wait for it too (task 1426). A job that finished
    /// since the reading is not joined here but on the next pass, whose
    /// reading after it decides the landings.
    pub(super) fn end_cleanup_for_handoff(&mut self) {
        self.end_cleanup();
        self.disk.cleaning = cleaning(self.disk.short, self.cleanup.for_disk());
    }
    /// The free bytes of the queue's directory, where the run worktrees
    /// are; `None` when they cannot be read.
    pub(super) fn free_bytes(&self, env: &HostEnv<'_>) -> Option<u64> {
        (self.free_space)(&env.layout.runs_dir)
            .or_else(|| env.layout.db.parent().and_then(self.free_space))
    }
    /// What a claim and a landing need, read again every
    /// [`NEEDS_INTERVAL`] from the latest `build_outputs_removed`,
    /// `scratchpad_removed` and `run_tmp_removed`.
    pub(super) fn disk_needs(&mut self, env: &mut HostEnv<'_>) -> Result<DiskNeeds> {
        if let Some((at, needs)) = self.disk.needs
            && at.elapsed() < NEEDS_INTERVAL
        {
            return Ok(needs);
        }
        let builds =
            crate::application::recent_run_sizes(&*env.queue, self.disk_config.sample_runs)?;
        let needs = self.disk_config.needs(&builds);
        self.disk.needs = Some((Instant::now(), needs));
        Ok(needs)
    }
    /// Ask for what the ended runs left to be cleaned for room, when
    /// [`asks_for_cleanup`] says so; the job does it off the loop, and
    /// [`Self::cleaned_for_disk`] follows.
    fn clean_for_disk(
        &mut self,
        env: &mut HostEnv<'_>,
        held: &[RunId],
        free: Option<u64>,
        needed: Option<u64>,
        interval: Duration,
    ) {
        if !asks_for_cleanup(
            self.disk.cleaned.map(|at| at.elapsed()),
            self.cleanup.for_disk(),
            interval,
        ) {
            return;
        }
        // A request a drain does not take is not a cleanup: the next one
        // after the drain ends is not kept waiting for it (task 1427).
        if self.request_cleanup(env, held, None, Some(DiskRequest { free, needed })) {
            self.disk.cleaned = Some(Instant::now());
        }
    }
    /// Once a cleanup for room is done: record `auto_repaired` when it
    /// freed something, with the free bytes read again.
    pub(super) fn cleaned_for_disk(
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
        let question = disk_question(free, needs);
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
pub(super) struct Reading {
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
pub(super) fn read_disk(free: Option<u64>, needs: &DiskNeeds, for_disk: bool) -> Reading {
    let short = short_of(free, needs.claim.max(needs.landing));
    Reading {
        short,
        landing_short: short_of(free, needs.landing),
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
pub(super) enum Judged {
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
pub(super) fn judge_reading(
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
            needed_bytes: needs.landing,
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
pub(super) fn claims_wait_for_cleanup(
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
pub(super) fn asks_for_cleanup(
    since_cleaned: Option<Duration>,
    for_disk: bool,
    interval: Duration,
) -> bool {
    !for_disk && since_cleaned.is_none_or(|since| since >= interval)
}

/// What a judged pass does with the disk ask ([`disk_ask_step`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AskStep {
    /// Open the disk ask, once per entry: `None` with no run, a run to
    /// add the run whose landing waits to it.
    Open(Vec<Option<RunId>>),
    /// Short, but the ask of this shortage was answered `wait`: nothing
    /// until there is room.
    Nothing,
    /// There is room again: close the disk ask and forget this shortage.
    Close,
    /// There is room and nothing was asked: only the cleanup's interval
    /// starts anew.
    Forget,
}

/// What a judged pass does with the disk ask (ADR-0047 decision 44):
/// short (`ask`), the ask of this shortage is opened once (with no run
/// when no landing waits, else with each `waiting` run) and the waiting
/// runs not `joined` yet are added while it is `open`; once `asked` and no
/// longer open (answered `wait`), nothing is asked until there is room.
/// With room, the ask is closed when one is open or was `asked`.
pub(super) fn disk_ask_step(
    ask: bool,
    asked: bool,
    open: bool,
    joined: &[RunId],
    waiting: &[RunId],
) -> AskStep {
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

/// Whether the answer of a disk ask asks again (a person freed the disk:
/// `done`), cleaning and checking at once; `wait` and any other answer
/// leave the queue waiting without asking again.
pub(super) fn asks_again_on(answer: &str) -> bool {
    answer.trim() == "done"
}

/// The question of the disk ask, with the free bytes and what a claim and
/// a landing need (`unknown` when not read).
pub(super) fn disk_question(free: Option<u64>, needs: &DiskNeeds) -> String {
    let show = |bytes: Option<u64>| bytes.map_or_else(|| "unknown".into(), |b| gib(b as f64));
    DISK_QUESTION
        .replace("{free}", &show(free))
        .replace("{claim}", &show(needs.claim))
        .replace("{landing}", &show(needs.landing))
}

/// The queue's `cost` ask about the disk.
fn is_disk_ask(ask: &Ask) -> bool {
    ask.kind == AskKind::QueueHold
        && ask.reason_category == AskReason::Cost
        && ask.subject.as_deref() == Some(DISK_SUBJECT)
}

/// What a cleanup ([`HostOpsState::request_cleanup`]) removed.
#[derive(Debug, Default)]
pub(super) struct Cleaned {
    pub(super) bytes: u64,
    pub(super) runs: Vec<RunId>,
    /// The build caches of the queue's directory it cleared, by path.
    pub(super) caches: Vec<String>,
}

impl Cleaned {
    pub(super) fn add(&mut self, run: &RunId, bytes: u64) {
        if bytes > 0 {
            self.bytes += bytes;
            if !self.runs.contains(run) {
                self.runs.push(run.clone());
            }
        }
    }
    /// A build cache of no run cleared at `path`.
    pub(super) fn add_cache(&mut self, path: &str, bytes: u64) {
        if bytes > 0 {
            self.bytes += bytes;
            self.caches.push(path.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::claim_hold::HoldReason;
    use crate::domain::landing_hold::{self, LandingHold, LandingHoldInputs};

    const GIB: u64 = 1 << 30;

    /// A claim needs far more than a landing (a measured build), as the
    /// cases below read.
    const NEEDS: DiskNeeds = DiskNeeds {
        largest_build: None,
        claim: Some(100 * GIB),
        landing: Some(GIB),
    };

    /// What a pass does with `free` free bytes while a cleanup for room
    /// runs or waits (`for_disk`) or not, through the functions
    /// `check_disk` and `hold_claims` call: whether the claims wait for it
    /// without a hold, whether a claim is held (`claim_held`), how a
    /// landing waiting outside a drain goes, whether its hold is recorded
    /// (`landing_held`), and whether the disk ask is opened.
    fn pass(free: u64, for_disk: bool) -> (bool, bool, LandingHold, bool, bool) {
        let reading = read_disk(Some(free), &NEEDS, for_disk);
        let waits = claims_wait_for_cleanup(reading.cleaning, Some(free), NEEDS.claim);
        let claim_held = !waits
            && ClaimHold::judge(&HoldInputs {
                free_bytes: Some(free),
                needed_bytes: NEEDS.claim,
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
        // No need known: never short.
        let unknown = DiskNeeds::default();
        assert!(!read_disk(Some(1), &unknown, true).short);
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
            disk_ask_step(true, false, false, &[], &[]),
            AskStep::Open(vec![None])
        );
        assert_eq!(
            disk_ask_step(true, false, false, &[], &[a.clone(), b.clone()]),
            AskStep::Open(vec![Some(a.clone()), Some(b.clone())])
        );
        // Asked and open: told once, only a new landing joins.
        assert_eq!(
            disk_ask_step(true, true, true, &[], &[]),
            AskStep::Open(vec![])
        );
        assert_eq!(
            disk_ask_step(
                true,
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
                true,
                true,
                false,
                std::slice::from_ref(&a),
                std::slice::from_ref(&b)
            ),
            AskStep::Nothing
        );
        // Room: closed when asked or open, else only forgotten.
        assert_eq!(disk_ask_step(false, true, false, &[], &[]), AskStep::Close);
        assert_eq!(disk_ask_step(false, false, true, &[], &[]), AskStep::Close);
        assert_eq!(
            disk_ask_step(false, false, false, &[], &[a]),
            AskStep::Forget
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
            disk_ask_step(true, false, false, &[], &[]),
            AskStep::Open(vec![None])
        );
        // and a cleanup for room is asked for at once.
        assert!(asks_for_cleanup(None, false, CLEANUP_INTERVAL));
    }

    /// The disk ask's question names the free bytes, what a claim and a
    /// landing need, and the scratchpads both among what was cleaned and
    /// in the size of a run the needs follow (task 1100); nothing read is
    /// `unknown`.
    #[test]
    fn the_disk_question_names_the_reading_the_needs_and_the_scratchpads() {
        let question = disk_question(
            Some(GIB / 2),
            &DiskNeeds {
                largest_build: None,
                claim: Some(GIB),
                landing: Some(GIB),
            },
        );
        assert!(question.contains("0.5 GiB free"), "{question}");
        assert!(
            question.contains("a new run needs 1.0 GiB and a landing's verification 1.0 GiB"),
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
        assert!(
            question.contains("the largest build outputs plus the largest Claude Code scratchpad"),
            "{question}"
        );
        assert!(!question.contains("Affected:"));
        let unread = disk_question(None, &DiskNeeds::default());
        assert!(unread.contains("unknown free"), "{unread}");
        assert!(unread.contains("a new run needs unknown"), "{unread}");
    }
}
