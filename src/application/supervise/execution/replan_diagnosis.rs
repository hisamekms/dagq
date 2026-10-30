//! The runtime's detection of a long-running task and the diagnosis of
//! its replanning operation (ADR-t1521-1 decisions 1 to 4,
//! docs/plans/task-replanning.md, "入口と診断" and "回数・抑制・上限と観測").
//! Each pass of the loop, with `[replan] enabled`:
//!
//! - the latest run of each task in progress is looked at once the queue
//!   moved on: a task whose window of reviews makes it a candidate
//!   ([`replan_detect::detect`]) gets one operation of origin `runtime`,
//!   whose evidence is the window's events. A task with an open operation
//!   gets none, so a restart or a second supervisor detects nothing twice.
//! - one operation at a time, queue-wide, whatever its origin (a person's
//!   request, a worker's proposal, the detection), goes from `candidate` to
//!   `diagnosing` and its read-only diagnosis job starts on the recovery
//!   job's route, outside the run slots. The run goes on meanwhile: the
//!   diagnosis stops no turn. The claim, the queue-wide count of the jobs
//!   that run and the job's start, naming this supervisor as its owner,
//!   are one transaction of the queue, and the provider starts only after
//!   it: of supervisors that start at once, one does.
//! - the job's verdict is checked again before it is applied: the
//!   operation still `diagnosing` under the generation the job started on,
//!   the task still in progress with the run its latest, not landing. Then
//!   [`replan_detect::decide`] goes on (`continuing`), parks the run for
//!   the replanning (`pause_requested`, which the park takes up), or asks
//!   a person in a `decide` ask whose answer (`continue` or `replan`) the
//!   next pass applies. A job that fails is started again, up to
//!   [`replan_detect::MAX_DIAGNOSIS_ATTEMPTS`] jobs, then a person is asked.
//! - a job fails when it does not start, gives no verdict, or gives one
//!   whose apply ends in an error (the operation gone, a read or a write of
//!   the queue failing, the move's or the ask's included; a premise that
//!   moved on, which the queue refuses as a [`crate::domain::DomainError`]
//!   and the queue service carries as one, discards the verdict instead):
//!   each is recorded as `replan_diagnosis_failed`,
//!   the last with its verdict, which ends the job's start and counts
//!   toward the limit once per start, however many records name it
//!   ([`replan_detect::failed_jobs`]: two supervisors that fail on the
//!   same kept verdict fail one job), so that an error that persists ends
//!   in the person's ask rather than in jobs without end. A move of a
//!   verdict that committed applies it, whatever fails after it (closing
//!   the operation's asks), and fails no job
//!   ([`replan_detect::MoveResult::settles`]). The failure of an apply is
//!   written with a claim of the operation under the job's generation, the
//!   guard of every move and ask of the verdict, so that of two
//!   supervisors that take up the same verdict a hold kept, an apply and a
//!   failure are not both settled: the one the queue takes first wins, and
//!   a failure refused is no failure. The ask a park refused at the
//!   lineage's limit makes claims it under the job's generation as well
//!   ([`replan_detect::after_park_refused`]). A failure a hold refused
//!   (as the refusal itself says, through the queue service too) keeps the
//!   verdict for the release, with what its job read, on its first write
//!   and on a write again alike. A verdict kept for a hold whose apply
//!   fails after the release is recorded so once, and that record settles
//!   it. A verdict the queue
//!   settled (applied, asked about, kept or discarded) whose
//!   `replan_diagnosed` then cannot be written fails nothing: only the
//!   record is written again ([`replan_detect::verdict_end`]). A record of
//!   any of them that cannot be written is kept and written again at the
//!   next passes, the operation's next job and kept verdict waiting for it
//!   (meanwhile the queue counts the job as running for the other
//!   supervisors); one of an operation that is gone only ends the job's
//!   start ([`replan_detect::failed_apply`]).
//!
//! A run that lands while its operation is diagnosed closes it as
//! `continuing`; one that is landing waits, and the diagnosis is taken up
//! again if it comes back. A job of a supervisor that is gone (or of this
//! one before an exec) is started again from the queue's records.
//!
//! A task's hold comes before the diagnosis's start and the apply of its
//! results (ADR-t1879-1 decision 6): no job starts, no ask opens and no
//! verdict or answer moves the operation while the task is held, the
//! queue refusing each in its transaction ([`DiagnosisPremise`] includes
//! the hold). A verdict that reaches a held task is kept as
//! `replan_diagnosed` ([`replan_detect::HELD_OUTCOME`]) with what it read,
//! and an answer stays on its ask; once the hold is released, the next
//! pass applies each on the same path, its premise checked again, and a
//! verdict whose premise no longer holds is discarded and diagnosed again.

use super::*;
use crate::application::prompt::{DiagnosisMaterial, TRIAGE_ACCESS, replan_diagnosis_prompt};
use crate::domain::replan_detect::{
    self, AskWhy, DECIDED_BY_PERSON, DECIDED_BY_RUNTIME, DIAGNOSIS_ASKER, DIAGNOSIS_OPTIONS,
    DecideFacts, Decided, Detection, DiagnosisVerdict, JobOwner, JobPlace, JobRecord, OwnerSeen,
    PlanningPayload,
};
use crate::domain::task_hold::HoldGuard;
use crate::domain::task_replan::{
    Diagnosis, DiagnosisChoice, DiagnosisPremise, GuardedStep, NewReplan, ReplanMove,
    ReplanOperation, ReplanOrigin, ReplanStatus,
};
use crate::domain::{Ask, AskConfidence, ReplanId};

/// The events the detection counts ([`replan_detect::window`]): a task is
/// looked at again only once one of them comes.
const DETECTED_ON: [EventKind; 3] = [
    EventKind::ReviewFinished,
    EventKind::ConcernDecided,
    EventKind::RecoveryRequested,
];

/// The label of the diagnosis job's `headless_jobs` row and of its actor.
pub(super) const DIAGNOSIS_LABEL: &str = "replan_diagnosis";

/// The diagnosis job running now: one at a time, queue-wide.
pub(super) struct DiagnosisJob {
    read: DiagnosisRead,
    switchable: bool,
    job: HeadlessJob,
}

/// What a diagnosis job read as it started, which its verdict rests on:
/// kept with the verdict (`read` of its `replan_diagnosed`) when a hold
/// keeps it, so that the release applies it on the same premise.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct DiagnosisRead {
    operation: ReplanId,
    task: TaskId,
    run: RunId,
    /// The generation its claim moved the operation to.
    generation: i64,
    attempt: usize,
    /// The run's status as it started.
    run_status: RunStatus,
    /// The run's HEAD as it started, when its worktree could be read.
    head: Option<String>,
    window_end: EventId,
    evidence: Vec<EventId>,
}

/// Where a diagnosis job of an operation stands, from the queue's records.
enum InFlight {
    /// None started, or the last one ended: the next one is this attempt,
    /// after this many jobs that failed (a verdict discarded because the
    /// run moved on uses up none).
    Next(usize, usize),
    /// A job another supervisor still alive runs.
    Elsewhere,
}

/// The next diagnosis job of `op` from `events` (its task's): the attempt
/// after the last one started, or none while its owner, among `live`
/// (the owners of a job the queue records as running that still run it:
/// [`replan_detect::split_owners`]), runs it. A job of a supervisor that
/// is gone or of this process before an exec is started again as the
/// next attempt.
fn diagnosis_in_flight(op: ReplanId, events: &[RunEvent], live: &[JobOwner]) -> InFlight {
    let started: Vec<&RunEvent> = events
        .iter()
        .filter(|e| e.kind == EventKind::ReplanDiagnosisStarted && of_operation(e, op))
        .collect();
    let failed = replan_detect::failed_jobs(events, op);
    let Some(last) = started.last() else {
        return InFlight::Next(1, failed);
    };
    // Its end is tied to it: an ask's own record, or another attempt's
    // end, ends it not.
    let start = PlanningPayload::of(last);
    let ended = events.iter().any(|e| {
        e.id > last.id
            && of_operation(e, op)
            && matches!(
                e.kind.known().and_then(|kind| JobRecord::of(kind, &e.payload)),
                Some(JobRecord::Ended(end)) if end.ends(&start)
            )
    });
    let elsewhere = start.owner().is_some_and(|owner| live.contains(&owner));
    if !ended && elsewhere {
        InFlight::Elsewhere
    } else {
        InFlight::Next(started.len() + 1, failed)
    }
}

/// A record of a diagnosis job's end that could not be written: its
/// `replan_diagnosis_failed` (it did not start, gave no verdict or its
/// verdict's apply failed, [`replan_detect::FailedApply::Kept`]), or the
/// `replan_diagnosed` of a verdict the queue settled already (applied,
/// asked about, kept for a hold or discarded,
/// [`replan_detect::VerdictEnd::RecordKept`]). Kept by the supervisor and
/// written again at each pass, at the request of `requester` (the job)
/// when the first write was.
pub(super) struct UnrecordedRecord {
    run: RunId,
    operation: ReplanId,
    kind: EventKind,
    payload: Value,
    requester: Option<ActorContext>,
    /// The guard of a failed apply's record, written again under it.
    guard: Option<FailureGuard>,
}

/// The guard of the `replan_diagnosis_failed` of a verdict's apply that
/// failed ([`replan_detect::FailedApply`]): written with a claim of the
/// operation under the generation of the job that `read` is of, as every
/// move and ask of the verdict is
/// ([`crate::application::RunTransitions::fail_replan_diagnosis`]); `read`
/// is also what a hold that refuses the record keeps the verdict with.
/// `kept` is whether the verdict was one a hold kept, which any supervisor
/// applies after the release.
#[derive(Debug, Clone)]
struct FailureGuard {
    read: DiagnosisRead,
    kept: bool,
}

