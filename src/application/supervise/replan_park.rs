//! The park of a run whose task is being replanned (ADR-t1521-2 decisions
//! 1 and 2, docs/plans/task-replanning.md, "停止境界
//! と競合" and "snapshotと保存の順"): once an operation is past the pause's
//! fence (`pause_requested`), no next turn of the run is sent and its
//! wrapper takes none (the wrapper's own check of the guard); the
//! supervisor waits for the turn that runs to end, stops the run's
//! recorded wrapper and turn by their handle only past the turn's limit,
//! saves the run's work in a snapshot under the queue's `replans/`, and
//! records the snapshot and the park in one transaction, which gives the
//! run's slot back. Every step reads the operation again, so a supervisor
//! that restarts, adopts the run or takes it over after an exec goes on
//! from the step the queue records. A continuation or a withdrawal finds
//! the run's work as it was saved before the run resumes
//! ([`Supervisor::restore_parked`]).
//!
//! The same park takes the run of a held task (ADR-t1879-1 decision 7),
//! the hold as its other reason ([`ParkFor`]): the same fence, the same
//! wait for the turn that runs and stop of its writer, the same withheld
//! review and verification, and a settle that gives the slot back. A hold
//! changes no plan, so its park saves no snapshot and moves no operation:
//! the run waits for a session with its worktree as it is (`hold_parked`),
//! and the settle, which finds the hold still open and settling, records
//! that the hold settled. A process a turn left in the worktree keeps the
//! hold settling until it ends. A run at rest is not parked for a hold: the
//! store settles it in the transaction that let it rest (a lease given
//! back, a recovery, a status written with nobody leasing it), and
//! [`Supervisor::settle_resting_holds`] settles what that left.
//!
//! Past the fence no receipt is accepted, and a session waiting for an
//! answer is not reopened. A held run's turn that wrote a receipt (the
//! first session's, a revise's or a resume's) has it validated as usual,
//! but the next step (review, revise, landing) does not start; a run the
//! validation sent back to `needs_session` waits with that reason. The
//! return of a lease is not counted as its writer's end: a run whose
//! registered process has no recorded end (a resume given up) is taken by
//! the supervisor for the hold's park, which waits for the writer to end
//! before the run waits. A run whose replanning park ended `blocked` is
//! the replanning's: the hold takes no lease, and lets it wait once no
//! writer is in its registered processes or its worktree. A resume after
//! the release goes on in the same session from where it stopped.

use super::*;
use crate::domain::background_wrapper::{BackgroundHandle, is_background};
use crate::domain::recovery::run_processes;
use crate::domain::replan_snapshot::{
    self, MANIFEST, RestoreCheck, RestoreStart, SavedFile, SavedKind, SnapshotManifest,
    UntrackedFile, WorktreeState,
};
use crate::domain::run::run_workspaces;
use crate::domain::task_hold::TaskHold;
use crate::domain::task_replan::{ReplanMove, ReplanOperation, ReplanStatus};

/// Whether the last of `events`' `resume_started`, `resume_finished` and
/// `resume_skipped` is a `resume_started`: a resume that never ended.
fn mid_resume(events: &[RunEvent]) -> bool {
    events
        .iter()
        .rev()
        .find(|event| {
            [
                EventKind::ResumeStarted.as_str(),
                EventKind::ResumeFinished.as_str(),
                EventKind::ResumeSkipped.as_str(),
            ]
            .contains(&event.kind.as_str())
        })
        .is_some_and(|event| event.kind == EventKind::ResumeStarted.as_str())
}

/// What parks a run at its turn's end: a replanning past its pause's
/// fence, which also saves the run's work, or its task's hold while it
/// settles, which leaves the work in place.
#[derive(Debug, Clone)]
enum ParkFor {
    Replan(Box<ReplanOperation>),
    Hold(TaskHold),
}

impl ParkFor {
    /// Since when the run's writer is waited for (unix seconds).
    const fn since(&self) -> i64 {
        match self {
            Self::Replan(op) => op.updated_at,
            Self::Hold(hold) => hold.created_at,
        }
    }

    /// What parks it, for the logs.
    fn describe(&self) -> String {
        match self {
            Self::Replan(op) => format!("replan operation {}", op.id),
            Self::Hold(hold) => format!("the hold {} of task {}", hold.id, hold.task_id),
        }
    }

    /// How long the processes a run's turns left in its worktree are given
    /// to end at one look: a replanning's park, which blocks on them, waits
    /// [`LEFT_WRITER_GRACE`]; a hold's looks once and waits for them on the
    /// next looks, its run keeping its slot meanwhile, so that it settles
    /// only once no writer is left.
    const fn writer_grace(&self) -> Duration {
        match self {
            Self::Replan(_) => LEFT_WRITER_GRACE,
            Self::Hold(_) => Duration::ZERO,
        }
    }

    /// `workspace_closed`'s `by` for a session the park closes.
    const fn by(&self) -> &'static str {
        match self {
            Self::Replan(_) => "replan",
            Self::Hold(_) => "hold",
        }
    }

    /// The event a result the park joins and does not act on is recorded
    /// as, with the park's own part of its payload.
    fn withheld(&self) -> (EventKind, Value) {
        match self {
            Self::Replan(op) => (
                EventKind::ReplanResultWithheld,
                json!({"operation_id": op.id, "generation": op.generation}),
            ),
            Self::Hold(hold) => (EventKind::HoldResultWithheld, json!({"hold_id": hold.id})),
        }
    }
}

/// What a hold's park ends a run in once its writer ended
/// ([`Supervisor::park_held`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtEnd {
    /// A run whose step a hold reached in a slot (or whose supervisor died
    /// in it) waits for a session (`hold_parked`); one that stopped rests
    /// as it is.
    Park,
    /// The exit after a validation or a verdict: a run its validation sent
    /// back to a session, or one that stopped, rests as it is with its own
    /// reason; any other is parked.
    Rested,
    /// A run that rests already, nobody holding it: it rests as it is.
    Resting,
}

impl AtEnd {
    /// Whether a run in `status` rests as it is rather than being parked.
    const fn rests(self, status: RunStatus) -> bool {
        match status {
            RunStatus::Failed | RunStatus::Interrupted => true,
            RunStatus::NeedsSession => !matches!(self, Self::Park),
            RunStatus::AwaitingIntegration | RunStatus::Validating => {
                matches!(self, Self::Resting)
            }
            _ => false,
        }
    }
}

/// `payload` with the park's own part (`base`) in front.
fn withheld_payload(base: &Value, payload: Value) -> Value {
    let mut merged = base.clone();
    if let (Some(merged), Value::Object(payload)) = (merged.as_object_mut(), payload) {
        merged.extend(payload);
    }
    merged
}

/// The seconds past a turn's limit the supervisor still waits for the
/// run's writer to end by itself before it stops it: the wrapper stops a
/// turn at its limit and needs a moment to record it.
const STOP_GRACE_SECS: i64 = HEARTBEAT_TIMEOUT_SECS;

/// How long the processes a run's turns left in its worktree are given to
/// end once its wrapper and turns are gone, before the park is blocked.
const LEFT_WRITER_GRACE: Duration = Duration::from_secs(2);

/// Whether the wait for the writer of an operation that began quiescing at
/// `since` (unix seconds) is over at `now`: past the turn's limit
/// (`limit_secs`) and [`STOP_GRACE_SECS`].
pub(super) fn quiesce_overdue(since: i64, now: i64, limit_secs: i64) -> bool {
    now - since > limit_secs.max(0) + STOP_GRACE_SECS
}

/// What the park makes of the processes a run left in its worktree once
/// its sessions and turns are gone: `left` as listed (an error when they
/// could not be listed), `waited` since the first look. None left is
/// [`Quiet::Stopped`]; some left within `grace` is `None`, to look again;
/// some left at `grace` or later, or a list that cannot be read, is
/// [`Quiet::Stuck`].
pub(super) fn left_writers_verdict(
    left: std::result::Result<Vec<String>, String>,
    waited: Duration,
    grace: Duration,
) -> Option<Quiet> {
    match left {
        Err(error) => Some(Quiet::Stuck(format!(
            "the processes left in its worktree could not be listed: {error}"
        ))),
        Ok(left) if left.is_empty() => Some(Quiet::Stopped),
        Ok(left) if waited >= grace => Some(Quiet::Stuck(format!(
            "processes it left in its worktree still run: {}",
            left.join(", ")
        ))),
        Ok(_) => None,
    }
}

