//! The handoff of a supervisor to another binary (ADR-0045 decision 10):
//! asked through its registration, the supervisor starts no new work, lets
//! the validations and landings in progress finish, stops its headless
//! jobs, and ends its loop so the entry point can exec the new binary under
//! the same pid. The new process keeps the token, the registration and
//! every lease, and rebuilds each run's slot the way an adoption does
//! (ADR-0039), from the queue and the run files, without writing
//! `run_adopted`. What the queue does not hold — a resumed session's
//! request, an `/exit` a rejected run waits for — goes through a
//! `handoff.json` in the run directory ([`write_snapshot`],
//! [`take_snapshot`]).
//!
//! This module holds host運用's side: the registration of this process and
//! the state of its handoff and stop ([`Registration`]), with the decision
//! on a handoff request read again ([`reread`]). The slots are
//! 実行と着地's: the loop snapshots them before the exec and rebuilds them
//! after it (`handoff_slots`).

use super::*;
use crate::application::SupervisorRegistry;
use crate::domain::EventKind;
use crate::domain::LeaseToken;
use serde::Deserialize;

/// The run event of each run the process that took a registration over
/// after an exec found leased to it.
pub const SUPERVISOR_HANDED_OFF: &str =
    crate::domain::event_kind::EventKind::SupervisorHandedOff.as_str();

/// What a run directory's `handoff.json` holds for the next process.
const SNAPSHOT: &str = "handoff.json";

/// host運用's registration of this supervisor: its token and heartbeat, and
/// the handoff and stop it drains for. Like [`super::HostOpsState`]'s, its
/// fields are host運用's own: the loop reads them and takes its steps of a
/// handoff and a stop through the methods below.
pub(in crate::application::supervise) struct Registration {
    /// The registration's token, which every lease of this process carries.
    token: LeaseToken,
    heartbeat: Heartbeat,
    /// The binary a handoff asked this process to exec (ADR-0045 decision
    /// 10): no new work starts, and the loop ends once every slot rests at
    /// a point the next process rebuilds it from.
    handoff: Option<String>,
    /// Set when the loop ended for that exec: the registration stays.
    exec: Option<String>,
    /// This pass drains (a stop, a handoff, or claiming stopped after a
    /// provisioning failure): nothing may wait for the program to appear.
    draining: bool,
    /// This pass drains for a stop (SIGINT/SIGTERM or an owner gone).
    stopping: bool,
    /// Whether this process recorded `supervisor_draining` for its stop
    /// request (task 1277): once, on the first pass that saw it.
    stop_recorded: bool,
    /// When this process found its owner gone ([`owner_gone`]), on the
    /// monotonic clock: it stops for that rather than on a stop request,
    /// the reason its `supervisor_draining` and `supervisor_stopped`
    /// record, and lets its runs go instead of draining them
    /// ([`owner_gone_ends`]).
    owner_gone: Option<Instant>,
}

/// How long a supervisor whose owner is gone may still wait, from the
/// pass that found it gone to its exit: for the work it cannot give back
/// (a validation, a landing, an e2e, the verification of an
/// `already_satisfied` claim), then for the jobs the end of its loop
/// joins (the cleanup, the ledger, the host's sample). What still runs
/// then is left: the leases go stale with its process, and a job's
/// outcomes are not recorded. Tests shorten it
/// ([`super::LoopSettings::owner_gone_grace`]).
pub const OWNER_GONE_GRACE: Duration = Duration::from_secs(60);

/// Whether the loop of a supervisor whose owner has been gone for
/// `gone_for` ends now, `slots` holding runs after it let go every one it
/// could: once none is left, or once `grace` is over. Nothing waits for a
/// person's answer, a headless job or a session: nobody reads them after
/// the owner (a test) is gone.
pub(in crate::application::supervise) fn owner_gone_ends(
    gone_for: Duration,
    grace: Duration,
    slots: usize,
) -> bool {
    slots == 0 || gone_for >= grace
}

/// How long the end of the loop of a supervisor whose owner has been gone
/// for `gone_for` may still wait for its jobs: what is left of `grace`.
pub(in crate::application::supervise) fn owner_gone_left(
    gone_for: Duration,
    grace: Duration,
) -> Duration {
    grace.saturating_sub(gone_for)
}