/// The error of a verdict's apply whose operation is no longer in the
/// queue ([`replan_detect::FailedApply::Ended`]).
#[derive(Debug)]
struct OperationGone(ReplanId);

impl std::fmt::Display for OperationGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "replan operation {} is gone", self.0)
    }
}

impl std::error::Error for OperationGone {}

/// A move of an operation out of its diagnosis whose transaction committed
/// ([`Supervisor::move_diagnosed`]), and how what followed it went.
struct Moved {
    /// Reading and closing the operation's open diagnosis asks.
    followed: Result<()>,
}

impl Moved {
    /// Note on the `replan_diagnosed` (`payload`) of `job`'s verdict what
    /// failed after its move committed: the verdict is applied all the
    /// same.
    fn note(&self, job: &DiagnosisRead, payload: &mut Value) {
        if let Err(error) = &self.followed {
            info!(run_id = %job.run, "replan operation {}: the verdict of diagnosis job {} is applied; what followed its move failed: {error:#}", job.operation, job.attempt);
            payload["after_move_error"] = json!(format!("{error:#}"));
        }
    }
}

/// What a verdict's move `moved` came to ([`replan_detect::MoveResult`]):
/// committed, refused by the queue ([`refused`]) or failed before it
/// committed.
fn move_result(moved: &Result<Moved>) -> replan_detect::MoveResult {
    match moved {
        Ok(moved) => replan_detect::MoveResult::Committed {
            followed: moved.followed.is_ok(),
        },
        Err(error) if refused(error) => replan_detect::MoveResult::Refused,
        Err(_) => replan_detect::MoveResult::Failed,
    }
}

/// Whether `error` of a verdict's move or ask is the queue's refusal of it
/// (a [`crate::domain::DomainError`]: its premise or the operation moved
/// on, or a hold came), on the queue or through the queue service
/// ([`crate::application::supervisor_service::rejected`]), after which the
/// verdict is discarded or kept, and not a read or a write that failed,
/// which fails the job.
fn refused(error: &anyhow::Error) -> bool {
    crate::application::supervisor_service::rejected(error)
}

/// How the write of a failed apply's `replan_diagnosis_failed` went, from
/// its result `written` alone ([`replan_detect::failed_apply`]), with the
/// error when it did not go and the hold when one refused it. A refusal is
/// the hold's when the refusal itself names the hold
/// ([`crate::application::supervisor_service::held_refusal`]), never by a
/// read after it: a hold released in between would make it read as another
/// settlement, and the verdict the hold keeps would be discarded.
fn failure_write(written: Result<()>) -> (replan_detect::FailureWrite, String, Option<HoldGuard>) {
    let Err(error) = written else {
        return (replan_detect::FailureWrite::Written, String::new(), None);
    };
    let why = format!("{error:#}");
    if !refused(&error) {
        return (replan_detect::FailureWrite::Failed, why, None);
    }
    match crate::application::supervisor_service::held_refusal(&error) {
        Some(hold) => (replan_detect::FailureWrite::Held, why, Some(hold)),
        None => (replan_detect::FailureWrite::Refused, why, None),
    }
}

/// `event`'s `operation_id` is `op`'s.
fn of_operation(event: &RunEvent, op: ReplanId) -> bool {
    PlanningPayload::of(event).is_of(op)
}

impl DiagnosisRead {
    /// What its verdict rests on: the run and its HEAD as it started and
    /// the events it read, `manual` whether the decision took the
    /// operation as a person's request and, for a park without a person,
    /// the lineage's limit of them.
    fn premise(&self, manual: bool, max_auto_parks: Option<usize>) -> DiagnosisPremise {
        DiagnosisPremise {
            run_status: self.run_status,
            read_until: Some(self.window_end),
            head: self.head.clone(),
            manual: Some(manual),
            max_auto_parks,
        }
    }
}