/// A park stopped because its supervisor no longer holds the run, or its
/// operation moved on (another supervisor settled it): no failure of the
/// park, so it is not recorded, and no ref or snapshot is changed.
#[derive(Debug)]
struct ParkLost(String);

impl std::fmt::Display for ParkLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParkLost {}

/// What the park records as `carried_answers`: the answers a park before
/// left owed and no turn ran since ([`RunHistory::owed_answers`]: the
/// continuation that was to give them was parked again first), then the
/// answer requests `run`'s session had been sent whose turn never ran
/// ([`crate::domain::turn::answers_not_started`] over its `turns/` and the
/// requests its turns ran, [`RunHistory::requests_run`]): one still
/// waiting, or one a wrapper took just before it met the fence or ran in a
/// turn whose resumed thread was gone. Each ask once, the oldest first.
fn owed_and_untaken(
    files: &dyn RunFiles,
    run: &TaskRun,
    events: &[RunEvent],
) -> Result<Vec<Value>> {
    let history = RunHistory::from_events(events);
    let mut carried = history.owed_answers();
    let Some(run_dir) = run.run_dir() else {
        return Ok(carried);
    };
    let listed = super::headless::listed_requests(files, Path::new(run_dir))?;
    for (seq, ask, prompt) in
        crate::domain::turn::answers_not_started(&listed, &history.requests_run())
    {
        if carried.iter().all(|owed| owed["ask_id"] != json!(ask)) {
            carried.push(json!({"ask_id": ask, "seq": seq, "prompt": prompt}));
        }
    }
    Ok(carried)
}

/// A restore stopped because its supervisor no longer holds the run (or
/// cannot tell that it does): no failure of the restored work, so it is
/// not recorded and the operation does not move.
#[derive(Debug)]
struct RestoreLost(String);

impl std::fmt::Display for RestoreLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RestoreLost {}

/// Where the wait for a run's writer stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Quiet {
    /// Its wrapper or a turn still runs: wait.
    Waiting,
    /// No writer is left.
    Stopped,
    /// A writer outlived the stop: why.
    Stuck(String),
}