/// What the end of the loop does next with a job it waits for
/// ([`job_wait`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::application::supervise) enum JobWait {
    /// It finished: join it, which does not wait.
    Join,
    /// It runs and its deadline is not over: look again.
    Wait,
    /// It runs and its deadline is over: leave it running.
    Leave,
}

/// [`JobWait`] of a job that `finished` or not, with `left` the time from
/// now to its deadline (zero once it is over; none waits until the job
/// finishes: every end but that of a supervisor whose owner is gone).
pub(in crate::application::supervise) fn job_wait(
    finished: bool,
    left: Option<Duration>,
) -> JobWait {
    if finished {
        JobWait::Join
    } else if left.is_some_and(|left| left.is_zero()) {
        JobWait::Leave
    } else {
        JobWait::Wait
    }
}

/// How [`send_within`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::application::supervise) enum Sent {
    /// All was taken.
    Taken,
    /// Not all was taken after the tries, or before the deadline was over.
    NotTaken,
    /// The deadline was over before anything was sent.
    Late,
}

/// Send what the end of the loop holds by `send` (whether all of it was
/// taken) until it is, at most `tries` times `poll` apart, never past the
/// deadline of a supervisor whose owner is gone: `left` is what is left of
/// it now (none for every other end, which has no deadline), and nothing is
/// sent, nor waited for, once it is over. `sleep` waits. What was not
/// taken, or not sent, stays for its caller to keep.
pub(in crate::application::supervise) fn send_within(
    tries: u32,
    poll: Duration,
    left: &dyn Fn() -> Option<Duration>,
    sleep: &mut dyn FnMut(Duration),
    send: &mut dyn FnMut() -> bool,
) -> Sent {
    let over = || left().is_some_and(|left| left.is_zero());
    if over() {
        return Sent::Late;
    }
    for attempt in 1..=tries {
        if send() {
            return Sent::Taken;
        }
        if attempt == tries || over() {
            break;
        }
        sleep(left().map_or(poll, |left| poll.min(left)));
        if over() {
            break;
        }
    }
    Sent::NotTaken
}

/// Whether the process a supervisor stops with is gone
/// ([`crate::domain::queue_service::OWNER_PID_ENV`], a test's pid), as
/// `alive` finds it: never without an owner, which is every production
/// start (`up`'s own environment has none, and nothing else gives one).
pub(in crate::application::supervise) fn owner_gone(
    owner: Option<u32>,
    alive: impl Fn(u32) -> bool,
) -> bool {
    owner.is_some_and(|pid| !alive(pid))
}

/// The reason a stop is recorded with: the owner's going, or a stop
/// request.
const fn stop_reason(owner_gone: bool) -> &'static str {
    if owner_gone {
        "owner_gone"
    } else {
        "stop_requested"
    }
}

/// What the queue's handoff request, read again, makes of the handoff this
/// process drains for (task 1286).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::application::supervise) enum HandoffChange {
    /// The same binary is asked: the drain goes on.
    Same,
    /// Another binary is asked: the drain goes on for it.
    Replaced(String),
    /// The request was withdrawn: normal work resumes.
    Withdrawn,
}

/// [`HandoffChange`] of the handoff to `binary` given the request read now
/// (`requested`).
pub(in crate::application::supervise) fn handoff_change(
    binary: &str,
    requested: Option<String>,
) -> HandoffChange {
    match requested {
        Some(now) if now == binary => HandoffChange::Same,
        Some(now) => HandoffChange::Replaced(now),
        None => HandoffChange::Withdrawn,
    }
}

/// What a pass does with the handoff to `binary` it drains for, given the
/// request read again (`requested`) and whether it drains for a stop too
/// (`stopping`): the handoff it drains for now, whether the request was
/// withdrawn (normal work resumes on this pass, without reading a new
/// request), and whether the cleanup goes back to normal (not while a stop
/// keeps it ending, task 1427). The same read follows a take that changed
/// no row: the request was withdrawn or replaced meanwhile, or the same
/// binary was asked again, which the next pass takes (task 1286).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::application::supervise) struct Reread {
    pub(in crate::application::supervise) change: HandoffChange,
    pub(in crate::application::supervise) handoff: Option<String>,
    pub(in crate::application::supervise) resume_cleanup: bool,
}

