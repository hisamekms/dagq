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
const DISK_QUESTION: &str = "The free disk space of the queue's directory stays below what the runs need, and cleaning what the ended runs left (their build outputs and those of the runs nobody works on that wait for an answer, a landing or a resume, the worktrees, the Claude Code scratchpads and the run TMPDIRs of completed and canceled tasks' runs, `git worktree prune`) did not free enough: {free} free, a new run needs {claim} and a landing's verification {landing} (the size of a recent run, the largest build outputs plus the largest Claude Code scratchpad and the largest run TMPDIR of the recent runs, times [disk] claim_factor / integrate_factor of dagq.toml). No new run is claimed and no run lands until there is room; the runs in flight go on. Free disk space (for example the worktrees of failed runs nobody looks at any more, or other files on that disk) and answer `done`, or answer `wait` to leave the queue waiting: the supervisor resumes by itself once there is room.";

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
        env: &mut PassEnv<'_>,
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
        if ask {
            let waiting: &[RunId] = if self.disk.landing_short {
                landings
            } else {
                &[]
            };
            let open = unclosed.iter().any(|ask| ask.is_open());
            self.ask_for_disk(env, free, &needs, waiting, open)?;
        } else if unclosed.iter().any(|ask| ask.is_open()) || self.disk.asked {
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
        } else {
            self.disk.cleaned = None;
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
    pub(super) fn free_bytes(&self, env: &PassEnv<'_>) -> Option<u64> {
        (self.free_space)(&env.layout.runs_dir)
            .or_else(|| env.layout.db.parent().and_then(self.free_space))
    }
    /// What a claim and a landing need, read again every
    /// [`NEEDS_INTERVAL`] from the latest `build_outputs_removed`,
    /// `scratchpad_removed` and `run_tmp_removed`.
    pub(super) fn disk_needs(&mut self, env: &mut PassEnv<'_>) -> Result<DiskNeeds> {
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
        env: &mut PassEnv<'_>,
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
        env: &mut PassEnv<'_>,
        request: DiskRequest,
        removed: &Cleaned,
    ) {
        if removed.bytes == 0 {
            return;
        }
        let after = self.free_bytes(env);
        info!(
            "the free disk space was short: removed {} bytes of what {} ended run(s) left",
            removed.bytes,
            removed.runs.len()
        );
        if let Err(error) = env.queue.record_queue_event(
            EventKind::AutoRepaired,
            disk_cleanup_record(request, removed, after, env.token),
        ) {
            warn!(error = %format_args!("{error:#}"), "the cleanup for disk space could not be recorded: {error:#}");
        }
    }
    /// Open the disk ask of this shortage, or add the runs whose landing
    /// waits for the disk to the open one; once opened it is not opened
    /// again until there is room or a person answers `done`.
    fn ask_for_disk(
        &mut self,
        env: &mut PassEnv<'_>,
        free: Option<u64>,
        needs: &DiskNeeds,
        runs: &[RunId],
        open: bool,
    ) -> Result<()> {
        let joining: Vec<Option<RunId>> = if self.disk.asked {
            if !open {
                // Answered `wait`: nothing more until there is room.
                return Ok(());
            }
            runs.iter()
                .filter(|run| !self.disk.joined.contains(run))
                .cloned()
                .map(Some)
                .collect()
        } else if runs.is_empty() {
            vec![None]
        } else {
            runs.iter().cloned().map(Some).collect()
        };
        let show = |bytes: Option<u64>| bytes.map_or_else(|| "unknown".into(), |b| gib(b as f64));
        let question = DISK_QUESTION
            .replace("{free}", &show(free))
            .replace("{claim}", &show(needs.claim))
            .replace("{landing}", &show(needs.landing));
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
    fn apply_disk_answers(&mut self, env: &mut PassEnv<'_>, unclosed: &[Ask]) -> Result<()> {
        for ask in unclosed.iter().filter(|ask| ask.answered_at.is_some()) {
            let answer = ask.answer.as_deref().unwrap_or_default().trim().to_owned();
            env.queue.close_ask(ask.id)?;
            info!(ask_id = %ask.id, "applied the answer {answer:?} of the disk ask {}", ask.id);
            if answer == "done" {
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
/// the bytes and runs it removed.
fn disk_cleanup_record(
    request: DiskRequest,
    removed: &Cleaned,
    after: Option<u64>,
    token: &LeaseToken,
) -> Value {
    json!({
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
    })
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
}