impl Supervisor<'_> {
    /// Read `[replan]` of `dagq.toml` again, as `[fresh_session]` is: a
    /// file that cannot be read keeps the settings in use, with one warning
    /// until the error changes.
    fn reread_replan(&mut self) {
        let Some(read) = self.replan_file.clone() else {
            return;
        };
        match read() {
            Ok(read) => {
                self.replan = read.unwrap_or_default();
                self.replan_error = None;
            }
            Err(error) => {
                let message = format!("{error:#}");
                if self.replan_error.as_ref() != Some(&message) {
                    warn!(error = %message, "[replan] of dagq.toml not read: {message}; keeping the settings in use");
                    self.replan_error = Some(message);
                }
            }
        }
    }

    /// Whether a diagnosis job runs now, which a loop with no slot waits
    /// for like a run.
    pub(super) const fn diagnosis_running(&self) -> bool {
        self.replan_diagnosis.is_some()
    }

    /// One pass of the detection and the diagnosis (the module's doc),
    /// detecting and starting only when `starting` (a supervisor at work).
    /// A person's answer is applied even once `[replan]` is turned off, and
    /// a job that runs is reaped first, whatever fails after: they were
    /// decided before, and a job left unreaped would hold a stopping loop.
    /// Whether this pass did anything (an operation opened or moved, a job
    /// started or ended, an ask), after which the loop takes one more
    /// pass: the park of a run just fenced starts on the next.
    pub(super) fn replan_diagnosis_pass(&mut self, starting: bool) -> Result<bool> {
        self.reread_replan();
        let mut done = self.record_unrecorded();
        done |= self.reap_replan_diagnosis()?;
        match self.apply_diagnosis_answers() {
            Ok(applied) => done |= applied,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the answers to the diagnoses could not all be applied: {error:#}");
            }
        }
        match self.apply_kept_diagnoses() {
            Ok(applied) => done |= applied,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the verdicts kept for holds could not all be applied: {error:#}");
            }
        }
        if starting && self.replan.enabled {
            done |= self.detect_replan_candidates()?;
            done |= self.start_replan_diagnosis()?;
        }
        Ok(done)
    }

    /// Open an operation of origin `runtime` on the latest run of each task
    /// in progress that is a candidate now, looking at a task again only
    /// once one of [`DETECTED_ON`] came since the last look (or `[replan]`
    /// changed).
    fn detect_replan_candidates(&mut self) -> Result<bool> {
        let mut opened = false;
        if self.replan_seen.0 != Some(self.replan) {
            self.replan_seen = (Some(self.replan), HashMap::new());
        }
        let kinds = DETECTED_ON.map(EventKind::as_str);
        let newest: HashMap<TaskId, EventId> = self
            .queue
            .latest_task_events(&kinds)?
            .into_iter()
            .filter_map(|e| e.task_id.map(|task| (task, e.id)))
            .collect();
        for run in self.queue.latest_runs_in_progress()? {
            let Some(&newest) = newest.get(&run.task_id()) else {
                continue;
            };
            if self
                .replan_seen
                .1
                .get(&run.task_id())
                .is_some_and(|seen| *seen >= newest)
            {
                continue;
            }
            if matches!(
                run.status(),
                RunStatus::Integrating | RunStatus::Integrated | RunStatus::Succeeded
            ) {
                continue;
            }
            let task = run.task_id();
            let ops: Vec<ReplanOperation> = self
                .queue
                .replan_lineage(task)?
                .into_iter()
                .filter(|op| op.task_id == task)
                .collect();
            let events = if ops.iter().any(|op| !op.status.is_closed()) {
                Vec::new()
            } else {
                self.queue.task_events(task)?
            };
            let detection = replan_detect::detect(&self.replan, &events, &ops);
            let Detection::Candidate { evidence, .. } = &detection else {
                // A window read while an operation is open is read again
                // once it closes, so the reviews meanwhile count then.
                if !matches!(detection, Detection::Open { .. }) {
                    self.replan_seen.1.insert(task, newest);
                }
                continue;
            };
            let new = NewReplan {
                run: run.id().clone(),
                reason: detection.reason().unwrap_or_default(),
                evidence: evidence.clone(),
            };
            // A candidate not recorded is looked at again on the next pass.
            match self.queue.detect_replan(&new, "supervisor") {
                Ok(op) => {
                    opened = true;
                    self.replan_seen.1.insert(task, newest);
                    info!(run_id = %run.id(), task_id = %task, "task {task}: {}; replan operation {} is a candidate for its diagnosis", new.reason, op.id);
                }
                Err(error) => {
                    warn!(run_id = %run.id(), task_id = %task, error = %format_args!("{error:#}"), "task {task} could not be recorded as a candidate for its diagnosis: {error:#}");
                }
            }
        }
        Ok(opened)
    }

    /// Close `op` (in `candidate` or `diagnosing`) as `continuing` for
    /// `reason`: its task or run went on without it. Its ask, if one is
    /// open, closes with it.
    fn close_undiagnosed(&mut self, op: &ReplanOperation, reason: &str) -> Result<()> {
        self.queue.move_replan(
            op.id,
            op.generation,
            &ReplanMove {
                to: ReplanStatus::Continuing,
                reason: reason.to_owned(),
                snapshot_id: None,
                diagnosis: None,
            },
        )?;
        info!(task_id = %op.task_id, "replan operation {} of task {} closed: {reason}", op.id, op.task_id);
        for ask in self.diagnosis_asks(op.task_id, Some(op.id))? {
            self.queue.close_ask(ask.id)?;
        }
        Ok(())
    }

    /// The unclosed diagnosis asks about `task`, of operation `op` when
    /// given (by the `replan_diagnosed` that opened each).
    fn diagnosis_asks(&self, task: TaskId, op: Option<ReplanId>) -> Result<Vec<Ask>> {
        let asks: Vec<Ask> = self
            .queue
            .asks(crate::application::AskQuery::default())?
            .into_iter()
            .filter(|ask| {
                ask.kind == AskKind::Decide
                    && ask.asked_by == DIAGNOSIS_ASKER
                    && ask.task_id == Some(task)
                    && ask.closed_at.is_none()
            })
            .collect();
        let Some(op) = op else {
            return Ok(asks);
        };
        let events = self.queue.task_events(task)?;
        Ok(asks
            .into_iter()
            .filter(|ask| {
                events.iter().any(|e| {
                    let read = PlanningPayload::of(e);
                    e.kind == EventKind::ReplanDiagnosed
                        && read.is_of(op)
                        && read.ask_id == Some(ask.id)
                })
            })
            .collect())
    }

    /// Start the diagnosis job of the first operation due, when none runs
    /// and the recovery job's route is open.
    /// Whether one started (or a person was asked).
    fn start_replan_diagnosis(&mut self) -> Result<bool> {
        if self.replan_diagnosis.is_some() {
            return Ok(false);
        }
        // Owners that cannot be told (the queue's service of another
        // binary answers no registration of theirs) start no job.
        let owners = match self.diagnosis_owners() {
            Ok(owners) => owners,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "no diagnosis job starts: the owners of the running ones are not read: {error:#}");
                return Ok(false);
            }
        };
        for op in self.queue.open_replan_operations()? {
            if !matches!(
                op.status,
                ReplanStatus::Candidate | ReplanStatus::Diagnosing
            ) {
                continue;
            }
            match self.try_start_diagnosis(&op, &owners) {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(error) => {
                    warn!(task_id = %op.task_id, error = %format_args!("{error:#}"), "the diagnosis of replan operation {} could not start: {error:#}", op.id);
                }
            }
        }
        Ok(false)
    }

    /// This supervisor as the owner of the diagnosis job it starts: its
    /// pid, its registration's token and its process's start.
    fn diagnosis_owner(&self) -> JobOwner {
        JobOwner {
            pid: self.layout.pid,
            token: Some(self.registration.token().as_str().to_owned()),
            process_start: self.processes.start_identity(self.layout.pid),
        }
    }

    /// The supervisors that own a diagnosis job the queue records as
    /// running, split into those that still run theirs and those gone
    /// ([`replan_detect::split_owners`]): this one among the gone, which
    /// runs none now (a job of it before an exec is gone), and each whose
    /// registration and process are gone. The queue's transaction of a
    /// start counts the jobs of the others
    /// ([`crate::application::RunTransitions::start_replan_diagnosis`]).
    fn diagnosis_owners(&self) -> Result<(Vec<JobOwner>, Vec<JobOwner>)> {
        let owners = self.queue.running_diagnosis_owners()?;
        let token = self.registration.token().as_str();
        let processes: Vec<(u32, bool, Option<String>)> = owners
            .iter()
            .map(|o| {
                let alive = self.processes.alive(o.owner.pid);
                let start = alive
                    .then(|| self.processes.start_identity(o.owner.pid))
                    .flatten();
                (o.owner.pid, alive, start)
            })
            .collect();
        Ok(replan_detect::split_owners(&owners, |owner| {
            let (pid_alive, pid_start) = processes
                .iter()
                .find(|(pid, ..)| *pid == owner.pid)
                .map_or((false, None), |(_, alive, start)| {
                    (*alive, start.as_deref())
                });
            OwnerSeen {
                me: (token, self.layout.pid),
                pid_alive,
                pid_start,
            }
        }))
    }

    /// Start `op`'s diagnosis job if it is due, of the owners of a job the
    /// queue records as running those in `owners.0` running theirs and
    /// those in `owners.1` none; whether one started (or a person was
    /// asked instead), so that no other starts in this pass.
    fn try_start_diagnosis(
        &mut self,
        op: &ReplanOperation,
        (live, gone): &(Vec<JobOwner>, Vec<JobOwner>),
    ) -> Result<bool> {
        let detail = self.queue.show(op.task_id)?;
        if detail.task.status() != TaskStatus::InProgress {
            let reason = format!(
                "task {} is {} before its diagnosis",
                op.task_id,
                detail.task.status().as_str()
            );
            self.close_undiagnosed(op, &reason)?;
            return Ok(false);
        }
        let Some(run) = detail.runs.last().cloned() else {
            return Ok(false);
        };
        // A later run takes the operation over at its claim; the landing
        // owns a run that is landing (the diagnosis waits for it).
        if *run.id() != op.run_id || run.status() == RunStatus::Integrating {
            return Ok(false);
        }
        if matches!(run.status(), RunStatus::Integrated | RunStatus::Succeeded) {
            self.close_undiagnosed(op, &format!("run {} landed before its diagnosis", run.id()))?;
            return Ok(false);
        }
        // A held task gets no job and no ask until it is released; the
        // claim below reads the hold again in its transaction.
        if self.held_out(op.task_id, Some(run.id()), GuardedStep::Recover)? {
            return Ok(false);
        }
        if !self.diagnosis_asks(op.task_id, Some(op.id))?.is_empty() {
            return Ok(false);
        }
        let events = self.queue.task_events(op.task_id)?;
        // A verdict a hold kept is applied, not diagnosed again.
        if replan_detect::kept_verdict(&events, op.id, op.generation).is_some() {
            return Ok(false);
        }
        let (attempt, failed) = match diagnosis_in_flight(op.id, &events, live) {
            InFlight::Elsewhere => return Ok(false),
            InFlight::Next(attempt, failed) => (attempt, failed),
        };
        // Another asker's open `decide` about the task holds its place
        // (`asks_open`): the diagnosis waits for it to close.
        if self
            .queue
            .asks(crate::application::AskQuery::default())?
            .iter()
            .any(|ask| {
                ask.kind == AskKind::Decide
                    && ask.task_id == Some(op.task_id)
                    && ask.run_id.is_none()
                    && ask.asked_by != DIAGNOSIS_ASKER
                    && ask.answered_at.is_none()
                    && ask.closed_at.is_none()
            })
        {
            return Ok(false);
        }
        // The job works under the run's directory only: for a run without
        // one nothing is written and no job starts; a person is asked (the
        // ask claims the operation) unless the run waits for its directory.
        let dir = match replan_detect::job_place(run.run_dir(), run.status(), DIAGNOSIS_LABEL) {
            JobPlace::Dir(dir) => dir,
            JobPlace::Wait => return Ok(false),
            JobPlace::Ask => {
                warn!(run_id = %run.id(), task_id = %op.task_id, "replan operation {}: run {} has no run directory, so a person decides without a diagnosis", op.id, run.id());
                let window_end = events.last().map_or(EventId::new(0), |e| e.id);
                self.ask_diagnosis(
                    op,
                    &run,
                    attempt - 1,
                    (None, window_end),
                    AskWhy::NoRunDir,
                    None,
                )?;
                return Ok(true);
            }
        };
        let window_end = events.last().map_or(EventId::new(0), |e| e.id);
        // A person decides once the jobs failed or none can start; the ask
        // claims the operation itself.
        match replan_detect::diagnosis_step(failed, self.unrecorded(op.id)) {
            replan_detect::DiagnosisStep::Start => {}
            replan_detect::DiagnosisStep::Wait => return Ok(false),
            replan_detect::DiagnosisStep::Ask => {
                self.ask_diagnosis(
                    op,
                    &run,
                    attempt - 1,
                    (None, window_end),
                    AskWhy::JobFailed,
                    None,
                )?;
                return Ok(true);
            }
        }
        let Some(route) = self.recovery_route() else {
            return Ok(false);
        };
        let (launch, switchable) = match route {
            crate::domain::actor_model::JobStartRoute::Start(launch, switchable) => {
                (launch, switchable)
            }
            crate::domain::actor_model::JobStartRoute::Unavailable(_, why) => {
                warn!(task_id = %op.task_id, "the diagnosis of replan operation {} cannot start: {why}", op.id);
                self.ask_diagnosis(
                    op,
                    &run,
                    attempt - 1,
                    (None, window_end),
                    AskWhy::JobFailed,
                    None,
                )?;
                return Ok(true);
            }
        };
        // A job starts between the turns of a run at work: a turn requested,
        // waiting in the session's `turns/` or running now would make its
        // verdict stale before it is read. A run at rest runs no turn,
        // whatever its last turn's events say (a wrapper that died in its
        // turn wrote no end).
        if run.status() == RunStatus::Running && self.turn_pending(&run, &events) {
            return Ok(false);
        }
        let head = self.run_head(&run);
        let lineage = self.queue.replan_lineage(op.task_id)?;
        let earlier_ops: Vec<ReplanOperation> = lineage
            .iter()
            .filter(|other| other.task_id == op.task_id && other.id != op.id)
            .cloned()
            .collect();
        let window = replan_detect::window(&self.replan, &events, &earlier_ops);
        let turns = replan_detect::count_turns(&events);
        let since = window.since;
        let after = |e: &&RunEvent| since.is_none_or(|since| e.id > since);
        let reviews: Vec<Value> = events
            .iter()
            .filter(after)
            .filter(|e| {
                e.kind == EventKind::ReviewFinished
                    && matches!(e.payload["verdict"].as_str(), Some("concern" | "revise"))
            })
            .map(|e| {
                json!({"event_id": e.id, "run_id": e.run_id, "verdict": e.payload["verdict"],
                       "summary": e.payload["summary"], "reasons": e.payload["reasons"]})
            })
            .collect();
        let outside: Vec<Value> = events
            .iter()
            .filter(after)
            .filter(|e| {
                e.kind.is_in(&[
                    EventKind::ConflictPrecheck,
                    EventKind::LandingRecheckFailed,
                    EventKind::RunE2eFailed,
                    EventKind::ProviderSwitched,
                ]) || (e.kind == EventKind::TurnFinished && !e.payload["failure"].is_null())
            })
            .map(|e| {
                json!({"event_id": e.id, "kind": e.kind.as_str(), "code": e.payload["code"],
                       "failure": e.payload["failure"], "reason": e.payload["reason"]})
            })
            .collect();
        let earlier: Vec<Value> = earlier_ops
            .iter()
            .map(|other| {
                json!({"operation_id": other.id, "status": other.status,
                       "choice": other.diagnosis.choice, "causes": other.diagnosis.causes,
                       "remaining": other.diagnosis.remaining, "summary": other.diagnosis.summary,
                       "window_end": other.diagnosis.window_end})
            })
            .collect();
        let receipt = run
            .run_dir()
            .map(|dir| Path::new(dir).join("receipt.json"))
            .and_then(|path| self.files.read_to_string(&path).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map(|receipt| {
                format!(
                    "{} (commit {})",
                    receipt["summary"].as_str().unwrap_or("(no summary)"),
                    receipt["commit"].as_str().unwrap_or("none")
                )
            });
        let prompt = replan_diagnosis_prompt(&DiagnosisMaterial {
            operation: op,
            task: &detail.task,
            run: &run,
            attempt,
            turns: &turns,
            window: &window,
            reviews: &reviews,
            outside: &outside,
            earlier: &earlier,
            receipt: receipt.as_deref(),
        });
        let evidence = op.diagnosis.evidence.clone();
        // The claim of this attempt and its start, in one transaction of
        // the queue: diagnosing under a new generation, owned by this
        // supervisor, once no other diagnosis job runs queue-wide. Of two
        // supervisors that read the queue alike only one starts a job, and
        // a job or an ask of an earlier generation applies nothing.
        let reason = format!("its diagnosis {attempt} starts");
        let started = json!({"attempt": attempt, "window_end": window_end,
                             "evidence": evidence, "origin": op.origin,
                             "launch": launch.to_value(), "prompt_bytes": prompt.bytes});
        let op = match self.queue.start_replan_diagnosis(
            (op.id, op.generation),
            &reason,
            run.id(),
            (started, &self.diagnosis_owner(), gone),
        ) {
            Ok(claimed) => claimed,
            Err(error) => {
                info!(task_id = %op.task_id, "replan operation {} is not claimed ({reason}): {error:#}", op.id);
                return Ok(false);
            }
        };
        // Named by the claim's generation: no two jobs share their files.
        let stem = format!("operation-{}-{}", op.id, op.generation);
        let stdout = dir.join(format!("{stem}.out"));
        let stderr = dir.join(format!("{stem}.err"));
        // A job that does not start after its start was recorded ends it as
        // failed, so the next pass starts the next attempt (a supervisor
        // stopped here leaves the start to the next one: see
        // `diagnosis_in_flight`).
        let written = self
            .files
            .create_dir_all(&dir)
            .with_context(|| format!("create {}", dir.display()))
            .and_then(|()| {
                let path = dir.join(format!("{stem}.prompt.txt"));
                self.files
                    .write(&path, prompt.text.as_bytes())
                    .with_context(|| format!("write {}", path.display()))
            });
        let spawned = written.map_err(|error| (error, false)).and_then(|()| {
            self.spawn_diagnosis(
                run.id(),
                &dir,
                attempt,
                &prompt.text,
                &launch,
                (stdout, stderr),
            )
            .map_err(|error| (error, true))
        });
        match spawned {
            Ok(job) => {
                info!(run_id = %run.id(), task_id = %op.task_id, "replan operation {}: diagnosis job {attempt} started on {}", op.id, launch.provider.as_str());
                self.replan_diagnosis = Some(Box::new(DiagnosisJob {
                    read: DiagnosisRead {
                        operation: op.id,
                        task: op.task_id,
                        run: run.id().clone(),
                        generation: op.generation,
                        attempt,
                        run_status: run.status(),
                        head,
                        window_end,
                        evidence,
                    },
                    switchable,
                    job,
                }));
            }
            Err((failed, spawning)) => {
                let error = format!("the diagnosis job could not start: {failed:#}");
                let unusable = if spawning {
                    self.recovery_start_failed(run.id(), &launch, &failed, &error, switchable)
                } else {
                    None
                };
                let mut payload = json!({"operation_id": op.id, "attempt": attempt,
                                         "started_generation": op.generation, "error": error});
                if let Some((provider, reason)) = unusable {
                    payload["provider_unusable"] = json!({"provider": provider, "reason": reason});
                }
                warn!(run_id = %run.id(), "replan operation {}: {error}", op.id);
                self.record_job_failure(run.id(), op.id, payload);
            }
        }
        Ok(true)
    }

    /// Start the provider's process of a diagnosis job: read-only, in its
    /// own scratch directory under the run's.
    fn spawn_diagnosis(
        &mut self,
        run: &RunId,
        dir: &Path,
        attempt: usize,
        prompt: &str,
        launch: &crate::domain::actor_model::ActorLaunch,
        (stdout, stderr): (PathBuf, PathBuf),
    ) -> Result<HeadlessJob> {
        let agent = self
            .job_agent(launch.provider)
            .with_context(|| format!("no {} runs on this supervisor", launch.provider.as_str()))?;
        let child = self
            .actors_on(agent)
            .spawn(ActorExecutionSpec::new(
                ActorContext::recovery_job(run, DIAGNOSIS_LABEL, attempt),
                WorkspaceAccess::Scratch(dir.to_path_buf()),
                ActorProgram::Headless {
                    program: HeadlessProgram::Job {
                        cwd: dir,
                        prompt,
                        access: TRIAGE_ACCESS,
                    },
                    session_id: None,
                    launch: Some(launch),
                    without_mcp: false,
                    without_env: &[],
                    env: Vec::new(),
                    streams: Streams::Files {
                        stdout: &stdout,
                        stderr: &stderr,
                    },
                },
            ))
            .context("start the diagnosis job")?
            .process()?;
        Ok(self.headless_job(
            "diagnosis job",
            child,
            stdout,
            stderr,
            JobSubject {
                label: Some(DIAGNOSIS_LABEL.to_owned()),
                provider: launch.provider,
                model: launch.model.clone(),
                ..JobSubject::run(headless_job::RECOVERY, run, attempt)
            },
        ))
    }

    /// Reap the diagnosis job once it ended and apply its verdict, or
    /// record its failure (its own, or its verdict's apply's) for the next
    /// attempt.
    /// Whether a job ended (taken from the watch whatever its record).
    fn reap_replan_diagnosis(&mut self) -> Result<bool> {
        let Some(provider) = self.replan_diagnosis.as_ref().map(|job| job.job.provider()) else {
            return Ok(false);
        };
        // The job's reply is read by the provider it ran on.
        let agent = self.job_agent(provider).unwrap_or(self.reviewer);
        let files = self.files.clone();
        let running = self.replan_diagnosis.as_mut().expect("read above");
        // A job that cannot be polled is stopped and counts as failed, so
        // that a loop waiting for it is not held.
        let output = match running.job.poll(&*files, agent) {
            Ok(None) => return Ok(false),
            Ok(Some(output)) => output,
            Err(error) => {
                running.job.stop();
                Err(format!("the diagnosis job could not be read: {error:#}"))
            }
        };
        let job = self.replan_diagnosis.take().expect("polled above");
        let duration_secs = job.job.elapsed().as_secs();
        let outcome = output.and_then(|stdout| DiagnosisVerdict::parse(&stdout));
        let end = self.recovery_job_end(
            &job.read.run,
            &job.job,
            agent,
            job.switchable,
            outcome.as_ref().err().map(String::as_str),
        );
        match outcome {
            Err(error) => {
                let job = &job.read;
                let mut payload = json!({"operation_id": job.operation, "attempt": job.attempt,
                                         "started_generation": job.generation,
                                         "error": error, "duration_secs": duration_secs});
                end.record(&mut payload);
                warn!(run_id = %job.run, "replan operation {}: diagnosis job {} failed: {error}", job.operation, job.attempt);
                self.record_job_failure(&job.run, job.operation, payload);
                Ok(true)
            }
            Ok(verdict) => {
                let job = &job.read;
                let mut payload = json!({"operation_id": job.operation, "attempt": job.attempt,
                                         "started_generation": job.generation,
                                         "verdict": verdict, "window_end": job.window_end,
                                         "duration_secs": duration_secs});
                end.record(&mut payload);
                let failed = payload.clone();
                if let Err(error) = self.apply_diagnosis(job, &verdict, payload) {
                    self.record_failed_apply(job, failed, &error, false);
                }
                Ok(true)
            }
        }
    }

    /// Record the apply of `job`'s verdict that ended in `error` as its
    /// `replan_diagnosis_failed` (`payload`, the verdict's, with the
    /// error), which counts toward the limit of its operation, or ends
    /// only the job's start when the operation is gone. The record of an
    /// operation still there claims it under the job's generation
    /// ([`FailureGuard`]), so that it is refused once another supervisor
    /// settled the same verdict (`kept`, a verdict a hold kept), and is
    /// then no failure. A record that cannot be written is kept for the
    /// next passes ([`replan_detect::failed_apply`]).
    fn record_failed_apply(
        &mut self,
        job: &DiagnosisRead,
        mut payload: Value,
        error: &anyhow::Error,
        kept: bool,
    ) {
        let found = error.downcast_ref::<OperationGone>().is_none();
        // A kept verdict's payload carries the outcome it was kept with.
        if let Some(fields) = payload.as_object_mut() {
            fields.remove("outcome");
            fields.remove("reason");
        }
        payload["error"] = json!(format!("the verdict could not be applied: {error:#}"));
        if !found {
            payload["operation_gone"] = json!(true);
        }
        let guard = found.then(|| FailureGuard {
            read: job.clone(),
            kept,
        });
        let written = self.write_failure(&job.run, job.operation, guard.as_ref(), &payload);
        warn!(run_id = %job.run, "replan operation {}: the verdict of diagnosis job {} could not be applied: {error:#}", job.operation, job.attempt);
        self.settle_failure(&job.run, job.operation, guard, payload, written);
    }

    /// Settle the failure of a verdict's apply of `operation` on `run`
    /// (`payload`, written under `guard` unless the operation is gone) by
    /// how its write went ([`replan_detect::failed_apply`]), the same on
    /// its first write and on each write again of a record kept unwritten.
    /// Whether it is settled: anything but kept to be written again.
    fn settle_failure(
        &mut self,
        run: &RunId,
        operation: ReplanId,
        guard: Option<FailureGuard>,
        payload: Value,
        (write, why, hold): (replan_detect::FailureWrite, String, Option<HoldGuard>),
    ) -> bool {
        match replan_detect::failed_apply(guard.is_some(), write) {
            replan_detect::FailedApply::Counted => {
                warn!(run_id = %run, "replan operation {}: the failure of its diagnosis's apply is recorded, a failed job", operation);
            }
            replan_detect::FailedApply::Ended => {
                warn!(run_id = %run, "replan operation {} is gone: the verdict of its diagnosis is not applied", operation);
            }
            replan_detect::FailedApply::Superseded => {
                if let Some(guard) = guard {
                    let requester = Some(Self::job_requester(&guard.read));
                    self.supersede_failure(run, operation, guard.kept, payload, &why, requester);
                }
            }
            replan_detect::FailedApply::Held => {
                if let (Some(guard), Some(hold)) = (guard, hold) {
                    self.keep_failed_for_hold(&guard, hold, payload, &why);
                }
            }
            replan_detect::FailedApply::Kept => {
                warn!(run_id = %run, "replan operation {}: the failure of its diagnosis's apply could not be recorded ({why}): it is recorded again at the next pass", operation);
                self.replan_unrecorded.push(UnrecordedRecord {
                    run: run.clone(),
                    operation,
                    kind: EventKind::ReplanDiagnosisFailed,
                    payload,
                    requester: None,
                    guard,
                });
                return false;
            }
            replan_detect::FailedApply::Dropped => {
                warn!(run_id = %run, "replan operation {} is gone and the failure of its diagnosis's apply could not be recorded ({why}): it is dropped", operation);
            }
        }
        true
    }

    /// The failure of a verdict's apply that a hold refused (`why`), the
    /// task held before the record ([`replan_detect::FailedApply::Held`]):
    /// nothing is counted, and the verdict is kept for the release. A
    /// verdict a hold kept already stays as it is; this supervisor's own
    /// job's verdict (`payload`) is kept with what its job read
    /// ([`Self::keep_diagnosis`]), applied after the release on its premise
    /// checked again.
    fn keep_failed_for_hold(
        &mut self,
        guard: &FailureGuard,
        hold: HoldGuard,
        mut payload: Value,
        why: &str,
    ) {
        info!(run_id = %guard.read.run, task_id = %guard.read.task, "replan operation {}: the failure of its diagnosis's apply is not recorded, the task is held, and the verdict is kept: {why}", guard.read.operation);
        if guard.kept {
            return;
        }
        if let Some(fields) = payload.as_object_mut() {
            fields.remove("error");
        }
        if let Err(error) = self.keep_diagnosis(&guard.read, hold, payload) {
            warn!(run_id = %guard.read.run, "replan operation {}: the verdict could not be kept for the hold: {error:#}", guard.read.operation);
        }
    }

    /// Write the `replan_diagnosis_failed` (`payload`) of a verdict's
    /// apply of `operation` on `run`: under `guard`, with the claim that
    /// excludes another settlement of the verdict, or, of an operation
    /// gone, alone. How it went, and the error when it did not go.
    fn write_failure(
        &mut self,
        run: &RunId,
        operation: ReplanId,
        guard: Option<&FailureGuard>,
        payload: &Value,
    ) -> (replan_detect::FailureWrite, String, Option<HoldGuard>) {
        let written = match guard {
            Some(guard) => self
                .queue
                .fail_replan_diagnosis(
                    (operation, guard.read.generation),
                    "the apply of its diagnosis's verdict failed",
                    run,
                    payload.clone(),
                )
                .map(|_| ()),
            None => self.queue.record_runtime_event(
                run,
                EventKind::ReplanDiagnosisFailed,
                payload.clone(),
            ),
        };
        failure_write(written)
    }

    /// The failure of a verdict's apply that the queue refused (`why`): the
    /// operation moved on before it, under another supervisor's settlement
    /// of the same verdict or another claim
    /// ([`replan_detect::FailedApply::Superseded`]). Nothing is counted. A
    /// verdict a hold kept (`kept`) is left as the queue has it; this
    /// supervisor's own job's verdict is recorded as discarded (`payload`,
    /// under `requester`), which ends the job's start.
    fn supersede_failure(
        &mut self,
        run: &RunId,
        operation: ReplanId,
        kept: bool,
        mut payload: Value,
        why: &str,
        requester: Option<ActorContext>,
    ) {
        info!(run_id = %run, "replan operation {}: the failure of its diagnosis's apply is not recorded, the operation moved on: {why}", operation);
        if kept {
            return;
        }
        if let Some(fields) = payload.as_object_mut() {
            fields.remove("error");
        }
        payload["outcome"] = json!("discarded");
        payload["reason"] = json!(format!(
            "the operation moved on before the failure of its apply was recorded: {why}"
        ));
        self.record_settled(run, operation, payload, requester);
    }

    /// Record the `replan_diagnosis_failed` of a job of `operation` that
    /// did not start or gave no verdict, kept for the next passes when it
    /// cannot be written, as [`Self::record_failed_apply`] keeps one.
    fn record_job_failure(&mut self, run: &RunId, operation: ReplanId, payload: Value) {
        if let Err(error) =
            self.queue
                .record_runtime_event(run, EventKind::ReplanDiagnosisFailed, payload.clone())
        {
            warn!(run_id = %run, "replan operation {}: the failure of its diagnosis job could not be recorded ({error:#}): it is recorded again at the next pass", operation);
            self.replan_unrecorded.push(UnrecordedRecord {
                run: run.clone(),
                operation,
                kind: EventKind::ReplanDiagnosisFailed,
                payload,
                requester: None,
                guard: None,
            });
        }
    }

    /// Record the `replan_diagnosed` (`payload`) of a verdict of a job of
    /// `operation` whose outcome the queue settled already: applied, asked
    /// about, kept for a hold or discarded. A write that fails fails
    /// nothing: the verdict is not applied again nor counted as a failed
    /// job, and only the record is kept and written again at the next
    /// passes ([`replan_detect::verdict_end`]), under `requester` (the job
    /// whose verdict it is) when there is one.
    fn record_settled(
        &mut self,
        run: &RunId,
        operation: ReplanId,
        payload: Value,
        requester: Option<ActorContext>,
    ) {
        let recorded =
            self.queue
                .record_runtime_event(run, EventKind::ReplanDiagnosed, payload.clone());
        if let (replan_detect::VerdictEnd::RecordKept, Err(error)) = (
            replan_detect::verdict_end(true, recorded.is_ok()),
            &recorded,
        ) {
            warn!(run_id = %run, "replan operation {}: the outcome of its diagnosis is settled but could not be recorded ({error:#}): it is recorded again at the next pass", operation);
            self.replan_unrecorded.push(UnrecordedRecord {
                run: run.clone(),
                operation,
                kind: EventKind::ReplanDiagnosed,
                payload,
                requester,
                guard: None,
            });
        }
    }

    /// Write again each record kept unwritten ([`UnrecordedRecord`]), in
    /// order; one that still cannot be written is kept for the next pass.
    /// Whether one was written.
    fn record_unrecorded(&mut self) -> bool {
        let mut written = false;
        for record in std::mem::take(&mut self.replan_unrecorded) {
            // A failure of an apply is written again under its guard, and
            // settled as its first write is: a hold that came since keeps
            // its verdict.
            if record.guard.is_some() {
                let UnrecordedRecord {
                    run,
                    operation,
                    payload,
                    guard,
                    ..
                } = record;
                let write = self.write_failure(&run, operation, guard.as_ref(), &payload);
                written |= self.settle_failure(&run, operation, guard, payload, write);
                continue;
            }
            let write = |sv: &mut Self| {
                sv.queue
                    .record_runtime_event(&record.run, record.kind, record.payload.clone())
            };
            let done = match &record.requester {
                Some(requester) => self.for_job(requester, write),
                None => write(self),
            };
            match done {
                Ok(()) => written = true,
                Err(error) => {
                    warn!(run_id = %record.run, "replan operation {}: its {} is still not recorded: {error:#}", record.operation, record.kind.as_str());
                    self.replan_unrecorded.push(record);
                }
            }
        }
        written
    }

    /// Whether a record of `op`'s diagnosis is kept unwritten
    /// ([`UnrecordedRecord`]): its next job and its kept verdict wait for it.
    fn unrecorded(&self, op: ReplanId) -> bool {
        self.replan_unrecorded.iter().any(|u| u.operation == op)
    }

    /// The actor of diagnosis job `job`, the requester of what its verdict
    /// writes.
    fn job_requester(job: &DiagnosisRead) -> ActorContext {
        ActorContext::recovery_job(&job.run, DIAGNOSIS_LABEL, job.attempt)
    }

    /// Apply `verdict` of `job`, checked again against the queue now, and
    /// record it as `replan_diagnosed` (`payload` with the outcome;
    /// [`Self::record_settled`], so that a record that fails after the
    /// queue settled the verdict fails nothing). An error is a read or a
    /// write that failed before the verdict was settled, which fails the
    /// job; a refusal of the queue discards the verdict. A
    /// verdict that reaches a held task, or that the queue refused for a
    /// hold that came meanwhile, is kept instead ([`Self::keep_diagnosis`]).
    /// Every event written meanwhile, a discarded verdict's included, has
    /// the job as its `requested_by` ([`Self::for_job`]).
    fn apply_diagnosis(
        &mut self,
        job: &DiagnosisRead,
        verdict: &DiagnosisVerdict,
        payload: Value,
    ) -> Result<()> {
        let requester = Self::job_requester(job);
        self.for_job(&requester, |sv| sv.apply_verdict(job, verdict, payload))
    }

    /// [`Self::apply_diagnosis`] with the job recorded as the requester.
    fn apply_verdict(
        &mut self,
        job: &DiagnosisRead,
        verdict: &DiagnosisVerdict,
        mut payload: Value,
    ) -> Result<()> {
        let lineage = self.queue.replan_lineage(job.task)?;
        let Some(op) = lineage.iter().find(|op| op.id == job.operation).cloned() else {
            return Err(OperationGone(job.operation).into());
        };
        let discarded = |payload: &mut Value, why: String| {
            payload["outcome"] = json!("discarded");
            payload["reason"] = json!(why);
        };
        let detail = self.queue.show(op.task_id)?;
        let latest = detail.runs.last();
        if op.status != ReplanStatus::Diagnosing || op.generation != job.generation {
            discarded(
                &mut payload,
                format!(
                    "the operation moved on while it was diagnosed ({} under generation {})",
                    op.status.as_str(),
                    op.generation
                ),
            );
        } else if detail.task.status() != TaskStatus::InProgress
            || latest.is_some_and(|run| {
                matches!(run.status(), RunStatus::Integrated | RunStatus::Succeeded)
            })
        {
            discarded(
                &mut payload,
                "the task's run landed or the task ended while it was diagnosed".to_owned(),
            );
            self.record_settled(
                &job.run,
                job.operation,
                payload,
                Some(Self::job_requester(job)),
            );
            // The next pass closes it as well (the task is not in progress).
            if let Err(error) =
                self.close_undiagnosed(&op, "the run landed or the task ended during its diagnosis")
            {
                warn!(task_id = %op.task_id, "replan operation {} is not closed yet: {error:#}", op.id);
            }
            return Ok(());
        } else if let Some(hold) = self.queue.hold_guard(op.task_id)? {
            return self.keep_diagnosis(job, hold, payload);
        } else if latest
            .is_none_or(|run| *run.id() != op.run_id || run.status() == RunStatus::Integrating)
        {
            discarded(
                &mut payload,
                "the run is landing or another run took the task over: it is diagnosed again"
                    .to_owned(),
            );
        } else {
            // An operation a person's request joined is theirs to accept
            // going on, whoever opened it.
            let origin = if op.request_id.is_some() && op.origin != ReplanOrigin::Manual {
                if self.request_was_a_persons(&op)? {
                    ReplanOrigin::Manual
                } else {
                    op.origin
                }
            } else {
                op.origin
            };
            let manual = origin == ReplanOrigin::Manual;
            let facts = DecideFacts {
                origin,
                auto_parks: replan_detect::auto_parks(&lineage),
                config: self.replan,
                remaining_before: lineage
                    .iter()
                    .filter(|other| other.task_id == op.task_id && other.id != op.id)
                    .filter_map(|other| other.diagnosis.remaining)
                    .collect(),
            };
            let run = latest.expect("checked above").clone();
            let decided = replan_detect::decide(verdict, &facts);
            self.pause_at(PausePoint::VerdictDecided, &job.run);
            match decided {
                Decided::Continue(reason) => {
                    // The runtime's choice, which may override the job's
                    // (a replanning for external causes only), is recorded.
                    let mut diagnosis = verdict.diagnosis(
                        job.evidence.clone(),
                        Some(job.window_end),
                        Some(DECIDED_BY_RUNTIME),
                    );
                    diagnosis.choice = Some(DiagnosisChoice::Continue);
                    diagnosis.premise = Some(job.premise(manual, None));
                    let moved =
                        self.move_on_premise(&op, ReplanStatus::Continuing, &reason, diagnosis);
                    let result = move_result(&moved);
                    match moved {
                        Ok(moved) => {
                            payload["outcome"] = json!("continue");
                            payload["reason"] = json!(reason);
                            moved.note(job, &mut payload);
                        }
                        Err(error) => {
                            if let Some(hold) = self.queue.hold_guard(op.task_id)? {
                                return self.keep_diagnosis(job, hold, payload);
                            }
                            // A read or a write that failed before the move
                            // committed is the job's failure, not a premise
                            // that moved on.
                            if !result.settles() {
                                return Err(error);
                            }
                            discarded(&mut payload, format!("{error:#}"));
                        }
                    }
                }
                Decided::Park(reason) => {
                    let mut diagnosis = verdict.diagnosis(
                        job.evidence.clone(),
                        Some(job.window_end),
                        Some(DECIDED_BY_RUNTIME),
                    );
                    diagnosis.premise = Some(job.premise(manual, Some(self.replan.max_auto_parks)));
                    // Refused in its transaction when the run began landing
                    // or moved on since the job started (diagnosed again),
                    // or when another park of the lineage used up its limit
                    // meanwhile (a person is asked).
                    let moved =
                        self.move_on_premise(&op, ReplanStatus::PauseRequested, &reason, diagnosis);
                    let result = move_result(&moved);
                    match moved {
                        Ok(moved) => {
                            payload["outcome"] = json!("park");
                            payload["reason"] = json!(reason);
                            moved.note(job, &mut payload);
                        }
                        Err(error) => {
                            if let Some(hold) = self.queue.hold_guard(op.task_id)? {
                                return self.keep_diagnosis(job, hold, payload);
                            }
                            if !result.settles() {
                                return Err(error);
                            }
                            let lineage = self.queue.replan_lineage(op.task_id)?;
                            // The limit is why a person is asked; the rest
                            // of the premise must still hold. The ask claims
                            // the operation under the job's generation, as
                            // the move did, so that a failure of the same
                            // verdict another supervisor recorded since
                            // refuses it.
                            match replan_detect::after_park_refused(
                                replan_detect::auto_parks(&lineage),
                                self.replan.max_auto_parks,
                                job.generation,
                            ) {
                                replan_detect::ParkRefused::Ask { generation } => {
                                    let op = ReplanOperation { generation, ..op };
                                    return self.ask_diagnosis(
                                        &op,
                                        &run,
                                        job.attempt,
                                        (Some((verdict, payload, job, manual)), job.window_end),
                                        AskWhy::ParkLimit,
                                        Some(DiagnosisChoice::Replan),
                                    );
                                }
                                replan_detect::ParkRefused::Discard => {
                                    discarded(&mut payload, format!("{error:#}"));
                                }
                            }
                        }
                    }
                }
                Decided::Ask {
                    why,
                    recommendation,
                } => {
                    // Refused when the run moved on since the job started
                    // (diagnosed again), as a move on its verdict is.
                    return self.ask_diagnosis(
                        &op,
                        &run,
                        job.attempt,
                        (Some((verdict, payload, job, manual)), job.window_end),
                        why,
                        recommendation,
                    );
                }
            }
        }
        info!(run_id = %job.run, "replan operation {}: diagnosis {} {}: {}", job.operation, job.attempt, payload["outcome"].as_str().unwrap_or_default(), payload["reason"].as_str().unwrap_or_default());
        self.record_settled(
            &job.run,
            job.operation,
            payload,
            Some(Self::job_requester(job)),
        );
        Ok(())
    }

    /// Keep the verdict of `job` (in `payload`) while its task is held by
    /// `hold`: recorded as `replan_diagnosed` of [`replan_detect::HELD_OUTCOME`]
    /// under the job's generation, with what the job read, and the
    /// operation left `diagnosing` for [`Self::apply_kept_diagnoses`].
    fn keep_diagnosis(
        &mut self,
        job: &DiagnosisRead,
        hold: HoldGuard,
        mut payload: Value,
    ) -> Result<()> {
        let reason = format!(
            "task {} is held (hold {}): the verdict is kept until the release",
            job.task, hold.hold
        );
        payload["outcome"] = json!(replan_detect::HELD_OUTCOME);
        payload["reason"] = json!(reason);
        payload["hold_id"] = json!(hold.hold);
        payload["generation"] = json!(job.generation);
        payload["read"] = json!(job);
        info!(run_id = %job.run, task_id = %job.task, "replan operation {}: diagnosis {}: {reason}", job.operation, job.attempt);
        self.record_settled(
            &job.run,
            job.operation,
            payload,
            Some(Self::job_requester(job)),
        );
        Ok(())
    }

    /// Apply each verdict a hold kept ([`replan_detect::kept_verdict`])
    /// once its task is held no more, as [`Self::apply_diagnosis`] would
    /// have: its premise is checked again, and a verdict that no longer
    /// holds is discarded, after which the next pass diagnoses the
    /// operation again. One whose apply fails is recorded once as its job's
    /// failure ([`Self::record_failed_apply`]), whose record names it and
    /// so settles it ([`replan_detect::kept_verdict`]): the next pass
    /// starts the next job, or asks a person past the limit. Whether one
    /// was applied, discarded or failed.
    fn apply_kept_diagnoses(&mut self) -> Result<bool> {
        let mut settled = false;
        for op in self.queue.open_replan_operations()? {
            if op.status != ReplanStatus::Diagnosing
                || self.unrecorded(op.id)
                || self.queue.hold_guard(op.task_id)?.is_some()
            {
                continue;
            }
            let events = self.queue.task_events(op.task_id)?;
            let Some(kept) = replan_detect::kept_verdict(&events, op.id, op.generation) else {
                continue;
            };
            let mut payload = kept.payload.clone();
            payload[replan_detect::KEPT_ID] = json!(kept.id);
            let read = serde_json::from_value::<DiagnosisRead>(payload["read"].clone());
            let verdict = PlanningPayload::of(kept).verdict;
            if let Some(fields) = payload.as_object_mut() {
                for key in ["read", "hold_id", "generation", "why"] {
                    fields.remove(key);
                }
            }
            match (read, verdict) {
                (Ok(read), Some(verdict)) => {
                    let failed = payload.clone();
                    if let Err(error) = self.apply_diagnosis(&read, &verdict, payload) {
                        warn!(task_id = %op.task_id, error = %format_args!("{error:#}"), "the verdict kept for replan operation {} could not be applied: {error:#}", op.id);
                        self.record_failed_apply(&read, failed, &error, true);
                    }
                }
                _ => {
                    // A record this binary cannot read is diagnosed again.
                    payload["outcome"] = json!("discarded");
                    payload["reason"] = json!("the verdict kept for the hold cannot be read");
                    let run = kept.run_id.clone().unwrap_or_else(|| op.run_id.clone());
                    self.record_settled(&run, op.id, payload, None);
                }
            }
            settled = true;
        }
        Ok(settled)
    }

    /// [`Self::move_diagnosed`] of a diagnosis job's verdict, once the
    /// run's HEAD is still the one its premise read (the rest of the
    /// premise is checked in the move's transaction).
    fn move_on_premise(
        &mut self,
        op: &ReplanOperation,
        to: ReplanStatus,
        reason: &str,
        diagnosis: Diagnosis,
    ) -> Result<Moved> {
        if let Some(premise) = &diagnosis.premise {
            self.head_still(op, premise)?;
        }
        self.move_diagnosed(op, to, reason, diagnosis)
    }

    /// Refuse a diagnosis job's verdict once the HEAD of `op`'s run moved
    /// from the one its `premise` read: checked just before the write, out
    /// of the queue's transaction, which checks the rest.
    fn head_still(&mut self, op: &ReplanOperation, premise: &DiagnosisPremise) -> Result<()> {
        let run = self
            .queue
            .show(op.task_id)?
            .runs
            .last()
            .cloned()
            .with_context(|| format!("task {} has no run", op.task_id))?;
        let now = self.run_head(&run);
        if premise.head_moved(now.as_deref()) {
            return Err(crate::domain::DomainError::ReplanPremiseStale {
                operation: op.id,
                why: format!(
                    "run {}'s HEAD moved from {} to {} since its diagnosis started",
                    run.id(),
                    premise.head.as_deref().unwrap_or_default(),
                    now.as_deref().unwrap_or_default()
                ),
            }
            .into());
        }
        Ok(())
    }

    /// Move `op` from its diagnosis to `to` with `diagnosis` recorded; an
    /// ask of its diagnosis still open closes with it, and its task is
    /// looked at again by the next detection. An error is the move's own
    /// (it did not commit); one of closing the asks after it committed is
    /// [`Moved::followed`]'s, and leaves the move as it is: an ask left
    /// open is closed once it is answered, its operation having moved on
    /// ([`Self::apply_diagnosis_answers`]).
    fn move_diagnosed(
        &mut self,
        op: &ReplanOperation,
        to: ReplanStatus,
        reason: &str,
        diagnosis: Diagnosis,
    ) -> Result<Moved> {
        self.queue.move_replan(
            op.id,
            op.generation,
            &ReplanMove {
                to,
                reason: reason.to_owned(),
                snapshot_id: None,
                diagnosis: Some(diagnosis),
            },
        )?;
        self.replan_seen.1.remove(&op.task_id);
        let followed = self.close_open_diagnosis_asks(op);
        if let Err(error) = &followed {
            warn!(task_id = %op.task_id, "replan operation {} moved to {}, but its open diagnosis asks could not be closed: {error:#}", op.id, to.as_str());
        }
        Ok(Moved { followed })
    }

    /// Close the unanswered diagnosis asks of `op`.
    fn close_open_diagnosis_asks(&mut self, op: &ReplanOperation) -> Result<()> {
        for ask in self.diagnosis_asks(op.task_id, Some(op.id))? {
            if ask.answered_at.is_none() {
                self.queue.close_ask(ask.id)?;
            }
        }
        Ok(())
    }

    /// Whether `op`'s planning request was recorded by a person's (or the
    /// inbox's) `replan request` that joined it, not by the runtime's
    /// fence.
    fn request_was_a_persons(&self, op: &ReplanOperation) -> Result<bool> {
        let events = self.queue.task_events(op.task_id)?;
        Ok(events.iter().any(|e| {
            let read = PlanningPayload::of(e);
            e.kind == EventKind::ReplanRequested
                && read.is_of(op.id)
                && read.origin == Some(ReplanOrigin::Manual)
        }))
    }

    /// Whether a turn of `run`'s headless session is requested, waits in
    /// its `turns/` or runs ([`replan_detect::turn_pending`]), from `events`
    /// (its task's), its requests and its idle marker. Requests that cannot
    /// be read are logged and leave the events alone to judge (the
    /// premise's checks still refuse a verdict a turn made stale).
    fn turn_pending(&self, run: &TaskRun, events: &[RunEvent]) -> bool {
        let listed = run.run_dir().map_or_else(Vec::new, |dir| {
            headless::listed_requests(&*self.files, Path::new(dir)).unwrap_or_else(|error| {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the requests of {} cannot be read; its diagnosis reads its turn events alone: {error:#}", run.id());
                Vec::new()
            })
        });
        let mark = run
            .idle_marker_path()
            .ok()
            .and_then(|marker| headless::last_turn(&*self.files, &marker));
        replan_detect::turn_pending(events, run.id(), &listed, mark.as_ref())
    }

    /// `run`'s HEAD, when its worktree is there and reads.
    fn run_head(&self, run: &TaskRun) -> Option<String> {
        let worktree = run.worktree_path().map(Path::new)?;
        if !run.in_container() && !self.files.is_dir(worktree) {
            return None;
        }
        crate::application::run_git::head(&*self.repository, run)
            .ok()
            .map(|head| head.to_string())
    }

    /// Ask a person what to do with `op` (still `diagnosing`) for `why`,
    /// recommending `recommendation`: the verdict of diagnosis `attempt`
    /// (with the `replan_diagnosed` payload it ends and the premise of the
    /// job that gave it) when there is one, and the last event it read. A
    /// verdict whose premise no longer holds opens no ask: it is recorded
    /// as discarded and the next pass diagnoses the operation again.
    fn ask_diagnosis(
        &mut self,
        op: &ReplanOperation,
        run: &TaskRun,
        attempt: usize,
        (verdict, window_end): (
            Option<(&DiagnosisVerdict, Value, &DiagnosisRead, bool)>,
            EventId,
        ),
        why: AskWhy,
        recommendation: Option<DiagnosisChoice>,
    ) -> Result<()> {
        let (verdict, mut payload, job) = match verdict {
            Some((verdict, payload, job, manual)) => (Some(verdict), payload, Some((job, manual))),
            None => (
                None,
                json!({"operation_id": op.id, "attempt": attempt, "window_end": window_end}),
                None,
            ),
        };
        let premise = job.map(|(job, manual)| job.premise(manual, None));
        let failures = if verdict.is_none() {
            replan_detect::failures_said(&self.queue.task_events(op.task_id)?, op.id)
        } else {
            String::new()
        };
        let said = verdict.map_or_else(
            || failures,
            |verdict| {
                format!(
                    "The diagnosis chose {} with {} confidence: {}. Causes: {}. Progress: {}. Unmet acceptance items: {}.{}",
                    verdict.choice.as_str(),
                    verdict.confidence.trim(),
                    verdict.reason,
                    if verdict.causes.is_empty() {
                        "none named".to_owned()
                    } else {
                        verdict
                            .causes
                            .iter()
                            .map(|cause| cause.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                    or_none(&verdict.progress),
                    verdict
                        .remaining
                        .map_or_else(|| "not said".to_owned(), |n| n.to_string()),
                    if verdict.question.trim().is_empty() {
                        String::new()
                    } else {
                        format!(" It asks: {}", verdict.question)
                    },
                )
            },
        );
        let question = format!(
            "Replan operation {} of task {} (run {}, origin {}): {}. {said}\n\
             Answer continue to let the run go on with the same conditions (its revise and resume limits stay as they are), or replan to have the runtime park it at a safe boundary and save its work for a planner's replacement that keeps every acceptance item (plan review checks it before it is applied).",
            op.id,
            op.task_id,
            run.id(),
            op.origin.as_str(),
            why.sentence(),
        );
        let ask = NewAsk {
            kind: AskKind::Decide,
            task_id: Some(op.task_id),
            run_id: None,
            question,
            options: DIAGNOSIS_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
            asked_by: DIAGNOSIS_ASKER.to_owned(),
            reason_category: match why {
                AskWhy::JobFailed | AskWhy::ParkLimit | AskWhy::NoRunDir => {
                    AskReason::RecoveryFailed
                }
                _ => AskReason::Scope,
            },
            topics: Vec::new(),
            recommendation: recommendation
                .filter(|choice| *choice != DiagnosisChoice::Ask)
                .map(|choice| choice.as_str().to_owned()),
            confidence: Some(if verdict.is_some_and(DiagnosisVerdict::sure) {
                AskConfidence::High
            } else {
                AskConfidence::Low
            }),
            finding_id: None,
            request_id: None,
        };
        payload["why"] = json!(why);
        payload["reason"] = json!(why.sentence());
        // The ask, the operation claimed again for it and the
        // `replan_diagnosed` that ties them are written in one transaction:
        // a job of an earlier claim applies nothing over the ask, its answer
        // applies only while no later claim came, and a supervisor stopped
        // midway leaves no ask its answers would not find. Another ask of
        // the task holding its place defers it until it closes (the job is
        // not started again while that ask is open).
        let reason = format!("a person is asked: {}", why.sentence());
        let asked = match &premise {
            Some(premise) => self.head_still(op, premise),
            None => Ok(()),
        }
        .and_then(|()| {
            self.queue.ask_replan_diagnosis(
                &ask,
                (op.id, op.generation),
                &reason,
                run.id(),
                (payload.clone(), premise.as_ref()),
            )
        });
        let ask = match asked {
            Ok((outcome, _)) => outcome.ask,
            Err(error) => {
                // A hold that came meanwhile keeps the verdict for the
                // release; with no verdict, the release starts a job.
                if let Some((job, _)) = job
                    && let Some(hold) = self.queue.hold_guard(op.task_id)?
                {
                    return self.keep_diagnosis(job, hold, payload);
                }
                // A verdict whose ask a read or a write failed to open is
                // its job's failure; one whose premise moved on is
                // discarded and diagnosed again.
                if job.is_some() && !refused(&error) {
                    return Err(error);
                }
                payload["outcome"] = json!("discarded");
                payload["reason"] = json!(format!("{error:#}"));
                info!(task_id = %op.task_id, "replan operation {}: no ask: {error:#}", op.id);
                let requester = job.map(|(job, _)| Self::job_requester(job));
                self.record_settled(run.id(), op.id, payload, requester);
                return Ok(());
            }
        };
        if ask.asked_by == DIAGNOSIS_ASKER {
            warn!(run_id = %run.id(), "replan operation {}: {}; decide ask {}", op.id, why.sentence(), ask.id);
        } else {
            warn!(task_id = %op.task_id, "replan operation {}: ask {} of task {} is open, so its diagnosis asks once it closes", op.id, ask.id, op.task_id);
        }
        Ok(())
    }

    /// Apply each answered diagnosis ask: `continue` closes its operation
    /// as `continuing`, `replan` moves it past the pause's fence, both
    /// recorded as a person's decision. An ask whose operation moved on is
    /// closed; any other answer is a person's to read.
    fn apply_diagnosis_answers(&mut self) -> Result<bool> {
        let mut applied = false;
        let answered: Vec<Ask> = self
            .queue
            .asks(crate::application::AskQuery::default())?
            .into_iter()
            .filter(|ask| {
                ask.kind == AskKind::Decide
                    && ask.asked_by == DIAGNOSIS_ASKER
                    && ask.answered_at.is_some()
                    && ask.closed_at.is_none()
            })
            .collect();
        for ask in answered {
            let Some(task) = ask.task_id else {
                continue;
            };
            let Some(choice) = crate::domain::recovery::chosen_option(
                &ask.options,
                ask.answer.as_deref().unwrap_or_default(),
            )
            .cloned() else {
                continue;
            };
            let events = self.queue.task_events(task)?;
            let Some(asked) = events.iter().rfind(|e| {
                e.kind == EventKind::ReplanDiagnosed
                    && PlanningPayload::of(e).ask_id == Some(ask.id)
            }) else {
                continue;
            };
            let asked = PlanningPayload::of(asked);
            let Some(op_id) = asked.operation_id else {
                continue;
            };
            let lineage = self.queue.replan_lineage(task)?;
            let Some(op) = lineage.into_iter().find(|op| op.id == op_id) else {
                continue;
            };
            // An ask of an older binary names no generation: it is the
            // operation's while it is diagnosing.
            let claimed_since = asked.generation.is_some_and(|g| g != op.generation);
            if op.status != ReplanStatus::Diagnosing || claimed_since {
                info!(ask_id = %ask.id, "ask {} is closed: replan operation {} is {} under generation {} already", ask.id, op.id, op.status.as_str(), op.generation);
                self.queue.close_ask(ask.id)?;
                applied = true;
                continue;
            }
            let detail = self.queue.show(task)?;
            let Some(run) = detail.runs.last().cloned() else {
                continue;
            };
            if detail.task.status() != TaskStatus::InProgress
                || matches!(run.status(), RunStatus::Integrated | RunStatus::Succeeded)
                || *run.id() != op.run_id
            {
                self.close_undiagnosed(&op, &format!("the run landed, the task ended or another run took it over before the answer of ask {}", ask.id))?;
                applied = true;
                continue;
            }
            // The landing owns a run that is landing: answered once it is
            // back or closed once it landed.
            if run.status() == RunStatus::Integrating {
                continue;
            }
            // A held task's answer stays on its ask, applied once the hold
            // is released (the move reads the hold again).
            if self.held_out(task, Some(run.id()), GuardedStep::Recover)? {
                continue;
            }
            let window_end = asked.window_end;
            let chosen = if choice == "replan" {
                DiagnosisChoice::Replan
            } else {
                DiagnosisChoice::Continue
            };
            let mut diagnosis = asked.verdict.as_ref().map_or_else(
                || Diagnosis {
                    evidence: op.diagnosis.evidence.clone(),
                    window_end,
                    ..Diagnosis::default()
                },
                |verdict| verdict.diagnosis(op.diagnosis.evidence.clone(), window_end, None),
            );
            diagnosis.choice = Some(chosen);
            diagnosis.decided_by = Some(DECIDED_BY_PERSON.to_owned());
            // A person decides on what the run is now: only its landing,
            // or a change while the move is written, refuses the answer.
            diagnosis.premise = Some(DiagnosisPremise {
                run_status: run.status(),
                read_until: None,
                head: None,
                manual: None,
                max_auto_parks: None,
            });
            let (to, reason) = if chosen == DiagnosisChoice::Replan {
                (
                    ReplanStatus::PauseRequested,
                    format!("a person chose replan in ask {}", ask.id),
                )
            } else {
                (
                    ReplanStatus::Continuing,
                    format!("a person chose continue in ask {}", ask.id),
                )
            };
            match self.move_diagnosed(&op, to, &reason, diagnosis) {
                Ok(Moved { .. }) => {
                    info!(ask_id = %ask.id, task_id = %task, "replan operation {}: {reason}", op.id);
                    self.queue.close_ask(ask.id)?;
                    applied = true;
                }
                Err(error) => {
                    warn!(ask_id = %ask.id, error = %format_args!("{error:#}"), "the answer of ask {} could not be applied to replan operation {}: {error:#}", ask.id, op.id);
                }
            }
        }
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::DomainError;

    /// The queue's refusal of a verdict's move or ask, through any context,
    /// discards the verdict; any other error, a write that failed, fails
    /// the job.
    #[test]
    fn a_refusal_discards_a_verdict_and_a_failed_write_fails_the_job() {
        let stale = anyhow::Error::new(DomainError::ReplanPremiseStale {
            operation: ReplanId::new(1),
            why: "task 1 was edited".to_owned(),
        });
        assert!(refused(&stale));
        assert!(refused(&stale.context("apply the verdict")));
        let held = anyhow::Error::new(DomainError::ReplanDiagnosisHeld {
            operation: ReplanId::new(1),
            task_id: TaskId::new(1),
            hold: crate::domain::HoldId::new(2),
        });
        assert!(refused(&held));
        let write = anyhow::anyhow!("ask write failed").context("open the diagnosis's ask");
        assert!(!refused(&write));
    }

    /// The write of a failed apply's record is judged by its result alone:
    /// a refusal that names a hold, on the queue or through the service,
    /// keeps the verdict for that hold (on a first write and on a write
    /// again alike, which both go through it), whatever a read of the hold
    /// after it would say; any other refusal is another settlement, and an
    /// error that is no refusal keeps the record for the next pass.
    #[test]
    fn a_failure_refused_for_a_hold_is_held_by_the_refusal_alone() {
        use crate::application::supervisor_service::service_error;
        use crate::domain::queue_service::{ServiceError, ServiceErrorCode};
        use crate::domain::task_hold::HoldGuard;
        use replan_detect::FailureWrite;
        let hold = HoldGuard {
            task: TaskId::new(1),
            hold: crate::domain::HoldId::new(2),
        };
        let on_queue = anyhow::Error::new(DomainError::ReplanDiagnosisHeld {
            operation: ReplanId::new(1),
            task_id: hold.task,
            hold: hold.hold,
        })
        .context("fail_replan_diagnosis");
        let through_service = |held: Option<HoldGuard>| {
            service_error(&ServiceError {
                code: ServiceErrorCode::Failed,
                message: "refused".into(),
                denied: None,
                kind: None,
                rejected: true,
                held: held.map(Box::new),
            })
        };
        for refusal in [on_queue, through_service(Some(hold))] {
            let (write, why, held) = failure_write(Err(refusal));
            assert_eq!((write, held), (FailureWrite::Held, Some(hold)), "{why}");
        }
        let stale = anyhow::Error::new(DomainError::ReplanGenerationStale {
            operation: ReplanId::new(1),
            expected: 3,
            actual: 4,
        });
        for refusal in [stale, through_service(None)] {
            let (write, _, held) = failure_write(Err(refusal));
            assert_eq!((write, held), (FailureWrite::Refused, None));
        }
        let (write, why, held) = failure_write(Err(anyhow::anyhow!("database or disk is full")));
        assert_eq!((write, held), (FailureWrite::Failed, None));
        assert!(why.contains("disk is full"), "{why}");
        assert_eq!(failure_write(Ok(())).0, FailureWrite::Written);
    }
}