/// [`Reread`] of the handoff to `binary`.
pub(in crate::application::supervise) fn reread(
    binary: &str,
    requested: Option<String>,
    stopping: bool,
) -> Reread {
    let change = handoff_change(binary, requested);
    let (handoff, resume_cleanup) = match &change {
        HandoffChange::Same => (Some(binary.to_owned()), false),
        HandoffChange::Replaced(now) => (Some(now.clone()), false),
        HandoffChange::Withdrawn => (None, !stopping),
    };
    Reread {
        change,
        handoff,
        resume_cleanup,
    }
}

/// Whether a pass drains: on a stop, while claiming stopped (a
/// provisioning failure), and while a handoff is asked.
pub(in crate::application::supervise) const fn drains(
    stopping: bool,
    claiming: bool,
    handing_off: bool,
) -> bool {
    stopping || !claiming || handing_off
}

/// Whether a landing in progress hands its run over instead of verifying
/// it again (ADR-t3344-1): only while a handoff is asked and no stop is,
/// for a stop wins over a handoff and its drain verifies again as a pass
/// that does not drain.
pub(in crate::application::supervise) const fn hands_landings_over(
    stopping: bool,
    handing_off: bool,
) -> bool {
    handing_off && !stopping
}

impl Registration {
    /// The registration `token` takes, kept by `heartbeat`: no handoff, no
    /// stop.
    pub(in crate::application::supervise) const fn new(
        token: LeaseToken,
        heartbeat: Heartbeat,
    ) -> Self {
        Self {
            token,
            heartbeat,
            handoff: None,
            exec: None,
            draining: false,
            stopping: false,
            stop_recorded: false,
            owner_gone: None,
        }
    }

    /// The registration's token, which every lease of this process carries.
    pub(in crate::application::supervise) const fn token(&self) -> &LeaseToken {
        &self.token
    }

    /// Whether the heartbeat still holds the registration and the leases.
    pub(in crate::application::supervise) fn check_heartbeat(&self) -> Result<()> {
        self.heartbeat.check()
    }

    /// The binary a handoff asked this process to exec, if one did.
    pub(in crate::application::supervise) fn handoff(&self) -> Option<&str> {
        self.handoff.as_deref()
    }

    /// Whether a handoff is asked: no new work starts.
    pub(in crate::application::supervise) const fn handing_off(&self) -> bool {
        self.handoff.is_some()
    }

    /// Whether the loop ended for an exec: the registration stays.
    pub(in crate::application::supervise) const fn execs(&self) -> bool {
        self.exec.is_some()
    }

    /// Whether this pass drains.
    pub(in crate::application::supervise) const fn draining(&self) -> bool {
        self.draining
    }

    /// Whether this pass drains for a stop, which wins over a handoff.
    pub(in crate::application::supervise) const fn stopping(&self) -> bool {
        self.stopping
    }

    /// Whether the stop request was recorded already.
    pub(in crate::application::supervise) const fn stop_recorded(&self) -> bool {
        self.stop_recorded
    }

    /// When this process found its owner gone, if it did.
    pub(in crate::application::supervise) const fn owner_gone_since(&self) -> Option<Instant> {
        self.owner_gone
    }

    /// This process found its owner gone at `now` (monotonic): it stops for
    /// that.
    pub(in crate::application::supervise) fn note_owner_gone(&mut self, now: Instant) {
        self.owner_gone = Some(now);
    }

    /// Read the queue's handoff request for this process: the handoff it
    /// drains for from now, if one is asked.
    pub(in crate::application::supervise) fn read_handoff(
        &mut self,
        queue: &(impl SupervisorRegistry + ?Sized),
    ) -> Result<Option<&str>> {
        self.handoff = queue.handoff_request(&self.token)?;
        Ok(self.handoff())
    }

    /// Drain for the handoff to `binary` as the request read again makes of
    /// it ([`reread`]): on for the same binary or the one that replaced it,
    /// or no longer once it was withdrawn.
    pub(in crate::application::supervise) fn follow_reread(
        &mut self,
        binary: &str,
        reread: &Reread,
    ) {
        if let HandoffChange::Replaced(now) = &reread.change {
            self.replace_handoff(binary, now);
        }
        self.handoff.clone_from(&reread.handoff);
    }