impl Supervisor<'_> {
    /// Let a test pause this supervisor at `point` for `run`
    /// ([`Ports::pauses`]); nothing outside the tests.
    pub(super) fn pause_at(&self, point: PausePoint, run: &RunId) {
        if let Some(pauses) = &self.pauses {
            pauses(point, run);
        }
    }

    /// The open operation that parks `run` now, if any.
    fn parking_of(&self, run: &RunId) -> Result<Option<ReplanOperation>> {
        Ok(self
            .queue
            .replans_of_run(run)?
            .into_iter()
            .rev()
            .find(|op| op.status.parks()))
    }

    /// What parks `run` in its slot now: an operation that parks it, else
    /// the open hold of its task while it settles. A replanning that
    /// guards the task past its park (parked, planning) parks nothing
    /// more, and a hold under it waits for the run to rest.
    fn park_for(&self, run: &TaskRun) -> Result<Option<ParkFor>> {
        if let Some(op) = self.parking_of(run.id())? {
            return Ok(Some(ParkFor::Replan(Box::new(op))));
        }
        if self.queue.replan_guard(run.task_id())?.is_some() {
            return Ok(None);
        }
        Ok(self
            .queue
            .open_hold(run.task_id())?
            .filter(|hold| hold.settled_at.is_none())
            .map(ParkFor::Hold))
    }

    /// Park the run of `slot` when an operation or its task's hold parks it
    /// ([`ParkFor`]), before its phase goes on: `None` leaves the step to
    /// the phase. A job that runs off
    /// the loop (an e2e, a recovery round, a landing) ends first, its
    /// bounded end acted on as before up to the next look, which parks the
    /// run. A review's jobs and program that run end first, and none starts
    /// past the fence: a review with jobs left to start, or whose program
    /// just ended, is withheld (`replan_result_withheld`, or
    /// `hold_result_withheld` for a hold). A receipt's validation or an
    /// `already_satisfied` claim's verification that ends meanwhile is not
    /// acted on either: no ask opens and no task closes on it. For a hold,
    /// a receipt the turn wrote and its validation go on as before (the
    /// late receipt is validated): what they start next is fenced, and the
    /// park takes the run there. The exit after a validation or a verdict
    /// is the park's too, so no lease is given back while a writer is left
    /// in the worktree; a run its validation sent back to a session rests
    /// with that reason. A resume's rewritten receipt is judged once no
    /// writer is left.
    pub(super) fn park_step(&mut self, slot: &mut Slot) -> Result<Option<Step>> {
        // A failure of the park (a busy queue, a move another process made
        // first) is the park's, not the phase's: it is tried again on the
        // next look, the run keeping its slot.
        match self.try_park_step(slot) {
            Ok(step) => Ok(step),
            Err(error) => {
                warn!(run_id = %slot.run.id(), error = %format_args!("{error:#}"), "run {}: its park at its turn's end failed and is tried again: {error:#}", slot.run.id());
                Ok(Some(Step::Continue))
            }
        }
    }

    fn try_park_step(&mut self, slot: &mut Slot) -> Result<Option<Step>> {
        let Some(park) = self.park_for(&slot.run)? else {
            return Ok(None);
        };
        let held = matches!(park, ParkFor::Hold(_));
        let (withheld_kind, withheld_base) = park.withheld();
        let mut withheld = None;
        match &mut slot.phase {
            Phase::E2e(_) | Phase::Recovery(_) | Phase::Landing(_) => return Ok(None),
            Phase::Validating(..) if held => return Ok(None),
            Phase::Session(watch) if held && self.files.is_file(&watch.receipt_path) => {
                return Ok(None);
            }
            // A receipt a revise's or a resume's turn rewrote is judged by
            // its phase, as a first session's is.
            Phase::Revise(watch) if held && watch.receipt_rewritten(self, &slot.run) => {
                return Ok(None);
            }
            // Its verdict may let the run rest and give the lease back, so
            // it is judged only once no writer is left in the worktree.
            Phase::Resume(watch) if held && watch.receipt_rewritten(&*self.files) => {
                return Ok(match self.left_writers(&slot.run, park.writer_grace()) {
                    Quiet::Stopped => None,
                    _ => Some(Step::Continue),
                });
            }
            // The review's jobs that run end first and are reaped; none
            // that waits (for the limit, a provider, or as a run once more)
            // starts. A review whose jobs all ended is judged as before,
            // and its verdict starts nothing past the fence; one with jobs
            // left to start is withheld and parked as it stands.
            Phase::Review(watch) => {
                let run = slot.run.clone();
                self.reap_review_jobs(&run, watch)?;
                if watch.running() > 0 {
                    return Ok(Some(Step::Continue));
                }
                if watch
                    .jobs
                    .iter()
                    .all(|job| !matches!(job.state, ReviewJobState::Waiting(_)))
                {
                    return Ok(None);
                }
                let waiting = watch
                    .jobs
                    .iter()
                    .filter(|job| matches!(job.state, ReviewJobState::Waiting(_)))
                    .count();
                self.queue.record_runtime_event(
                    slot.run.id(),
                    withheld_kind,
                    withheld_payload(
                        &withheld_base,
                        json!({"what": "review", "attempt": watch.attempt, "waiting": waiting}),
                    ),
                )?;
                let held = Phase::ReviewHeld {
                    session: watch.session.take(),
                    retried: watch.retried,
                    providers: Vec::new(),
                };
                withheld = Some(held);
            }
            // The program that runs ends first and its end is recorded;
            // neither the next program nor the agents' review starts.
            Phase::ReviewPrograms(watch) => {
                let Some(end) = watch.job.poll_program(&*self.files)? else {
                    return Ok(Some(Step::Continue));
                };
                let ran = (
                    watch.job.started.elapsed().as_secs(),
                    watch.job.timeout.as_secs(),
                );
                let mut programs = watch.programs.take();
                let run = self.queue.run(slot.run.id())?;
                self.record_review_program_end(&run, &mut programs, &end, ran)?;
                self.queue.record_runtime_event(
                    slot.run.id(),
                    withheld_kind,
                    withheld_payload(
                        &withheld_base,
                        json!({"what": "review_programs", "attempt": programs.attempt,
                               "round": programs.round, "ran": programs.outcomes.len()}),
                    ),
                )?;
                let held = Phase::ReviewHeld {
                    session: programs.session.take(),
                    retried: programs.retried,
                    providers: Vec::new(),
                };
                withheld = Some(held);
            }
            Phase::Satisfied(watch) => {
                if !watch.handle.as_ref().is_none_or(|h| h.is_finished()) {
                    return Ok(Some(Step::Continue));
                }
                if let Some(handle) = watch.handle.take() {
                    let checked = handle.join().map_or_else(
                        |_| json!({"error": "the verification thread panicked"}),
                        |verification| match verification {
                            Ok(verification) => json!({"head": verification.head,
                                "passed": verification.passed()}),
                            Err(error) => json!({"error": format!("{error:#}")}),
                        },
                    );
                    self.queue.record_runtime_event(
                        slot.run.id(),
                        withheld_kind,
                        withheld_payload(
                            &withheld_base,
                            json!({"what": "satisfied_verification", "ask_id": watch.ask,
                                   "result": checked}),
                        ),
                    )?;
                }
            }
            Phase::Validating(handle, _) => {
                if !handle.as_ref().is_none_or(|h| h.is_finished()) {
                    return Ok(Some(Step::Continue));
                }
                if let Some(handle) = handle.take() {
                    let checked = handle.join().map_or_else(
                        |_| json!({"error": "the validation thread panicked"}),
                        |validation| match validation {
                            Ok(validation) => json!({"accepted": validation.accepted,
                                "result_commit": validation.result_commit,
                                "reason": validation.reason}),
                            Err(error) => json!({"error": format!("{error:#}")}),
                        },
                    );
                    self.queue.record_runtime_event(
                        slot.run.id(),
                        withheld_kind,
                        withheld_payload(
                            &withheld_base,
                            json!({"what": "validation", "result": checked}),
                        ),
                    )?;
                }
            }
            _ => {}
        }
        // The review left as it stands waits for the park in place of its
        // jobs; were the park not to happen after all, it starts again.
        if let Some(withheld) = withheld {
            slot.transition(withheld, withheld_kind.as_str(), &*self.queue);
        }
        // An exit after a validation or a verdict whose run rests already
        // (its validation sent it back) keeps that rest and its reason.
        let rested = matches!(slot.phase, Phase::Exiting(_));
        let run = self.queue.run(slot.run.id())?;
        let token = self.registration.token.clone();
        let parked = match &park {
            ParkFor::Replan(op) => self.park(op, &run, &token)?,
            ParkFor::Hold(hold) => {
                let at_end = if rested { AtEnd::Rested } else { AtEnd::Park };
                self.park_held(hold, &run, &token, at_end)?
            }
        };
        match parked {
            Some(parked) => Ok(Some(Step::Done(Box::new(parked)))),
            None => Ok(Some(Step::Continue)),
        }
    }

    /// Take the run of a held task through the park: the wait for its
    /// writer at its turn's end and the settle that records the hold's wait
    /// ([`RunTransitions::park_for_hold`]), as far as each can go now. The
    /// run once parked; `None` while it waits, once the park is no longer
    /// this supervisor's, or once the hold was released or settled
    /// meanwhile (the run then goes on by its own paths). A writer that
    /// cannot be stopped is warned of and waited for again on the next
    /// look: the hold saves nothing, so no step fails. A run that rests as
    /// it is ([`AtEnd::rests`]) keeps its own reason: its lease is given
    /// back with the hold settled in the same transaction, the writer found
    /// ended ([`RunTransitions::release_for_hold`]).
    fn park_held(
        &mut self,
        hold: &TaskHold,
        run: &TaskRun,
        token: &LeaseToken,
        at_end: AtEnd,
    ) -> Result<Option<TaskRun>> {
        let park = ParkFor::Hold(hold.clone());
        self.pause_at(PausePoint::ParkLook, run.id());
        let quiet = match self.still_settling(run, token, &park) {
            Ok(()) => self.quiesce(&park, run, token),
            Err(error) => Err(error),
        };
        match quiet {
            Err(error) if error.chain().any(|cause| cause.is::<ParkLost>()) => {
                info!(run_id = %run.id(), "run {}: its park stopped: {error:#}", run.id());
                return Ok(None);
            }
            Err(error) => return Err(error),
            Ok(Quiet::Waiting) => return Ok(None),
            Ok(Quiet::Stuck(why)) => {
                warn!(run_id = %run.id(), "run {}: its park for the hold {} of task {} waits: {why}", run.id(), hold.id, hold.task_id);
                return Ok(None);
            }
            Ok(Quiet::Stopped) => {}
        }
        let run = &self.queue.run(run.id())?;
        if at_end.rests(run.status()) {
            self.queue.release_for_hold(hold.id, run.id(), token)?;
            info!(run_id = %run.id(), "run {}: the hold {} of task {} settled as it rests ({})", run.id(), hold.id, hold.task_id, run.status().as_str());
            return Ok(Some(self.queue.run(run.id())?));
        }
        let carried = owed_and_untaken(&*self.files, run, &self.queue.run_events(run.id())?)?;
        let mut payload = json!({"hold_id": hold.id});
        if !carried.is_empty() {
            payload["carried_answers"] = json!(carried);
        }
        let reason = format!(
            "task {} was held ({}); the run waits for the release with its session and work in place",
            hold.task_id, hold.reason
        );
        let parked = self
            .queue
            .park_for_hold(hold.id, run.id(), token, &reason, payload)?;
        match &parked {
            Some(_) => {
                info!(run_id = %run.id(), "run {} parked for the hold {} of task {}; its slot is free", run.id(), hold.id, hold.task_id);
            }
            None => {
                info!(run_id = %run.id(), "run {}: the hold {} of task {} was released or settled before its park", run.id(), hold.id, hold.task_id);
            }
        }
        Ok(parked)
    }

    /// Settle the holds whose task's latest run rests with nobody holding
    /// it (waiting for a resume, its landing turn, a person's answer, or
    /// stopped; one a replanning parked included): the hold's wait is the
    /// run's rest, recorded once ([`RunTransitions::settle_hold_at_rest`]).
    /// The store settles a hold in the transaction that lets its run rest
    /// when no writer of it may run; this pass takes what that did not
    /// reach, each through the same park as a run in a slot
    /// ([`Self::park_held`]): the wait for its writer (its session's
    /// wrapper and turn, and what a turn left in the worktree), then the
    /// settle. A run in one of this supervisor's slots is its park's.
    ///
    /// The runs it takes ([`RunTransitions::lease_for_hold_park`]): one that
    /// rests with nobody holding it while its writer may still run (a
    /// resume given up whose wrapper still runs its turn, a replanning's
    /// park that could not stop the writer), which rests as it is; and one
    /// resting under the lease of a supervisor that died (its resume's or
    /// its recovery round's, whose wrapper or job ended unwatched), which
    /// nothing else takes while its task is held: no adopter takes a resume
    /// whose session ended, nor a triage or a resume a hold refuses. A run
    /// waiting to land or validating under a stale lease is an adopter's,
    /// and goes through the park in its slot.
    pub(super) fn settle_resting_holds(&mut self) -> Result<()> {
        for hold in self.queue.settling_holds()? {
            let Some(run) = self.queue.latest_run_of(hold.task_id)? else {
                continue;
            };
            if self.in_slot(run.id()) {
                continue;
            }
            if let Err(error) = self.park_left_held(&hold, &run) {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its park for the hold {} failed and is tried again: {error:#}", run.id(), hold.id);
            }
        }
        Ok(())
    }

    /// Whether a writer of `run` may be left: a session process it
    /// registered lives, or a process runs in its worktree
    /// ([`Self::left_writers`], looked at once). A list that cannot be read
    /// is a writer left.
    fn writer_left(&self, run: &TaskRun) -> bool {
        let registered = self.queue.processes(run.id()).map_or(true, |processes| {
            processes
                .iter()
                .any(|p| p.exited_at.is_none() && self.processes.alive(p.pid))
        });
        registered || !matches!(self.left_writers(run, Duration::ZERO), Quiet::Stopped)
    }

    /// Whether this supervisor holds a run out of its slots for its hold's
    /// park that waits for the run's writer ([`Self::park_left_held`]): it
    /// is waited for like a job, so the park ends before the loop does. A
    /// queue that cannot be read waits for nothing.
    pub(super) fn hold_parks_going(&self) -> bool {
        let token = &self.registration.token;
        self.queue.settling_holds().is_ok_and(|holds| {
            holds.iter().any(|hold| {
                self.queue
                    .latest_run_of(hold.task_id)
                    .ok()
                    .flatten()
                    .is_some_and(|run| {
                        !self.in_slot(run.id())
                            && self
                                .queue
                                .run_lease(run.id())
                                .ok()
                                .flatten()
                                .is_some_and(|lease| lease.token == *token)
                    })
            })
        })
    }

    /// Take over for its hold's park a run resting `needs_session`,
    /// `failed` or `interrupted` under a lease nobody renews (or one this
    /// supervisor took for the park on an earlier look), and park it as far
    /// as it can go now. A park that waits for the run's writer keeps the
    /// lease, which this supervisor's heartbeat renews, so that no release
    /// settles the hold before the writer ended; the next look goes on.
    fn park_left_held(&mut self, hold: &TaskHold, run: &TaskRun) -> Result<()> {
        let stopped_or_waiting = matches!(
            run.status(),
            RunStatus::NeedsSession | RunStatus::Failed | RunStatus::Interrupted
        );
        let resting = stopped_or_waiting
            || matches!(
                run.status(),
                RunStatus::AwaitingIntegration | RunStatus::Validating
            );
        if !resting {
            return Ok(());
        }
        let token = self.registration.token.clone();
        let lease = self.queue.run_lease(run.id())?;
        // A run a replanning guards (its park could not stop the writer and
        // ended blocked) is the replanning's: the hold takes no lease of it
        // and settles once no writer of it is left.
        if self.queue.replan_guard(run.task_id())?.is_some() {
            if lease.is_none() && !self.writer_left(run) {
                self.queue
                    .settle_hold_at_rest(hold.id, run.id(), run.status())?;
            }
            return Ok(());
        }
        if let Some(lease) = lease
            && lease.token != token
            && !(stopped_or_waiting && self.lease_stale(&lease, self.generators.clock.now()))
        {
            return Ok(());
        }
        let Some(run) = self.queue.lease_for_hold_park(run.id(), &token, hold.id)? else {
            return Ok(());
        };
        // A resume a supervisor died in the middle of is parked as a slot's
        // run is; a run that rested by its own step keeps that rest.
        let at_end = if mid_resume(&self.queue.run_events(run.id())?) {
            AtEnd::Park
        } else {
            AtEnd::Resting
        };
        self.park_held(hold, &run, &token, at_end)?;
        Ok(())
    }

    /// Park the runs no supervisor holds whose operation parks them: one
    /// waiting for a person (its landing's or its `already_satisfied`
    /// claim's answer, the latter resting `validating`), for its resume or
    /// for its triage. A run another process holds is its holder's to park,
    /// one whose holder died its adopter's; a run that landed before its
    /// park ends the operation (`continuing`: it is not needed). The park
    /// is one supervisor's from the stop to the settle: it leases the run
    /// first ([`RunTransitions::lease_for_park`], refused while another
    /// holds it or once the operation no longer parks), and a park that
    /// does not settle in this pass gives the lease back for the next look.
    pub(super) fn park_idle_runs(&mut self) -> Result<()> {
        let token = self.registration.token.clone();
        for op in self.queue.parking_replans()? {
            if self.in_slot(&op.run_id) {
                continue;
            }
            let run = self.queue.run(&op.run_id)?;
            match run.status() {
                RunStatus::Integrated | RunStatus::Succeeded => {
                    if self.queue.run_lease(&op.run_id)?.is_none() {
                        self.move_op(
                            &op,
                            ReplanStatus::Continuing,
                            format!(
                                "run {} landed before it could be parked; the replanning is not needed",
                                run.id()
                            ),
                        );
                    }
                    continue;
                }
                RunStatus::NeedsSession
                | RunStatus::AwaitingIntegration
                | RunStatus::Validating
                | RunStatus::Failed
                | RunStatus::Interrupted => {}
                _ => continue,
            }
            let Some(run) = self
                .queue
                .lease_for_park(run.id(), &token, op.id, op.generation)?
            else {
                continue;
            };
            let parked = self.park(&op, &run, &token);
            if !matches!(parked, Ok(Some(_))) {
                // Not settled now: the next look leases it again, this
                // supervisor's or another's.
                if let Err(error) = self.queue.release_lease(run.id(), &token) {
                    info!(run_id = %run.id(), "run {}: the park's lease was not given back: {error:#}", run.id());
                }
            }
            if let Err(error) = parked {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {} could not be parked for replan operation {}: {error:#}", run.id(), op.id);
            }
        }
        Ok(())
    }

    /// Take `op`'s run through the park: the fence to `quiescing`, the
    /// wait for its writer, `snapshotting`, the save and the settle, as
    /// far as each can go now. `token` is the lease this supervisor holds
    /// the run with (in a slot, or leased for the park), which the settle
    /// gives back. The run once settled (`parked`, or `blocked` with its
    /// work left in place); `None` while it waits, or once the park is no
    /// longer this supervisor's ([`ParkLost`]): nothing is recorded then.
    fn park(
        &mut self,
        op: &ReplanOperation,
        run: &TaskRun,
        token: &LeaseToken,
    ) -> Result<Option<TaskRun>> {
        let mut op = op.clone();
        if op.status == ReplanStatus::PauseRequested {
            let Some(moved) = self.move_op(
                &op,
                ReplanStatus::Quiescing,
                format!(
                    "waiting for the writer of run {} to end at its turn's end",
                    run.id()
                ),
            ) else {
                return Ok(None);
            };
            info!(run_id = %run.id(), "run {}: replan operation {} parks it; no next turn is sent", run.id(), op.id);
            op = moved;
        }
        if !matches!(
            op.status,
            ReplanStatus::Quiescing | ReplanStatus::Snapshotting
        ) {
            return Ok(None);
        }
        // Looked at again at `snapshotting` too (a park taken up after a
        // restart): nothing is saved while a writer runs. A park that is no
        // longer this supervisor's (another took the run over while this
        // one stood still) looks at no session and stops none.
        self.pause_at(PausePoint::ParkLook, run.id());
        let park = ParkFor::Replan(Box::new(op.clone()));
        let quiet = match self.still_parking(run, token, &op) {
            Ok(()) => self.quiesce(&park, run, token),
            Err(error) => Err(error),
        };
        let quiet = match quiet {
            Err(error) if error.chain().any(|cause| cause.is::<ParkLost>()) => {
                info!(run_id = %run.id(), "run {}: its park stopped: {error:#}", run.id());
                return Ok(None);
            }
            quiet => quiet?,
        };
        match quiet {
            Quiet::Waiting => return Ok(None),
            Quiet::Stuck(why) => {
                if let Err(error) = self.still_parking(run, token, &op) {
                    info!(run_id = %run.id(), "run {}: its park stopped: {error:#}", run.id());
                    return Ok(None);
                }
                return self.settle(&op, run, token, Err(why)).map(Some);
            }
            Quiet::Stopped => {}
        }
        if op.status == ReplanStatus::Quiescing {
            let Some(moved) = self.move_op(
                &op,
                ReplanStatus::Snapshotting,
                format!("the writer of run {} ended; saving its work", run.id()),
            ) else {
                return Ok(None);
            };
            op = moved;
        }
        let saved = match self.save_snapshot(&op, run, token) {
            Err(error) if error.chain().any(|cause| cause.is::<ParkLost>()) => {
                info!(run_id = %run.id(), "run {}: its park stopped: {error:#}", run.id());
                return Ok(None);
            }
            saved => {
                saved.map_err(|error| format!("the snapshot of run {} failed: {error:#}", run.id()))
            }
        };
        self.settle(&op, run, token, saved).map(Some)
    }

    /// Move `op` to `to` for `reason`; `None` (warned) when another
    /// process moved it first or the move failed: the next look reads it
    /// again.
    fn move_op(
        &mut self,
        op: &ReplanOperation,
        to: ReplanStatus,
        reason: String,
    ) -> Option<ReplanOperation> {
        let step = ReplanMove {
            to,
            reason,
            snapshot_id: None,
            diagnosis: None,
        };
        match self.queue.move_replan(op.id, op.generation, &step) {
            Ok(moved) => Some(moved),
            Err(error) => {
                warn!(run_id = %op.run_id, error = %format_args!("{error:#}"), "replan operation {} could not move to {}: {error:#}", op.id, to.as_str());
                None
            }
        }
    }

    /// Record the end of the park: `parked` with the snapshot `saved`, or
    /// `blocked` with why it failed, the run parked and its slot given back
    /// when this supervisor holds it ([`RunTransitions::settle_replan`]).
    fn settle(
        &mut self,
        op: &ReplanOperation,
        run: &TaskRun,
        token: &LeaseToken,
        saved: std::result::Result<String, String>,
    ) -> Result<TaskRun> {
        let carried = owed_and_untaken(&*self.files, run, &self.queue.run_events(run.id())?)?;
        let (step, mut payload) = match &saved {
            Ok(snapshot) => (
                ReplanMove {
                    to: ReplanStatus::Parked,
                    reason: format!(
                        "parked for replan operation {}: its work is saved in snapshot {snapshot}",
                        op.id
                    ),
                    snapshot_id: Some(snapshot.clone()),
                    diagnosis: None,
                },
                json!({"operation_id": op.id, "generation": op.generation, "snapshot_id": snapshot}),
            ),
            Err(why) => (
                ReplanMove {
                    to: ReplanStatus::Blocked,
                    reason: format!(
                        "replan operation {} could not park run {}: {why}; its work is left as it is",
                        op.id,
                        run.id()
                    ),
                    snapshot_id: None,
                    diagnosis: None,
                },
                json!({"operation_id": op.id, "generation": op.generation, "blocked": why}),
            ),
        };
        if !carried.is_empty() {
            payload["carried_answers"] = json!(carried);
        }
        let (_, parked) =
            self.queue
                .settle_replan(op.id, op.generation, &step, Some(token), payload)?;
        match &saved {
            Ok(snapshot) => {
                info!(run_id = %run.id(), "run {} parked for replan operation {} with snapshot {snapshot}; its slot is free", run.id(), op.id);
            }
            Err(why) => {
                warn!(run_id = %run.id(), "run {}: replan operation {} is blocked: {why}", run.id(), op.id);
            }
        }
        Ok(parked)
    }

    /// Wait for `run`'s writer to end: each session it opened that is not
    /// closed, its wrapper and the turn the wrapper recorded. A session
    /// that ended is closed (`workspace_closed`); past the turn's limit one
    /// still running is stopped by its handle, which names the process and
    /// its start, so no other run's process is touched; one that outlives
    /// the stop is [`Quiet::Stuck`], and so is a process the run left in
    /// its worktree ([`Self::left_writers`]). Each stop and close runs only
    /// while the park is still `token`'s and `op` is where it was
    /// ([`Self::still_parking`]): no session another supervisor opened
    /// since is touched.
    fn quiesce(&mut self, park: &ParkFor, run: &TaskRun, token: &LeaseToken) -> Result<Quiet> {
        let now = self.generators.clock.now();
        let limit = self.stall.turn_limits().limit_secs;
        let overdue = quiesce_overdue(park.since(), now, limit);
        let events = self.queue.run_events(run.id())?;
        let processes = self.queue.processes(run.id())?;
        let mut running = Vec::new();
        for session in run_workspaces(run, &events)
            .into_iter()
            .filter(|session| !session.closed && is_background(&session.workspace_id))
        {
            let handle = session.workspace_id;
            let pid = BackgroundHandle::parse(&handle).map(|parsed| parsed.pid);
            let wrapper = processes
                .iter()
                .find(|p| p.role == "wrapper" && Some(p.pid) == pid);
            let wrapper_runs = match wrapper {
                Some(wrapper) => wrapper.exited_at.is_none() && !wrapper_dead(self, wrapper, now),
                // Launched and not registered yet: its process says.
                None => self.run_session_open(&handle)?,
            };
            let turn = left_turn(&*self.queue, &*self.processes, run.id(), &handle);
            if wrapper_runs || turn.is_some() {
                running.push((handle, turn, session.resume_attempt));
            } else {
                self.still_settling(run, token, park)?;
                self.close_parked_session(run, &handle, session.resume_attempt, park.by())?;
            }
        }
        if running.is_empty() {
            return Ok(self.left_writers(run, park.writer_grace()));
        }
        if !overdue {
            return Ok(Quiet::Waiting);
        }
        for (handle, turn, resume) in running {
            self.still_settling(run, token, park)?;
            warn!(run_id = %run.id(), "run {}: its session {handle} still runs past the turn's limit; stopping it for {}", run.id(), park.describe());
            let stopped =
                stop_session(self.sessions, &handle, StopRoute::Replan).and_then(|()| match turn {
                    Some(turn) => stop_left_turn(&*self.processes, &turn),
                    None => Ok(()),
                });
            if let Err(error) = stopped {
                return Ok(Quiet::Stuck(format!(
                    "its session {handle} could not be stopped: {error:#}"
                )));
            }
            if let Some(turn) = left_turn(&*self.queue, &*self.processes, run.id(), &handle) {
                return Ok(Quiet::Stuck(format!(
                    "the turn {} of its session {handle} still runs after the stop",
                    turn.pid
                )));
            }
            self.still_settling(run, token, park)?;
            self.close_parked_session(run, &handle, resume, park.by())?;
        }
        Ok(self.left_writers(run, park.writer_grace()))
    }

    /// Once `run`'s sessions and turns are gone, whether a process of the
    /// run still runs in its worktree: one a turn detached from its group
    /// (`nohup … &`), which the wrapper leaves running on purpose
    /// ([`run_processes`] picks the run's own by their working directory,
    /// never the supervisor's). Those are given [`LEFT_WRITER_GRACE`] to
    /// end; one still running then, or processes that cannot be listed,
    /// is [`Quiet::Stuck`]: the work is not saved while something may
    /// still write it, and the operation is blocked for a person or the
    /// recovery job to stop it.
    fn left_writers(&self, run: &TaskRun, grace: Duration) -> Quiet {
        let Some(worktree) = run.worktree_path() else {
            return Quiet::Stopped;
        };
        let since = self.generators.clock.monotonic();
        loop {
            let left = self
                .processes
                .list()
                .map(|all| {
                    run_processes(&all, Path::new(worktree), None, None, std::process::id())
                        .into_iter()
                        .map(|p| format!("{} ({})", p.pid, p.command))
                        .collect()
                })
                .map_err(|error| format!("{error:#}"));
            let waited = self
                .generators
                .clock
                .monotonic()
                .saturating_duration_since(since);
            if let Some(quiet) = left_writers_verdict(left, waited, grace) {
                return quiet;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Close the session `handle` of `run` whose writer ended (stopping the
    /// handle, which names the process and its start, is a no-op for one
    /// that is gone), recorded as `workspace_closed`.
    fn close_parked_session(
        &mut self,
        run: &TaskRun,
        handle: &str,
        resume: Option<u64>,
        by: &str,
    ) -> Result<()> {
        stop_session(self.sessions, handle, StopRoute::Replan)?;
        self.queue.record_workspace_closed(
            run.id(),
            handle,
            json!({"resume_attempt": resume, "by": by}),
        )
    }

    /// Save `run`'s work for `op` ([`replan_snapshot`]): a snapshot of this
    /// generation renamed into place before (a restart after the rename)
    /// is read back and kept; a staging directory left half made is a copy
    /// and is made again from the run's own work, which nothing touched.
    /// Its ID.
    fn save_snapshot(
        &mut self,
        op: &ReplanOperation,
        run: &TaskRun,
        token: &LeaseToken,
    ) -> Result<String> {
        let id = replan_snapshot::snapshot_id(op.id, op.generation);
        let root = self.layout.replans_dir.clone();
        let dir = replan_snapshot::snapshot_dir(&root, &id);
        if self.files.is_dir(&dir) {
            let manifest: SnapshotManifest =
                serde_json::from_slice(&self.files.read(&dir.join(MANIFEST))?)
                    .context("read the snapshot's manifest")?;
            self.check_snapshot(&dir, &manifest)?;
            return Ok(id);
        }
        let staging = replan_snapshot::staging_dir(&root, &id, self.layout.pid);
        if self.files.is_dir(&staging) {
            self.files.remove_dir_all(&staging)?;
        }
        for sub in ["untracked", "logs", "logs/turns"] {
            self.files.create_dir_all(&staging.join(sub))?;
        }
        let worktree = PathBuf::from(run.worktree_path().context("the run has no worktree")?);
        ensure!(
            self.files.is_dir(&worktree),
            "the worktree {} is missing",
            worktree.display()
        );
        let repository = crate::application::run_git::for_run(&*self.repository, run)?;
        replan_snapshot::check_git_operation(repository.git_operation(&worktree)?)?;
        let head = repository.head(&worktree)?;
        let pinned = replan_snapshot::pinned_ref(op.id);
        // Only the park's owner pins the head: a supervisor that lost the
        // park, or found it settled by another, moves no ref.
        self.still_parking(run, token, op)?;
        repository.update_ref(&pinned, head.as_str())?;
        let mut saving = Saving {
            files: &*self.files,
            staging: &staging,
            saved: Vec::new(),
            total: 0,
        };
        let index = repository.worktree_diff(&worktree, true)?;
        saving.save(
            "index.patch",
            SavedKind::Index,
            "git diff --cached --binary",
            &index,
        )?;
        let unstaged = repository.worktree_diff(&worktree, false)?;
        saving.save(
            "worktree.patch",
            SavedKind::Worktree,
            "git diff --binary",
            &unstaged,
        )?;
        for (at, path) in repository
            .untracked_paths(&worktree)?
            .into_iter()
            .enumerate()
        {
            let source = worktree.join(&path);
            let contents = read_regular(&*self.files, &source, &path, saving.total)?;
            saving.save(
                &format!("untracked/{at}"),
                SavedKind::Untracked,
                &path,
                &contents,
            )?;
            if self.files.executable(&source)?
                && let Some(saved) = saving.saved.last_mut()
            {
                saved.executable = true;
            }
        }
        if let Some(receipt) = run.receipt_path().map(PathBuf::from)
            && self.files.exists(&receipt)
        {
            let source = receipt.to_string_lossy().into_owned();
            let contents = read_regular(&*self.files, &receipt, &source, saving.total)?;
            saving.save("receipt.json", SavedKind::Receipt, &source, &contents)?;
        }
        let mut excluded = vec![
            "files Git ignores in the worktree (build output, made again)".to_owned(),
            format!("{RUN_RUNNER_FILE} (a copy of the runtime binary)"),
        ];
        if let Some(run_dir) = run.run_dir().map(PathBuf::from) {
            self.save_run_files(&mut saving, &mut excluded, &run_dir, "logs", saved_log)?;
            // The session's requests (taken or not) and its turns' output:
            // what was asked of it and what it said.
            let turns = crate::domain::turn::turns_dir(&run_dir);
            if self.files.is_dir(&turns) {
                self.save_run_files(&mut saving, &mut excluded, &turns, "logs/turns", |_| true)?;
            }
        }
        // The run's records: its turns, sessions and providers, asks and
        // answers, reviews and validations.
        let events = serde_json::to_vec_pretty(&self.queue.run_events(run.id())?)?;
        saving.save(
            "logs/events.json",
            SavedKind::Log,
            "the run's events",
            &events,
        )?;
        let conditions = serde_json::to_vec_pretty(&json!({
            "operation_id": op.id,
            "task_version": op.task_version,
            "goal_acceptance_version": op.goal_acceptance_version,
            "conditions": op.conditions,
            "diagnosis": op.diagnosis,
        }))?;
        saving.save(
            "conditions.json",
            SavedKind::Conditions,
            "the operation's conditions",
            &conditions,
        )?;
        let files = saving.saved;
        let empty = replan_snapshot::sha256(b"");
        let manifest = SnapshotManifest {
            id: id.clone(),
            operation: op.id,
            generation: op.generation,
            run_id: run.id().clone(),
            task_id: run.task_id(),
            base_commit: run.base_commit().to_string(),
            head: head.to_string(),
            branch: run.branch().map(str::to_owned),
            pinned_ref: pinned,
            worktree: worktree.to_string_lossy().into_owned(),
            dirty: files.iter().any(|file| match file.kind {
                SavedKind::Index | SavedKind::Worktree => file.sha256 != empty,
                SavedKind::Untracked => true,
                _ => false,
            }),
            files,
            excluded,
            conditions_version: op.conditions.version(),
            remaining: op.conditions.acceptance.clone(),
            saved_at: self.generators.clock.now(),
        };
        self.files.write(
            &staging.join(MANIFEST),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
        self.check_snapshot(&staging, &manifest)?;
        if let Err(error) = self.still_parking(run, token, op) {
            let _ = self.files.remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = self.files.rename(&staging, &dir) {
            // Another process saved the same generation first: its
            // snapshot holds, once it reads back whole.
            if self.files.is_dir(&dir) {
                let manifest: SnapshotManifest =
                    serde_json::from_slice(&self.files.read(&dir.join(MANIFEST))?)
                        .context("read the snapshot's manifest")?;
                self.check_snapshot(&dir, &manifest)?;
                let _ = self.files.remove_dir_all(&staging);
                return Ok(id);
            }
            return Err(error.into());
        }
        Ok(id)
    }

    /// Save the regular files of `dir` whose name `keep` picks as
    /// `<prefix>/<name>`, each read whole without following a link: a link
    /// or a special file is not followed but named in `excluded`, and a
    /// file past the snapshot's limits ([`replan_snapshot::check_entry`])
    /// fails the save. Subdirectories are not entered.
    fn save_run_files(
        &self,
        saving: &mut Saving<'_>,
        excluded: &mut Vec<String>,
        dir: &Path,
        prefix: &str,
        keep: fn(&str) -> bool,
    ) -> Result<()> {
        let mut paths: Vec<PathBuf> = self
            .files
            .read_dir(dir)?
            .into_iter()
            .filter(|path| path.file_name().and_then(|n| n.to_str()).is_some_and(keep))
            .collect();
        paths.sort();
        for path in paths {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();
            match self.files.entry_kind(&path)? {
                crate::application::EntryKind::File => {}
                crate::application::EntryKind::Dir => continue,
                kind => {
                    excluded.push(format!("{prefix}/{name}: {}, not followed", kind.as_str()));
                    continue;
                }
            }
            // Saved whole or not at all: a file past the limits fails the
            // save, and the park is blocked with the work left in place.
            let source = path.to_string_lossy().into_owned();
            let contents = read_regular(&*self.files, &path, &source, saving.total)?;
            saving.save(
                &format!("{prefix}/{name}"),
                SavedKind::Log,
                &source,
                &contents,
            )?;
        }
        Ok(())
    }

    /// Read back every file `manifest` names in `dir` and check each is
    /// as saved ([`replan_snapshot::check_saved`]).
    fn check_snapshot(&self, dir: &Path, manifest: &SnapshotManifest) -> Result<()> {
        let mut saved = Vec::new();
        for file in &manifest.files {
            if let Ok(contents) = self.files.read(&dir.join(&file.name)) {
                saved.push((
                    file.name.clone(),
                    contents.len() as u64,
                    replan_snapshot::sha256(&contents),
                ));
            }
        }
        replan_snapshot::check_saved(manifest, &saved)?;
        Ok(())
    }

    /// Before the resume of a run a replanning parked, once its operation
    /// ended in a continuation or a withdrawal: compare the run's worktree
    /// with the snapshot and make it again from the snapshot when it is
    /// gone (`replan_restored`). Whether the run may resume now: `false`
    /// while another supervisor restores it. The restore is one owner's
    /// from its start to its recorded result: the run is leased to this
    /// supervisor first ([`RunTransitions::begin_restore`]), each step that
    /// changes the scratch or the worktree checks the lease is still its
    /// own, and the result is recorded only while it is
    /// ([`RunTransitions::finish_restore`]), so a supervisor that lost the
    /// run (to another that took its lease over as stale) changes neither
    /// the worktree's path nor the operation. A restore that fails (a
    /// worktree that changed since, which is neither overwritten nor thrown
    /// away, or a snapshot that cannot be read or applied) records
    /// `replan_restore_failed` and moves the operation back to `blocked`
    /// with why: it guards the task again, the run does not resume, and
    /// `status` and `doctor` list it among the replans' problems. Nothing
    /// for a run no replanning parked, or one restored already.
    pub(super) fn restore_parked(&mut self, run: &TaskRun) -> Result<bool> {
        let ops = self.queue.replans_of_run(run.id())?;
        let Some(op) = ops.iter().rev().find(|op| op.snapshot_id.is_some()) else {
            return Ok(true);
        };
        if !op.status.reopens() {
            return Ok(true);
        }
        let token = self.registration.token.clone();
        match self
            .queue
            .begin_restore(run.id(), &token, op.id, op.generation)?
        {
            RestoreStart::Done => return Ok(true),
            RestoreStart::Elsewhere => {
                info!(run_id = %run.id(), "run {}: another supervisor holds it for the restore of replan operation {}; it is not resumed here", run.id(), op.id);
                return Ok(false);
            }
            RestoreStart::Owned => {}
        }
        let snapshot = op.snapshot_id.clone().unwrap_or_default();
        let restored = self.restore_from(run, &snapshot, &token);
        // A restore whose hold is gone is no failure of its work: nothing is
        // recorded, and the lease is given back if it still is this one's.
        if let Err(error) = &restored
            && error.chain().any(|cause| cause.is::<RestoreLost>())
        {
            if let Err(release) = self.queue.release_lease(run.id(), &token) {
                info!(run_id = %run.id(), "run {}: the restore's lease was not given back: {release:#}", run.id());
            }
            info!(run_id = %run.id(), "run {}: its restore stopped: {error:#}", run.id());
            return Ok(false);
        }
        let result = match &restored {
            Ok((action, manifest)) => Ok(
                json!({"operation_id": op.id, "snapshot_id": snapshot, "action": action,
                       "head": manifest.head, "dirty": manifest.dirty}),
            ),
            Err(error) => Err(format!(
                "the work of run {} could not be restored from snapshot {snapshot}: {error:#}; the snapshot and the worktree are kept and the run does not resume",
                run.id()
            )),
        };
        let failed = result.as_ref().err().cloned();
        if !self
            .queue
            .finish_restore(run.id(), &token, op.id, op.generation, result)?
        {
            info!(run_id = %run.id(), "run {}: the restore's result was not recorded: it lost the run or replan operation {} moved meanwhile", run.id(), op.id);
            return Ok(false);
        }
        match (restored, failed) {
            (Ok((action, _)), _) => {
                info!(run_id = %run.id(), "run {}: the work saved in snapshot {snapshot} is in its worktree ({action}); it resumes", run.id());
                Ok(true)
            }
            (Err(_), Some(why)) => bail!("{why}"),
            (Err(error), None) => Err(error),
        }
    }

    /// Fail with [`ParkLost`] unless this supervisor still holds `run` with
    /// `token` and `op` is still where this park took it (its status and
    /// generation): checked before the park pins the head, puts its
    /// snapshot in place and settles a failure.
    /// [`Self::still_parking`] for `park`: for a hold, this supervisor
    /// still holds `run` with `token` and the hold is still open and
    /// settling.
    fn still_settling(&self, run: &TaskRun, token: &LeaseToken, park: &ParkFor) -> Result<()> {
        let hold = match park {
            ParkFor::Replan(op) => return self.still_parking(run, token, op),
            ParkFor::Hold(hold) => hold,
        };
        let held = self.queue.holds_lease(run.id(), token).map_err(|error| {
            ParkLost(format!(
                "the hold of run {} for its park could not be read: {error:#}",
                run.id()
            ))
        })?;
        if !held {
            return Err(ParkLost(format!(
                "this supervisor no longer holds run {} for its park",
                run.id()
            ))
            .into());
        }
        let open = self.queue.open_hold(hold.task_id).map_err(|error| {
            ParkLost(format!("the hold {} could not be read: {error:#}", hold.id))
        })?;
        if open.is_none_or(|open| open.id != hold.id || open.settled_at.is_some()) {
            return Err(ParkLost(format!(
                "the hold {} was released or settled while run {} was parked",
                hold.id,
                run.id()
            ))
            .into());
        }
        Ok(())
    }

    fn still_parking(&self, run: &TaskRun, token: &LeaseToken, op: &ReplanOperation) -> Result<()> {
        let held = self.queue.holds_lease(run.id(), token).map_err(|error| {
            ParkLost(format!(
                "the hold of run {} for its park could not be read: {error:#}",
                run.id()
            ))
        })?;
        if !held {
            return Err(ParkLost(format!(
                "this supervisor no longer holds run {} for its park",
                run.id()
            ))
            .into());
        }
        let now = self
            .queue
            .replans_of_run(run.id())
            .map_err(|error| {
                ParkLost(format!(
                    "replan operation {} could not be read: {error:#}",
                    op.id
                ))
            })?
            .into_iter()
            .find(|found| found.id == op.id);
        if now.is_none_or(|now| now.status != op.status || now.generation != op.generation) {
            return Err(ParkLost(format!(
                "replan operation {} moved while run {} was parked",
                op.id,
                run.id()
            ))
            .into());
        }
        Ok(())
    }

    /// Fail with [`RestoreLost`] unless this supervisor still holds `run`
    /// with `token` for its restore (the heartbeat keeps the hold fresh):
    /// checked before each step that changes the scratch worktree or the
    /// worktree's path. A hold that cannot be read is lost too: the step
    /// does not run, and the restore is tried again later.
    fn still_restoring(&self, run: &TaskRun, token: &LeaseToken) -> Result<()> {
        match self.queue.holds_lease(run.id(), token) {
            Ok(true) => Ok(()),
            Ok(false) => Err(RestoreLost(format!(
                "this supervisor no longer holds run {} for its restore; another took it over",
                run.id()
            ))
            .into()),
            Err(error) => Err(RestoreLost(format!(
                "the hold of run {} for its restore could not be read: {error:#}",
                run.id()
            ))
            .into()),
        }
    }

    /// Bring `run`'s worktree to the work snapshot `snapshot` saved, as the
    /// restore `token` owns: as it is when it matches, made again when it
    /// is gone; what was done and the snapshot's manifest. A worktree that
    /// changed since is an error, and nothing in it is touched.
    fn restore_from(
        &mut self,
        run: &TaskRun,
        snapshot: &str,
        token: &LeaseToken,
    ) -> Result<(&'static str, SnapshotManifest)> {
        let dir = replan_snapshot::snapshot_dir(&self.layout.replans_dir, snapshot);
        let manifest: SnapshotManifest =
            serde_json::from_slice(&self.files.read(&dir.join(MANIFEST))?)
                .with_context(|| format!("read the manifest of snapshot {snapshot}"))?;
        // Before its files are read back by their names.
        manifest.check_paths()?;
        self.check_snapshot(&dir, &manifest)?;
        // The run's path as the queue finds it now (a moved queue), not
        // the one the snapshot was saved from.
        let worktree = PathBuf::from(run.worktree_path().context("the run has no worktree")?);
        let current = if self.files.is_dir(&worktree) {
            Some(self.worktree_state(run, &worktree)?)
        } else {
            None
        };
        let action = match replan_snapshot::check_restore(&manifest, current.as_ref()) {
            RestoreCheck::Intact => "intact",
            RestoreCheck::Missing => {
                self.recreate_worktree(run, &worktree, &dir, &manifest, token)?;
                let state = self.worktree_state(run, &worktree)?;
                if let RestoreCheck::Diverged(why) =
                    replan_snapshot::check_restore(&manifest, Some(&state))
                {
                    bail!("the worktree made again is not as saved: {why}");
                }
                "recreated"
            }
            RestoreCheck::Diverged(why) => bail!("the worktree changed since ({why})"),
        };
        Ok((action, manifest))
    }

    /// The worktree `worktree` of `run` as a snapshot compares it.
    fn worktree_state(&self, run: &TaskRun, worktree: &Path) -> Result<WorktreeState> {
        let repository = crate::application::run_git::for_run(&*self.repository, run)?;
        let mut untracked = Vec::new();
        let mut total = 0;
        for path in repository.untracked_paths(worktree)? {
            let file = worktree.join(&path);
            let contents = read_regular(&*self.files, &file, &path, total)?;
            total += contents.len() as u64;
            untracked.push(UntrackedFile {
                executable: self.files.executable(&file)?,
                sha256: replan_snapshot::sha256(&contents),
                path,
            });
        }
        untracked.sort();
        Ok(WorktreeState {
            head: repository.head(worktree)?.to_string(),
            index: replan_snapshot::sha256(&repository.worktree_diff(worktree, true)?),
            worktree: replan_snapshot::sha256(&repository.worktree_diff(worktree, false)?),
            untracked,
        })
    }

    /// Make `run`'s worktree again from the snapshot in `dir`: its branch
    /// at the pinned head, the staged and unstaged diffs and the untracked
    /// files with their execute bits. The work is put together in a
    /// scratch worktree beside it ([`replan_snapshot::scratch_worktree`]),
    /// compared with the snapshot, and only then moved to the worktree's
    /// path, so a restore stopped half way leaves nothing at that path: the
    /// next attempt discards the scratch worktree and starts again from
    /// the snapshot. A manifest that names a path outside the scratch
    /// worktree or the snapshot's directory
    /// ([`SnapshotManifest::check_paths`]) is refused before anything is
    /// made, and each step that changes the scratch or moves it into place
    /// runs only while the restore is still `token`'s.
    fn recreate_worktree(
        &self,
        run: &TaskRun,
        worktree: &Path,
        dir: &Path,
        manifest: &SnapshotManifest,
        token: &LeaseToken,
    ) -> Result<()> {
        // Nothing is joined to the scratch worktree or the snapshot's
        // directory before the manifest's paths are known to stay inside.
        manifest.check_paths()?;
        let read = |kind: SavedKind| -> Result<Vec<u8>> {
            match manifest.files.iter().find(|file| file.kind == kind) {
                Some(file) => Ok(self.files.read(&dir.join(&file.name))?),
                None => Ok(Vec::new()),
            }
        };
        let repository = crate::application::run_git::for_run(&*self.repository, run)?;
        let scratch = replan_snapshot::scratch_worktree(worktree);
        let branch = manifest
            .branch
            .as_deref()
            .context("the snapshot names no branch")?;
        self.still_restoring(run, token)?;
        repository.discard_worktree(&scratch, branch)?;
        self.still_restoring(run, token)?;
        // From the head the manifest recorded: the pinned ref only keeps
        // it reachable, so no later move of the ref changes the restore.
        repository.restore_worktree(
            &scratch,
            branch,
            &manifest.head,
            &read(SavedKind::Index)?,
            &read(SavedKind::Worktree)?,
        )?;
        for file in manifest
            .files
            .iter()
            .filter(|file| file.kind == SavedKind::Untracked)
        {
            self.still_restoring(run, token)?;
            let to = scratch.join(&file.source);
            if let Some(parent) = to.parent() {
                self.files.create_dir_all(parent)?;
            }
            self.files.copy(&dir.join(&file.name), &to)?;
            self.files.set_executable(&to, file.executable)?;
        }
        let state = self.worktree_state(run, &scratch)?;
        if let RestoreCheck::Diverged(why) = replan_snapshot::check_restore(manifest, Some(&state))
        {
            bail!(
                "the worktree made again in {} is not as saved: {why}",
                scratch.display()
            );
        }
        self.still_restoring(run, token)?;
        repository.move_worktree(&scratch, worktree)
    }
}

/// The bytes of `path` (named `source`) in `files`, which must be a
/// regular file that fits the snapshot's limits with `total` saved before
/// it: its size is checked before it is read, so one too large fails the
/// save whole rather than being read in part.
fn read_regular(files: &dyn RunFiles, path: &Path, source: &str, total: u64) -> Result<Vec<u8>> {
    let kind = files.entry_kind(path)?;
    let size = if kind == crate::application::EntryKind::File {
        files.size(path)?
    } else {
        0
    };
    replan_snapshot::check_entry(
        source,
        kind == crate::application::EntryKind::File,
        size,
        total,
    )?;
    Ok(files.read(path)?)
}

/// The files of a snapshot being saved in its staging directory, and the
/// bytes they hold together.
struct Saving<'a> {
    files: &'a dyn RunFiles,
    staging: &'a Path,
    saved: Vec<SavedFile>,
    total: u64,
}

impl Saving<'_> {
    /// Save `contents` (from `source`) as `name`, within the snapshot's
    /// limits ([`replan_snapshot::check_entry`]).
    fn save(&mut self, name: &str, kind: SavedKind, source: &str, contents: &[u8]) -> Result<()> {
        replan_snapshot::check_entry(source, true, contents.len() as u64, self.total)?;
        self.total += contents.len() as u64;
        self.files.write(&self.staging.join(name), contents)?;
        self.saved.push(SavedFile::of(name, kind, source, contents));
        Ok(())
    }
}

/// Whether a file of a run directory named `name` is one a snapshot keeps
/// as the run's material: a log, a prompt or request the run was sent, a
/// review's or a validation's record.
fn saved_log(name: &str) -> bool {
    name.ends_with(".log")
        || name.ends_with(".out")
        || name.ends_with(".err")
        || name == "prompt.txt"
        || name.starts_with("review")
        || name.starts_with("resume-")
        || name.starts_with("revise-")
        || name.starts_with("conflict-")
        || name.ends_with("known-failures.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_for_the_writer_ends_past_the_turns_limit_and_its_grace() {
        assert!(!quiesce_overdue(100, 100, 60));
        assert!(!quiesce_overdue(100, 100 + 60 + STOP_GRACE_SECS, 60));
        assert!(quiesce_overdue(100, 101 + 60 + STOP_GRACE_SECS, 60));
        assert!(quiesce_overdue(100, 101 + STOP_GRACE_SECS, -5));
    }

    #[test]
    fn a_holds_park_looks_at_the_processes_left_once_and_waits_for_them_again() {
        let left = || Ok(vec!["9 (sleep)".to_owned()]);
        assert!(matches!(
            left_writers_verdict(left(), Duration::ZERO, Duration::ZERO),
            Some(Quiet::Stuck(_))
        ));
        assert_eq!(
            left_writers_verdict(Ok(Vec::new()), Duration::ZERO, Duration::ZERO),
            Some(Quiet::Stopped)
        );
    }

    #[test]
    fn the_processes_left_in_the_worktree_are_waited_for_up_to_the_grace() {
        let grace = Duration::from_secs(2);
        let left = || Ok(vec!["42 (sleep 120)".to_owned()]);
        assert_eq!(
            left_writers_verdict(Ok(Vec::new()), grace * 10, grace),
            Some(Quiet::Stopped)
        );
        assert_eq!(left_writers_verdict(left(), Duration::ZERO, grace), None);
        assert_eq!(
            left_writers_verdict(left(), grace - Duration::from_millis(1), grace),
            None
        );
        let stuck = left_writers_verdict(left(), grace, grace);
        assert!(
            matches!(&stuck, Some(Quiet::Stuck(why)) if why.contains("42 (sleep 120)")),
            "{stuck:?}"
        );
        let unlisted = left_writers_verdict(Err("ps failed".into()), Duration::ZERO, grace);
        assert!(
            matches!(&unlisted, Some(Quiet::Stuck(why)) if why.contains("ps failed")),
            "{unlisted:?}"
        );
    }

    /// A file of the run's material past the snapshot's limits fails the
    /// save by its size, before any of it is read (the store's reads are
    /// bounded here below its size), and is never cut; one that fits is
    /// read whole.
    #[test]
    fn a_file_past_the_snapshots_limits_fails_the_save_before_it_is_read() {
        use crate::application::memory_files::MemoryFiles;
        let files = MemoryFiles::bounded(4);
        let log = Path::new("/run/huge.log");
        files.put(log, SystemTime::UNIX_EPOCH, "0123456789");
        let refused = read_regular(
            &files,
            log,
            "huge.log",
            replan_snapshot::MAX_TOTAL_BYTES - 5,
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains("huge.log"), "{refused}");
        assert!(refused.contains("bytes it may"), "{refused}");
        let small = Path::new("/run/small.log");
        files.put(small, SystemTime::UNIX_EPOCH, "0123");
        assert_eq!(
            read_regular(
                &files,
                small,
                "small.log",
                replan_snapshot::MAX_TOTAL_BYTES - 4
            )
            .unwrap(),
            b"0123"
        );
        let missing = read_regular(&files, Path::new("/run/gone.log"), "gone.log", 0);
        assert!(missing.is_err());
    }

    #[test]
    fn a_snapshot_keeps_the_runs_logs_and_records_but_not_its_binary() {
        for name in [
            "session.log",
            "session-resume-1.log",
            "review-1.out",
            "review-1.err",
            "review.md",
            "prompt.txt",
            "resume-1.txt",
            "revise-2.txt",
            "known-failures.json",
        ] {
            assert!(saved_log(name), "{name}");
        }
        for name in [
            RUN_RUNNER_FILE,
            "receipt.json",
            "handoff.json",
            "turns.lock",
        ] {
            assert!(!saved_log(name), "{name}");
        }
    }
}