    /// The handoff request this process drains for was replaced by one for
    /// `now`: the drain goes on for the new binary (task 1286).
    fn replace_handoff(&self, binary: &str, now: &str) {
        info!(
            "supervisor {} handoff to {binary} was replaced by a handoff to {now}: no new work starts; it execs {now} once the validations and landings in progress are done",
            self.token
        );
    }

    /// Whether this pass drains ([`drains`]), with `stopping` and `claiming`
    /// as the pass found them and the handoff it drains for.
    pub(in crate::application::supervise) fn drain_for(&mut self, stopping: bool, claiming: bool) {
        self.stopping = stopping;
        self.draining = drains(stopping, claiming, self.handing_off());
    }

    /// This pass drains for the handoff asked.
    pub(in crate::application::supervise) fn drain(&mut self) {
        self.draining = true;
    }

    /// The loop ends for the exec of `binary`: the registration stays for
    /// the next process.
    pub(in crate::application::supervise) fn exec_into(&mut self, binary: String) {
        self.exec = Some(binary);
    }

    /// Record, once, that this process drains for a stop request (SIGINT /
    /// SIGTERM, from `down`, the drain of `up` or `install
    /// --allow-breaking`, or launchd's bootout) (task 1277), or because its
    /// owner is gone ([`owner_gone`], with that reason), with the runs
    /// its slots hold: a handoff waiting for it fails at once instead of at
    /// its timeout, and the observer reads the stop behind the claims and
    /// resumes it holds. A failed write is only logged and tried again on
    /// the next pass; one kept to be sent is recorded.
    pub(in crate::application::supervise) fn record_stop_request(
        &mut self,
        queue: &(impl SupervisorRegistry + RunLog + ?Sized),
        build: &str,
        runs: &[RunId],
    ) {
        let handoff = match &self.handoff {
            Some(binary) => Some(binary.clone()),
            None => queue.handoff_request(&self.token).ok().flatten(),
        };
        let payload = json!({
            "supervisor": self.token,
            "pid": std::process::id(),
            "build": build,
            "reason": stop_reason(self.owner_gone.is_some()),
            "handoff_binary": handoff,
            "runs": runs,
        });
        // A record kept to be sent is recorded: asked again on the next
        // pass, the stop would be another.
        match queue.record_queue_event(EventKind::SupervisorDraining, payload) {
            Err(error) if !crate::application::supervisor_service::resend::kept(&error) => {
                warn!(error = %format_args!("{error:#}"), "the supervisor's stop request could not be recorded: {error:#}");
            }
            _ => {
                self.stop_recorded = true;
                info!(
                    "supervisor {} asked to stop: it drains the runs in progress; a stop wins over any handoff",
                    self.token
                );
            }
        }
    }

    /// The end of a loop not ended for an exec, whose heartbeat holds: stop
    /// the heartbeat, and remove the registration with the record of the
    /// stop (ADR-0051 decision 10; `ok` whether the loop ended without an
    /// error) in one transaction (ADR-t1662-1 decision 6). A record that
    /// fails keeps the row, which goes stale for the next `up` or `down` to
    /// prune. Whether the loop ended so; an exec, or a lost heartbeat,
    /// keeps the registration. `before_removal` runs once the registration
    /// is to go, before it goes: what has to be in place before another
    /// process can see it gone.
    pub(in crate::application::supervise) fn deregister(
        &mut self,
        queue: &(impl SupervisorRegistry + ?Sized),
        version: &str,
        ok: bool,
        before_removal: &mut dyn FnMut(),
    ) -> bool {
        if self.exec.is_some() || self.heartbeat.check().is_err() {
            return false;
        }
        before_removal();
        // The mark of the stop; an exec leaves it to the next process's
        // handoff mark.
        let mut stopped = json!({
            "supervisor": self.token,
            "dagq_version": version,
            "outcome": if ok { "stopped" } else { "failed" },
        });
        if self.owner_gone.is_some() {
            stopped["reason"] = json!(stop_reason(true));
        }
        self.heartbeat.stop();
        if let Err(error) =
            queue.prune_supervisor(&self.token, EventKind::SupervisorStopped, &|_| {
                stopped.clone()
            })
        {
            warn!(error = %format_args!("{error:#}"), "the supervisor's stop could not be recorded, and its registration is left for the next prune: {error:#}");
        }
        true
    }
}

/// `handoff.json`: the part of a slot the queue does not hold, written
/// before the exec by the supervisor `token`. Only that supervisor's next
/// process reads it; a file another process left behind is ignored.
#[derive(Debug, Serialize, Deserialize)]
struct Stamped {
    token: LeaseToken,
    #[serde(flatten)]
    snapshot: Snapshot,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(in crate::application::supervise) enum Snapshot {
    /// A resumed session of a `needs_session` run.
    Resume {
        workspace: String,
        attempt: usize,
        started_at: f64,
        message: String,
        /// What `message` took when it was built (ADR-t2072-1); a file
        /// written by an older binary has none.
        #[serde(default)]
        message_bytes: Option<PromptBytes>,
        message_sent_at: Option<f64>,
        /// A field of the interactive sessions' watch, kept so that the
        /// files of every binary still parse; the restore ignores it and
        /// does not watch with it.
        not_ready_asked: bool,
        exit_requested: bool,
        /// Like `not_ready_asked`: read for compatibility, not used to watch.
        #[serde(default)]
        exit_typed: bool,
        exit_for_silence: bool,
        approved: bool,
    },
    /// The `/exit` of a run that rests after its validation.
    Exit {
        workspace: Option<String>,
        resume: Option<usize>,
        close: bool,
        requested: bool,
        timed_out: bool,

        exit_for_silence: bool,
    },
}

/// Which [`Snapshot`] a run directory's `handoff.json` held, as
/// `supervisor_handed_off`'s `state` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::application::supervise) enum SnapshotKind {
    Resume,
    Exit,
}

impl SnapshotKind {
    pub(in crate::application::supervise) const fn as_str(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Exit => "exit",
        }
    }
}

impl Snapshot {
    pub(in crate::application::supervise) const fn kind(&self) -> SnapshotKind {
        match self {
            Self::Resume { .. } => SnapshotKind::Resume,
            Self::Exit { .. } => SnapshotKind::Exit,
        }
    }
}

pub(in crate::application::supervise) fn seconds(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub(in crate::application::supervise) fn time(seconds: f64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs_f64(seconds.max(0.0))
}

/// Write `snapshot` into `dir`'s `handoff.json`, stamped with `token`.
pub(in crate::application::supervise) fn write_snapshot(
    files: &dyn RunFiles,
    token: &LeaseToken,
    dir: &Path,
    snapshot: Snapshot,
) -> Result<()> {
    let text = serde_json::to_vec(&Stamped {
        token: token.clone(),
        snapshot,
    })?;
    files.write(&dir.join(SNAPSHOT), &text).map_err(Into::into)
}

/// Read and remove `run`'s `handoff.json`; `None` without one, when it does
/// not parse, or when another token wrote it.
pub(in crate::application::supervise) fn take_snapshot(
    files: &dyn RunFiles,
    token: &LeaseToken,
    run: &TaskRun,
) -> Option<Snapshot> {
    let path = Path::new(run.run_dir()?).join(SNAPSHOT);
    if !files.is_file(&path) {
        return None;
    }
    let text = files.read(&path).ok();
    let _ = files.remove_file(&path);
    text.and_then(|text| serde_json::from_slice::<Stamped>(&text).ok())
        .filter(|stamped| stamped.token == *token)
        .map(|stamped| stamped.snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A supervisor without an owner (every production start: `up`,
    /// `install`, the automatic update) never stops for one, whatever
    /// `alive` says, so its life (the handoff's exec, the drain, `up` and
    /// `down`) is as before; one with an owner stops once it is gone, and
    /// records that apart from a stop request.
    #[test]
    fn only_a_supervisor_with_an_owner_stops_once_the_owner_is_gone() {
        assert!(!owner_gone(None, |_| false));
        assert!(!owner_gone(None, |_| true));
        assert!(!owner_gone(Some(42), |pid| pid == 42));
        assert!(owner_gone(Some(42), |pid| pid != 42));
        assert_eq!(stop_reason(false), "stop_requested");
        assert_eq!(stop_reason(true), "owner_gone");
    }

    /// The end of the loop sends what it holds again a poll apart, at most
    /// its tries, with no deadline; a supervisor whose owner is gone sends
    /// and waits only within what is left of its grace (here shortened to
    /// 500 ms, its sends taking 120 ms each), and sends nothing once it is
    /// over, so it exits within it.
    #[test]
    fn the_end_of_a_loop_whose_owner_is_gone_sends_again_only_within_the_grace() {
        use std::cell::Cell;
        let poll = Duration::from_millis(200);
        let (clock, sends) = (Cell::new(Duration::ZERO), Cell::new(0));
        let mut sleep = |wait: Duration| clock.set(clock.get() + wait);
        let mut refused = || {
            sends.set(sends.get() + 1);
            clock.set(clock.get() + Duration::from_millis(120));
            false
        };
        // No deadline: every try, a poll apart.
        assert_eq!(
            send_within(15, poll, &|| None, &mut sleep, &mut refused),
            Sent::NotTaken
        );
        assert_eq!(sends.get(), 15);
        assert_eq!(clock.get(), 14 * poll + 15 * Duration::from_millis(120));
        // Taken at the third: no more.
        sends.set(0);
        let mut third = || {
            sends.set(sends.get() + 1);
            sends.get() == 3
        };
        assert_eq!(
            send_within(15, poll, &|| None, &mut sleep, &mut third),
            Sent::Taken
        );
        assert_eq!(sends.get(), 3);

        // The owner gone: within the 500 ms left of the grace.
        let grace = Duration::from_millis(500);
        let (start, waits) = (Duration::from_secs(10), Cell::new(Vec::new()));
        clock.set(start);
        sends.set(0);
        let left = || Some(grace.saturating_sub(clock.get() - start));
        let mut sleep = |wait: Duration| {
            waits.set([waits.take(), vec![wait]].concat());
            clock.set(clock.get() + wait);
        };
        let mut refused = || {
            sends.set(sends.get() + 1);
            clock.set(clock.get() + Duration::from_millis(120));
            false
        };
        assert_eq!(
            send_within(15, poll, &left, &mut sleep, &mut refused),
            Sent::NotTaken
        );
        // 120 + 200 + 120 + 60 (what was left): the third send is not made.
        assert_eq!(sends.get(), 2);
        assert_eq!(waits.take(), [poll, Duration::from_millis(60)]);
        assert_eq!(clock.get() - start, grace);
        // Over before the end begins: nothing is sent.
        sends.set(0);
        assert_eq!(
            send_within(15, poll, &|| Some(Duration::ZERO), &mut sleep, &mut refused),
            Sent::Late
        );
        assert_eq!(sends.get(), 0);
    }

    /// A supervisor whose owner is gone ends as soon as it holds no run
    /// after giving back what it could, whatever its headless jobs or a
    /// person's answers; the work it cannot give back is waited for only
    /// within the grace.
    #[test]
    fn a_supervisor_whose_owner_is_gone_waits_only_within_the_grace() {
        let grace = OWNER_GONE_GRACE;
        let ms = Duration::from_millis(1);
        assert!(owner_gone_ends(Duration::ZERO, grace, 0));
        assert!(!owner_gone_ends(Duration::ZERO, grace, 1));
        assert!(!owner_gone_ends(grace - ms, grace, 2));
        assert!(owner_gone_ends(grace, grace, 1));
        assert!(owner_gone_ends(grace + ms, grace, 1));
        // The end of the loop waits for what is left of the grace, and
        // for nothing once it is over.
        assert_eq!(owner_gone_left(Duration::ZERO, grace), grace);
        assert_eq!(owner_gone_left(grace - ms, grace), ms);
        assert_eq!(owner_gone_left(grace, grace), Duration::ZERO);
        assert_eq!(owner_gone_left(grace + ms, grace), Duration::ZERO);
    }

    /// The end of the loop joins a job that finished, looks again at one
    /// that runs until its deadline, and leaves it at the deadline; without
    /// a deadline it waits for the job to finish however long.
    #[test]
    fn a_job_is_waited_for_until_its_deadline_and_then_left() {
        let ms = Duration::from_millis(1);
        // 1 ms before the deadline, and at it (or past it: nothing left).
        assert_eq!(job_wait(false, Some(ms)), JobWait::Wait);
        assert_eq!(job_wait(false, Some(Duration::ZERO)), JobWait::Leave);
        assert_eq!(job_wait(true, Some(ms)), JobWait::Join);
        assert_eq!(job_wait(true, Some(Duration::ZERO)), JobWait::Join);
        // No deadline.
        assert_eq!(job_wait(false, None), JobWait::Wait);
        assert_eq!(job_wait(true, None), JobWait::Join);
    }

    /// A handoff request read again keeps, replaces or withdraws the
    /// handoff this process drains for; a pass drains on a stop, a stopped
    /// claim or a handoff.
    #[test]
    fn a_handoff_request_read_again_keeps_replaces_or_withdraws_the_drain() {
        assert_eq!(
            handoff_change("/bin/a", Some("/bin/a".to_owned())),
            HandoffChange::Same
        );
        assert_eq!(
            handoff_change("/bin/a", Some("/bin/b".to_owned())),
            HandoffChange::Replaced("/bin/b".to_owned())
        );
        assert_eq!(handoff_change("/bin/a", None), HandoffChange::Withdrawn);
        assert!(!drains(false, true, false));
        assert!(drains(true, true, false));
        assert!(drains(false, false, false));
        assert!(drains(false, true, true));
    }

    /// ADR-t3344-1: a landing is handed over only while a handoff is
    /// asked and no stop wins over it.
    #[test]
    fn only_a_handoff_without_a_stop_hands_landings_over() {
        assert!(hands_landings_over(false, true));
        assert!(!hands_landings_over(true, true));
        assert!(!hands_landings_over(false, false));
        assert!(!hands_landings_over(true, false));
    }

    /// A request withdrawn while the supervisor drains for a landing
    /// returns it to claims on the same pass, with the cleanup back to
    /// normal unless a stop keeps it ending; one replaced, at a pass or at
    /// a take that changed no row, goes on draining for the new binary
    /// without a claim between; the same binary asked again keeps the
    /// drain for the next pass's take.
    #[test]
    fn a_request_read_again_resumes_claims_or_drains_for_the_binary_asked() {
        let withdrawn = reread("/next/dagq", None, false);
        assert_eq!(withdrawn.change, HandoffChange::Withdrawn);
        assert_eq!(withdrawn.handoff, None);
        assert!(withdrawn.resume_cleanup);
        assert!(!drains(false, true, withdrawn.handoff.is_some()));
        // Claiming stopped by a provisioning failure still drains.
        assert!(drains(false, false, withdrawn.handoff.is_some()));
        let stopping = reread("/next/dagq", None, true);
        assert_eq!(stopping.handoff, None);
        assert!(!stopping.resume_cleanup);
        assert!(drains(true, true, stopping.handoff.is_some()));

        let replaced = reread("/next/dagq", Some("/replacement/dagq".to_owned()), false);
        assert_eq!(
            replaced.change,
            HandoffChange::Replaced("/replacement/dagq".to_owned())
        );
        assert_eq!(replaced.handoff.as_deref(), Some("/replacement/dagq"));
        assert!(!replaced.resume_cleanup);
        assert!(drains(false, true, replaced.handoff.is_some()));

        let same = reread("/next/dagq", Some("/next/dagq".to_owned()), false);
        assert_eq!(same.change, HandoffChange::Same);
        assert_eq!(same.handoff.as_deref(), Some("/next/dagq"));
        assert!(!same.resume_cleanup);
    }

    #[test]
    fn old_resume_snapshot_defaults_exit_typed_to_false() {
        let mut value = json!({
            "phase": "resume", "workspace": "w", "attempt": 1,
            "started_at": 0.0, "message": "resume", "message_sent_at": null,
            "not_ready_asked": false, "exit_requested": true,
            "exit_for_silence": false, "approved": true
        });
        for expected in [false, true] {
            let snapshot: Snapshot = serde_json::from_value(value.clone()).unwrap();
            assert!(
                matches!(snapshot, Snapshot::Resume { exit_typed, .. } if exit_typed == expected)
            );
            value["exit_typed"] = json!(true);
        }
    }
}
