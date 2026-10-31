//! The headless jobs of a run: the review (ADR-0027) and the recovery job
//! (ADR-0047 decisions 39 and 40), each a process waited for with a
//! timeout. Every headless job's process is recorded in `headless_jobs`
//! (task 443): a supervisor that takes over from one that died stops the
//! jobs it left before it starts its own. A job of a run's review stage is
//! of one of two kinds ([`JobKind`], ADR-t1895-1 decision 1), an agent's
//! session or a program's process, and both go the one way here: start,
//! record, wait under the kind's timeout, stop with what the job started,
//! and take over. [`JobDesk`] lends the agents, the record of a job and
//! the providers' holds to 計画管理's jobs.

use super::*;
pub use crate::application::review_programs::{PROGRAM_OUTPUT_TAIL, ProgramEnd};
use crate::application::review_programs::{
    ProgramBackends, prepare_program, program_end, spawn_program,
};
use crate::domain::EventKind;
use crate::domain::environment::{RunChild, RunChildren};
use crate::domain::headless_job::{JobFailure, JobKind, JobStop};
use crate::domain::provider_switch::SwitchReason;
use crate::domain::review_jobs::{self, ReviewJobId};
use crate::{
    application::{Clock, CommandSpec, HeadlessJobRecord, HeadlessJobStore, NewHeadlessJob},
    domain::headless_job::{Takeover, takeover},
};
use std::sync::Mutex;

/// How long the processes of a gone supervisor's job get after SIGTERM
/// before SIGKILL.
const TAKEOVER_GRACE: Duration = Duration::from_secs(5);

/// How many of the newest events of each kind that ends a job a supervisor
/// that closes a gone one's job with no run (stopped, found gone or not
/// the job) reads, to tell whether one ended it with its Execution
/// already.
const TAKEOVER_ENDS_READ: usize = 50;

/// The ends of this process's headless jobs not written to the queue yet:
/// each `headless_jobs` row's outcome, and the jobs stopped with no event
/// to end them ([`HeadlessJob::abandon`]). A job ends where no queue is at
/// hand, and the supervisor writes them at the top of its next pass.
#[derive(Clone, Default)]
pub struct JobEnds(Arc<Mutex<Ends>>);

#[derive(Default)]
struct Ends {
    rows: Vec<(i64, &'static str)>,
    abandoned: Vec<Abandoned>,
}

/// A job this process stopped with no event to end it (a handoff, a slot
/// it stops watching, a drop): its agent ran all the same, so its end is
/// written as `headless_job_stopped` with its Execution (ADR-t1486-1).
pub(super) struct Abandoned {
    pub(super) subject: JobSubject,
    pub(super) stdout: PathBuf,
    pub(super) started_at: Option<i64>,
}

impl JobEnds {
    fn lock(&self) -> std::sync::MutexGuard<'_, Ends> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn push(&self, id: i64, outcome: &'static str) {
        self.lock().rows.push((id, outcome));
    }
    fn abandoned(&self, job: Abandoned) {
        self.lock().abandoned.push(job);
    }
    fn take(&self) -> (Vec<(i64, &'static str)>, Vec<Abandoned>) {
        let mut ends = self.lock();
        (
            std::mem::take(&mut ends.rows),
            std::mem::take(&mut ends.abandoned),
        )
    }

    /// Write the rows' ends not written yet to `store`; the rows whose end
    /// could not be written, with why. The abandoned jobs are left to the
    /// supervisor, whose agents read their Execution
    /// ([`Supervisor::write_job_ends`]).
    pub fn write(&self, store: &dyn HeadlessJobStore) -> Vec<(i64, anyhow::Error)> {
        let rows = std::mem::take(&mut self.lock().rows);
        rows.into_iter()
            .filter_map(|(id, outcome)| store.end_headless_job(id, outcome).err().map(|e| (id, e)))
            .collect()
    }
}

/// The `headless_job_stopped` of `job`, stopped by `supervisor` with no
/// event to end it, with the Execution its agent was so far
/// (ADR-t1486-1): `session`, what its output named, or not measured.
/// The supervisor's operation that records `headless_job_stopped` of
/// `job` with `payload`, as its end is written ([`abandoned_end`]): on its
/// run, or on the queue for a job with none.
fn abandoned_record(job: &Abandoned, payload: serde_json::Value) -> serde_json::Value {
    match &job.subject.run_id {
        Some(run) => json!({"op": "record_runtime_event",
            "args": [run, EventKind::HeadlessJobStopped, payload]}),
        None => json!({"op": "record_queue_event",
            "args": [EventKind::HeadlessJobStopped, payload]}),
    }
}

pub(super) fn abandoned_end(
    job: &Abandoned,
    supervisor: &LeaseToken,
    session: Option<&crate::domain::headless_job::JobSession>,
) -> Value {
    let subject = &job.subject;
    let mut payload = json!({
        "kind": subject.kind,
        "label": subject.label,
        "run_id": subject.run_id,
        "proposal_id": subject.proposal_id,
        "goal_id": subject.goal_id,
        "attempt": subject.attempt,
        "provider": subject.provider,
        "supervisor": supervisor,
        // Unix seconds, as a takeover's `headless_job_stopped` gives its
        // row's.
        "started_at": job.started_at.map(|ms| ms.div_euclid(1000)),
    });
    crate::domain::headless_job::JobSession::record_execution(session, &mut payload);
    payload
}

/// The `headless_job_stopped` of `job`, a gone supervisor's job this one
/// stopped or found gone or not the job ([`close_taken_over`]), with the
/// processes it signalled (`descendants`) and killed:
/// with the Execution its agent was (ADR-t1486-1) when `ran` is `Some`,
/// `session` or not measured; without for a program job or one an event
/// ended with its Execution already.
pub(super) fn taken_over_end(
    job: &HeadlessJobRecord,
    (descendants, killed): (Vec<u32>, Vec<u32>),
    ran: Option<Option<&crate::domain::headless_job::JobSession>>,
) -> Value {
    let mut payload = json!({
        "headless_job_id": job.id,
        "kind": job.kind,
        "label": job.label,
        "run_id": job.run_id,
        "proposal_id": job.proposal_id,
        "goal_id": job.goal_id,
        "attempt": job.attempt,
        "pid": job.pid,
        "process_start": job.process_start,
        "descendants": descendants,
        "killed": killed,
        "supervisor": job.supervisor_token,
        "started_at": job.started_at,
    });
    if job.provider != headless_job::NO_PROVIDER {
        payload["provider"] = json!(job.provider);
    }
    if let Some(session) = ran {
        crate::domain::headless_job::JobSession::record_execution(session, &mut payload);
    }
    payload
}

/// Close the row of `job`, a gone supervisor's job judged `judged`, with
/// `end` (the row's outcome; `false` when another supervisor closed it
/// first, which then records its end), and give the `headless_job_stopped`
/// to record: the job is closed first, so of two supervisors taking over
/// at once only one signals it, then `stop`ped only when its pid runs its
/// process ([`Takeover::Stop`]), and its agent's Execution read
/// (`execution`, as [`taken_over_end`] takes it). A job found gone or not
/// the job (its pid runs another process, or its start cannot be told) is
/// never stopped and gets an end only for an Execution to record; that of
/// a job not the job, which may run still, is its output so far.
pub(super) fn close_taken_over(
    job: &HeadlessJobRecord,
    judged: Takeover,
    end: impl FnOnce(&'static str) -> Result<bool>,
    stop: impl FnOnce() -> (Vec<u32>, Vec<u32>),
    execution: impl FnOnce() -> Option<Option<crate::domain::headless_job::JobSession>>,
) -> Result<Option<Value>> {
    let outcome = match judged {
        Takeover::Gone => headless_job::GONE,
        Takeover::NotTheJob => headless_job::NOT_THE_JOB,
        Takeover::Stop => headless_job::TAKEN_OVER,
    };
    if !end(outcome)? {
        return Ok(None);
    }
    let stopped = if judged == Takeover::Stop {
        stop()
    } else {
        (Vec::new(), Vec::new())
    };
    let ran = execution();
    if judged != Takeover::Stop && ran.is_none() {
        return Ok(None);
    }
    let mut payload = taken_over_end(job, stopped, ran.as_ref().map(Option::as_ref));
    // The row's outcome, to tell a job stopped (`taken_over`) from one
    // closed with no signal (`gone`, `not_the_job`).
    payload["outcome"] = json!(outcome);
    Ok(Some(payload))
}

/// What `agent` (the provider of `job`'s row) reads out of the stdout the
/// row names of the session and Execution of `job`, a gone supervisor's
/// job; `None` when the row names no stdout or it cannot be read.
pub(super) fn taken_over_session(
    agent: &dyn AgentProvider,
    files: &dyn RunFiles,
    job: &HeadlessJobRecord,
) -> Option<crate::domain::headless_job::JobSession> {
    let stdout = files.read_to_string(job.stdout.as_ref()?).ok()?;
    // The row's start is in unix seconds; the provider reads from
    // milliseconds.
    agent.job_session(&stdout, Some(job.started_at * 1000))
}

/// What a headless job is about, for its `headless_jobs` row and its
/// timeout.
#[derive(Clone)]
pub struct JobSubject {
    pub(in crate::application::supervise) kind: &'static str,
    /// What runs it: an agent, unless it is a program job of a run's
    /// review.
    pub(in crate::application::supervise) job: JobKind,
    /// Whether it is a job of a run's review stage, whose timeout
    /// `[review.jobs]` sets by its kind.
    pub(in crate::application::supervise) review_stage: bool,
    /// Its `headless_jobs.label`: of a run's review, the agent its job is
    /// of ([`crate::domain::review_jobs::ReviewJobId::agent`]; none for the
    /// whole review's), so that the row tells its run and which job of the
    /// attempt it is. Other jobs name what they are about here (a review's
    /// program, a recovery job's alert, an agent eval's case).
    pub(in crate::application::supervise) label: Option<String>,
    pub(in crate::application::supervise) run_id: Option<RunId>,
    pub(in crate::application::supervise) proposal_id: Option<crate::domain::ProposalId>,
    pub(in crate::application::supervise) goal_id: Option<crate::domain::GoalId>,
    pub(in crate::application::supervise) attempt: usize,
    /// The provider the job was started on (ADR-t1063-1), written to the
    /// row's `headless_jobs.provider`. That column is `NOT NULL DEFAULT
    /// 'claude'`, so a row from before it, or one an older binary that does
    /// not know it writes, reads as `claude`.
    pub(in crate::application::supervise) provider: Provider,
    /// The model its launch names, which the model's turns of its
    /// session's steps carry (a Codex job's output does not name it).
    pub(in crate::application::supervise) model: Option<String>,
}

impl JobSubject {
    /// An agent job of `run`, on Claude; a job whose role names another
    /// provider sets `provider` to its launch's (the review and the
    /// recovery job, ADR-t1063-1).
    pub(super) fn run(kind: &'static str, run: &RunId, attempt: usize) -> Self {
        Self {
            kind,
            job: JobKind::Agent,
            review_stage: false,
            label: None,
            run_id: Some(run.clone()),
            proposal_id: None,
            goal_id: None,
            attempt,
            provider: crate::domain::actor_model::ROLE_PROVIDER,
            model: None,
        }
    }

    /// A `job` job of `run`'s review `attempt`.
    pub(super) fn review(job: JobKind, run: &RunId, attempt: usize) -> Self {
        Self {
            job,
            review_stage: true,
            ..Self::run(job.review_kind(), run, attempt)
        }
    }

    /// The program job named `program` of `run`'s review `attempt`: no
    /// provider runs it.
    pub fn review_program(run: &RunId, attempt: usize, program: &str) -> Self {
        Self {
            label: Some(program.to_owned()),
            ..Self::review(JobKind::Program, run, attempt)
        }
    }

    /// A program job of a case of an agent's eval (ADR-t1728-1 (i)),
    /// labelled `label`, under the review stage's program timeout as a
    /// run's review's program job; `attempt` is the case's run it goes
    /// before.
    pub(super) fn eval_program(label: String, attempt: usize) -> Self {
        Self {
            kind: crate::domain::headless_job::AGENT_EVAL_PROGRAM,
            job: JobKind::Program,
            review_stage: true,
            label: Some(label),
            run_id: None,
            proposal_id: None,
            goal_id: None,
            attempt,
            provider: crate::domain::actor_model::ROLE_PROVIDER,
            model: None,
        }
    }
}

/// What every headless job starts through, whatever its kind: where its
/// row is written, how its process is found and stopped, whose job it is,
/// where its end waits to be written, and where a run's job is its run's
/// process while it runs (the measure of the run's resources counts it).
pub struct JobPorts<'a> {
    pub store: &'a dyn HeadlessJobStore,
    pub processes: Arc<dyn ProcessControl + Send + Sync>,
    pub supervisor_token: &'a LeaseToken,
    pub clock: &'a Arc<dyn Clock>,
    pub ends: &'a JobEnds,
    pub children: &'a RunChildren,
}

/// The job whose process `child` just started, recorded in
/// `headless_jobs` with its kind, its pid and the start of its process,
/// waited for at most `timeout`; a job of a run is the run's process in
/// [`JobPorts::children`] until it is dropped. A record that fails is only
/// noted: the job runs either way.
pub fn record_job(
    ports: &JobPorts<'_>,
    what: &'static str,
    child: Box<dyn Spawned>,
    (stdout, stderr): (PathBuf, PathBuf),
    subject: JobSubject,
    timeout: Duration,
) -> HeadlessJob {
    let pid = child.id();
    let new = NewHeadlessJob {
        kind: subject.kind,
        label: subject.label.clone(),
        run_id: subject.run_id.clone(),
        proposal_id: subject.proposal_id,
        goal_id: subject.goal_id,
        attempt: subject.attempt,
        provider: (subject.job == JobKind::Agent).then_some(subject.provider),
        pid,
        process_start: ports.processes.start_identity(pid),
        supervisor_token: ports.supervisor_token.clone(),
        stdout: Some(stdout.clone()),
    };
    let record = match ports.store.record_headless_job(&new) {
        Ok(id) => Some((ports.ends.clone(), id)),
        Err(error) => {
            warn!(error = %format_args!("{error:#}"), "the start of the headless {what} (pid {pid}) could not be recorded: {error:#}");
            None
        }
    };
    HeadlessJob {
        what,
        kind: subject.job,
        child,
        started: ports.clock.monotonic(),
        clock: Arc::clone(ports.clock),
        timeout,
        stdout,
        stderr,
        processes: ports.processes.clone(),
        record,
        provider: subject.provider,
        started_at: started_at_ms(&**ports.clock),
        run_child: subject
            .run_id
            .as_ref()
            .map(|run| ports.children.started(run, pid)),
        // Only an agent's job is an Execution whose end is recorded when it
        // is abandoned (ADR-t1486-1); a program job's ends with its row.
        unended: (subject.job == JobKind::Agent).then(|| (ports.ends.clone(), subject)),
        steps: None,
        timed_out: false,
    }
}

/// Start the program job `program` (ADR-t1895-1 decision 1) in a process
/// group of its own, its stdout and stderr to `output`, and record it as
/// [`record_job`] does: its timeout stops the group and what the program
/// started.
pub fn start_program_job(
    ports: &JobPorts<'_>,
    spawner: &dyn Spawner,
    program: &CommandSpec,
    output: (PathBuf, PathBuf),
    subject: JobSubject,
    timeout: Duration,
) -> Result<HeadlessJob> {
    let child = spawn_program(spawner, program, (&output.0, &output.1))?;
    Ok(record_job(
        ports,
        "review program",
        child,
        output,
        subject,
        timeout,
    ))
}

/// Start the program review `program` of `run`'s review `attempt` as a
/// program job against `worktree` (ADR-t1895-2) on the backend of the
/// review's actor ([`ProgramBackends::backend`]: the host's adapter, or
/// the error of one not implemented, which starts nothing). Its output
/// goes under `output` (the run's directory) and what it runs is written
/// under `scratch`, each by `attempt` (the run's review's round of program
/// reviews) and name, and it is stopped with its
/// group past the program's own `timeout_secs`, or `timeout` without one.
/// `scratch` is a directory the runtime owns, outside what the worker can
/// write (not the run's directory): a script written where the worker can
/// replace it would run the worker's text.
#[allow(clippy::too_many_arguments)]
pub fn start_review_program(
    ports: &JobPorts<'_>,
    spawner: &dyn Spawner,
    backends: &ProgramBackends<'_>,
    program: &crate::application::review_programs::SnapshotProgram,
    (worktree, env): (&Path, &[(String, String)]),
    (output, scratch): (&Path, &Path),
    (run, attempt): (&RunId, usize),
    timeout: Duration,
) -> Result<HeadlessJob> {
    let name = &program.program.name;
    start_program_against(
        ports,
        spawner,
        backends,
        program,
        (worktree, env),
        (output, scratch),
        (
            &format!("review-program-{attempt}-{name}"),
            JobSubject::review_program(run, attempt, name),
        ),
        timeout,
    )
}

/// Start `program` as a program job against `worktree` about `subject`,
/// with `env` on top of what its backend sets (the guard of a container
/// run's landing worktree, ADR-t2179-1 decision 8), its files named by
/// `stem` ([`start_review_program`], and a case of an agent's eval).
#[allow(clippy::too_many_arguments)]
pub(super) fn start_program_against(
    ports: &JobPorts<'_>,
    spawner: &dyn Spawner,
    backends: &ProgramBackends<'_>,
    program: &crate::application::review_programs::SnapshotProgram,
    (worktree, env): (&Path, &[(String, String)]),
    (output, scratch): (&Path, &Path),
    (stem, subject): (&str, JobSubject),
    timeout: Duration,
) -> Result<HeadlessJob> {
    let prepared = prepare_program(
        backends,
        program,
        (worktree, env),
        (output, scratch, stem),
        timeout,
    )?;
    start_program_job(
        ports,
        spawner,
        &prepared.command,
        (prepared.stdout, prepared.stderr),
        subject,
        prepared.timeout,
    )
}

/// A headless job's process (a review or a recovery job) whose stdout and stderr
/// go to files, waited for at most `timeout`.
pub struct HeadlessJob {
    /// What the job is, for its failure messages: `review`, `recovery job`.
    pub(super) what: &'static str,
    /// What runs it.
    pub(super) kind: JobKind,
    pub(super) child: Box<dyn Spawned>,
    /// When it started, on [`Self::clock`]'s monotonic clock.
    pub(super) started: Instant,
    /// The clock it started on, which its time is measured on.
    pub(super) clock: Arc<dyn Clock>,
    pub(super) timeout: Duration,
    pub(super) stdout: PathBuf,
    pub(super) stderr: PathBuf,
    /// Finds and ends what the job's process started, at its stop.
    pub(super) processes: Arc<dyn ProcessControl + Send + Sync>,
    /// Its `headless_jobs` row, until its end is handed to `ends`; `None`
    /// when the start could not be recorded.
    pub(super) record: Option<(JobEnds, i64)>,
    /// The provider an agent job runs on, whose implementation reads its
    /// reply, session and failure (ADR-t1063-1); unread for a program job.
    pub(super) provider: Provider,
    /// When it started (unix milliseconds), for the model of its session.
    pub(super) started_at: Option<i64>,
    /// Until it ends, where its end goes if it is stopped with no event to
    /// end it ([`Self::abandon`]), and what it is about.
    pub(super) unended: Option<(JobEnds, JobSubject)>,
    /// Of a run's job, its process as the run's until the job is dropped.
    #[expect(
        dead_code,
        reason = "held for its drop, which ends the process as its run's"
    )]
    pub(super) run_child: Option<RunChild>,
    /// What this supervisor, its executor, reads of its agent's stdout as
    /// it runs and sends as the steps of its session (ADR-t1662-3 decision
    /// 1, [`crate::application::step_report::JobSteps`]); none for a
    /// program's job or a target with no token.
    pub(super) steps: Option<Box<JobStepsOn>>,
    /// Whether it was stopped at its timeout: read again, its end is the
    /// timeout, not the kill's exit.
    pub(super) timed_out: bool,
}

/// What a job reads its agent's steps with ([`job_steps`]): the collector,
/// and the files its stdout is read through, at each poll and at its stop.
pub(in crate::application::supervise) struct JobStepsOn {
    steps: crate::application::step_report::JobSteps,
    files: Arc<dyn RunFiles>,
    /// The target of no run whose session only its agent's output names
    /// (an eval's job): the control side registers the session to it once
    /// named, while the job runs ([`Supervisor::register_named_session`]).
    registers: Option<crate::domain::session_step::StepTarget>,
    /// Whether the session named is registered to `registers`.
    registered: bool,
}

/// Whether a job that has not ended after running `ran` is waited for
/// still: until its `timeout`, at which it is stopped.
fn runs_on(ran: Duration, timeout: Duration) -> bool {
    ran < timeout
}

impl HeadlessJob {
    /// The provider the job runs on.
    pub(in crate::application::supervise) const fn provider(&self) -> Provider {
        self.provider
    }

    /// Send the steps of its session as `steps` reads them.
    pub(in crate::application::supervise) fn send_steps(&mut self, steps: Option<JobStepsOn>) {
        self.steps = steps.map(Box::new);
    }

    /// The session its agent's output named so far (a Codex thread), its
    /// output read to now: what the control side registers to its target
    /// before it stops the job (a handoff, the loop's end), so that the
    /// steps its stop sends are taken.
    pub(in crate::application::supervise) fn named_session(&mut self) -> Option<String> {
        // Read, not sent: the session is registered before its steps go.
        let now = self.elapsed_ms();
        let on = self.steps.as_mut()?;
        on.steps.read_on(&*on.files, &self.stdout, now);
        on.steps.session()
    }

    /// The session its agent named so far that the control side has yet
    /// to register to the job's target of no run, with that target
    /// ([`JobStepsOn::registers`]); its output is read to now.
    fn session_to_register(&mut self) -> Option<(crate::domain::session_step::StepTarget, String)> {
        let session = self.named_session()?;
        let on = self.steps.as_ref()?;
        let target = on.registers.clone().filter(|_| !on.registered)?;
        Some((target, session))
    }

    /// Milliseconds since it started.
    fn elapsed_ms(&self) -> i64 {
        i64::try_from(self.elapsed().as_millis()).unwrap_or(i64::MAX)
    }

    /// Read what its stdout got since the last look into its steps; at its
    /// end (an exit, its timeout, a stop for a handoff or a drop), read the
    /// rest, end the open steps and send, what could not be left to the
    /// sweeps' resend before its token is revoked.
    fn read_steps(&mut self, ended: bool) {
        let now = self.elapsed_ms();
        if ended {
            if let Some(on) = self.steps.take() {
                let JobStepsOn { steps, files, .. } = *on;
                steps.finish(&*files, &self.stdout, now);
            }
        } else if let Some(on) = self.steps.as_mut() {
            on.steps.look(&*on.files, &self.stdout, now);
        }
    }

    /// The id of the job's `headless_jobs` row; `None` when its start
    /// could not be recorded.
    pub(in crate::application::supervise) fn row_id(&self) -> Option<i64> {
        self.record.as_ref().map(|(_, id)| *id)
    }

    /// Where the job's stdout is written.
    pub(in crate::application::supervise) fn stdout(&self) -> &Path {
        &self.stdout
    }

    /// When the job started (unix milliseconds).
    pub(in crate::application::supervise) const fn started_at(&self) -> Option<i64> {
        self.started_at
    }

    /// How long it has run.
    pub(in crate::application::supervise) fn elapsed(&self) -> Duration {
        self.clock.since(self.started)
    }

    /// `Some` once the job ended: its reply, as `provider` (the one that
    /// started it) reads it out of its stdout (ADR-t1063-1 decision 2), or
    /// why it failed (a non-zero exit, or the timeout, after which the
    /// process is killed).
    pub(in crate::application::supervise) fn poll(
        &mut self,
        files: &dyn RunFiles,
        provider: &dyn AgentProvider,
    ) -> Result<Option<std::result::Result<String, String>>> {
        Ok(self
            .poll_end(files, provider)?
            .map(|output| output.map_err(JobFailed::into_error)))
    }

    /// [`Self::poll`] with how the job failed, for a caller that treats a
    /// non-zero exit and the timeout apart (a run's review).
    pub(super) fn poll_end(
        &mut self,
        files: &dyn RunFiles,
        provider: &dyn AgentProvider,
    ) -> Result<Option<std::result::Result<String, JobFailed>>> {
        Ok(self
            .poll_output(files)?
            .map(|output| output.map(|stdout| provider.job_reply(&stdout))))
    }

    /// `Some` once the job ended, whatever its kind: its stdout, or how it
    /// failed (a non-zero exit, or the timeout, after which it is
    /// [`Self::stop`]ped).
    pub fn poll_output(
        &mut self,
        files: &dyn RunFiles,
    ) -> Result<Option<std::result::Result<String, JobFailed>>> {
        let timed_out = JobFailed::TimedOut(format!(
            "the headless {} did not finish within {} seconds",
            self.what,
            self.timeout.as_secs()
        ));
        if self.timed_out {
            return Ok(Some(Err(timed_out)));
        }
        let status = match self.child.try_wait()? {
            Some(status) => status,
            None if runs_on(self.elapsed(), self.timeout) => {
                self.read_steps(false);
                return Ok(None);
            }
            None => {
                // Its steps are read to its stop.
                self.stop();
                self.timed_out = true;
                return Ok(Some(Err(timed_out)));
            }
        };
        self.read_steps(true);
        self.ended(headless_job::ENDED);
        if !status.success {
            let stderr = files.read_to_string(&self.stderr).unwrap_or_default();
            return Ok(Some(Err(JobFailed::Exited(format!(
                "the headless {} exited with {status}: {}",
                self.what,
                or_none(tail(stderr.trim(), 500))
            )))));
        }
        Ok(Some(Ok(files
            .read_to_string(&self.stdout)
            .unwrap_or_default())))
    }

    /// `Some` once a program job ended ([`ProgramEnd`]): what it exited
    /// with, or the timeout, after which it is [`Self::stop`]ped; with the
    /// ends of its output either way ([`program_end`], read as
    /// [`crate::application::review_programs::run_program`] reads them).
    pub fn poll_program(&mut self, files: &dyn RunFiles) -> Result<Option<ProgramEnd>> {
        let exit = match self.child.try_wait()? {
            // Its kill's exit, read again: still its timeout.
            Some(_) if self.timed_out => None,
            Some(status) => {
                self.ended(headless_job::ENDED);
                Some(status)
            }
            None if runs_on(self.elapsed(), self.timeout) => return Ok(None),
            None => {
                self.stop();
                self.timed_out = true;
                None
            }
        };
        Ok(Some(program_end(files, (&self.stdout, &self.stderr), exit)))
    }

    /// The provider whose adapter reads why the job failed
    /// ([`Supervisor::job_failure`]): an agent job's. No provider reads a
    /// program's output, so a program job's failure is `other`: it never
    /// holds a provider or stops at a wall.
    pub(super) fn failure_reader(&self) -> Option<Provider> {
        (self.kind == JobKind::Agent).then_some(self.provider)
    }

    /// The pid of the job's process.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Kill the job's process and the processes it started (a `claude -p`'s
    /// Bash and what that runs, a program's children), so none outlives
    /// the job: with its process group when it leads one (a program job).
    /// The caller records the job's end; one that records none abandons it.
    pub fn stop(&mut self) {
        let descendants = self.processes.descendants(self.child.id());
        let _ = self.child.kill_group();
        let _ = self.child.wait();
        for pid in descendants {
            let _ = self.processes.kill(pid);
        }
        // What its agent wrote up to its stop is its session's.
        self.read_steps(true);
        self.ended(headless_job::STOPPED);
    }

    /// Stop a job whose end no event records (a handoff, a slot the
    /// supervisor stops watching, a drop): as [`Self::stop`], and its end
    /// is written with the next ends as `headless_job_stopped` with the
    /// Execution its agent was so far (ADR-t1486-1). A job that ended
    /// already is left as it is.
    pub(in crate::application::supervise) fn abandon(&mut self) {
        let unended = self.unended.take();
        self.stop();
        if let Some((ends, subject)) = unended {
            ends.abandoned(Abandoned {
                subject,
                stdout: self.stdout.clone(),
                started_at: self.started_at,
            });
        }
    }

    fn ended(&mut self, outcome: &'static str) {
        self.unended = None;
        if let Some((ends, id)) = self.record.take() {
            ends.push(id, outcome);
        }
    }
}

/// What an agent job of `target` started by `agent` on `model` (its
/// launch's) sends of its session's steps: its provider's
/// [`crate::application::StepReader`] on its stdout, to the sink of
/// `target`'s token, the session the reader finds named.
fn job_steps(
    (agent, model, known): (
        &dyn AgentProvider,
        Option<&str>,
        crate::domain::command_shape::Subcommands,
    ),
    (sinks, files): (
        Arc<dyn crate::application::TelemetrySinks>,
        Arc<dyn RunFiles>,
    ),
    db: &Path,
    target: crate::domain::session_step::StepTarget,
    unsent: &Arc<crate::application::step_report::UnsentSteps>,
) -> JobStepsOn {
    // Its words are digested under the queue's key.
    let known = known.keyed(sinks.digest_key(db));
    JobStepsOn {
        steps: crate::application::step_report::JobSteps::new(
            agent.step_reader(model, known),
            crate::application::step_report::VerificationSteps::new(
                sinks,
                db.to_path_buf(),
                target,
                None,
            ),
            unsent.clone(),
        ),
        files,
        registers: None,
        registered: false,
    }
}

/// The subcommands the shapes of its executors' steps keep in plain
/// (ADR-t1673-1): the common ones and those of the profiles `verifier`'s
/// `dagq.toml` declares, the common ones alone when it cannot be read.
pub(in crate::application::supervise) fn known_subcommands(
    verifier: &dyn Verifier,
) -> crate::domain::command_shape::Subcommands {
    let toolchains = verifier
        .toolchains()
        .unwrap_or_else(|_| crate::domain::toolchain::Toolchains::none());
    crate::domain::command_shape::Subcommands::of(&toolchains)
}

/// The agent that starts and reads the headless jobs of `provider`: the
/// reviewer for Claude (none under `--no-claude`), the Codex this
/// supervisor found for Codex (`None` without one).
pub(super) fn job_agent_of<'a>(
    provider: Provider,
    no_claude: bool,
    reviewer: &'a dyn AgentProvider,
    codex_jobs: Option<&'a dyn AgentProvider>,
) -> Option<&'a dyn AgentProvider> {
    match provider {
        Provider::Claude => (!no_claude).then_some(reviewer),
        Provider::Codex => codex_jobs,
    }
}

impl<'a> Supervisor<'a> {
    /// [`job_agent_of`] on this supervisor's agents.
    pub(super) fn job_agent(&self, provider: Provider) -> Option<&'a dyn AgentProvider> {
        job_agent_of(provider, self.no_claude, self.reviewer, self.codex_jobs)
    }
}

impl HeadlessJob {
    /// What the job wrote to its stdout and stderr, for the wall it may
    /// have stopped at (task 438).
    pub(super) fn output(&self, files: &dyn RunFiles) -> String {
        let read = |path: &Path| files.read_to_string(path).unwrap_or_default();
        format!("{}\n{}", read(&self.stdout), read(&self.stderr))
    }
}

/// A job dropped before its end was read (an error on the way, a loop that
/// failed) is abandoned, so it neither runs on unwatched nor leaves its row
/// open while this process lives, and its Execution is recorded.
impl Drop for HeadlessJob {
    fn drop(&mut self) {
        if self.record.is_some() || self.unended.is_some() {
            self.abandon();
        }
    }
}

/// Unix milliseconds on `clock`, a job's `started_at`; `None` before the
/// epoch.
fn started_at_ms(clock: &dyn crate::application::Clock) -> Option<i64> {
    clock
        .system_time()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok())
}

/// The timeout of a job of the review stage's `stage` kind (`None` outside
/// the stage), from `[review.jobs]` (`review_jobs`) and the review timeouts
/// of its provider's agent (`agent`, the reviewer's without one) and of the
/// reviewer.
fn timeout_of(
    review_jobs: &crate::domain::headless_job::ReviewJobs,
    reviewer: &dyn AgentProvider,
    agent: Option<&dyn AgentProvider>,
    stage: Option<JobKind>,
) -> Duration {
    let reviewer = reviewer.review_timeout();
    let provider = agent.map_or(reviewer, |agent| agent.review_timeout());
    review_jobs.job_timeout(stage, provider, reviewer)
}

/// 実行と着地's headless jobs and providers, lent to a context that starts
/// a headless job of its own (計画管理's plan and goal reviews and its
/// planners): the agents that run a job, the record of its process and its
/// timeout, why a provider cannot be used, and the holds a job that failed
/// raises. The holds are 実行と着地's state: only these operations change
/// them.
pub(in crate::application::supervise) struct JobDesk<'s> {
    pub(super) reviewer: &'s dyn AgentProvider,
    pub(super) codex_jobs: Option<&'s dyn AgentProvider>,
    pub(super) signals: &'s dyn AgentSignals,
    pub(super) service_access: &'s dyn crate::application::queue_service::ServiceAccess,
    /// The telemetry tokens this supervisor issues the targets of no run it
    /// starts the agent of (ADR-t1662-3 decision 11).
    pub(super) telemetry_tokens: &'s dyn crate::application::queue_service::TelemetryTokens,
    /// Where its jobs send their sessions' steps, and what they could not
    /// send by their end ([`Supervisor::job_steps`]).
    pub(super) telemetry_sinks: Arc<dyn crate::application::TelemetrySinks>,
    pub(super) unsent_steps: &'s Arc<crate::application::step_report::UnsentSteps>,
    pub(super) no_claude: bool,
    pub(super) job_ends: &'s JobEnds,
    /// The processes this supervisor started for its runs in other trees.
    pub(super) run_children: &'s RunChildren,
    /// `[review.jobs]` as read at the start.
    pub(super) review_jobs: crate::domain::headless_job::ReviewJobs,
    pub(super) provider: &'s mut stages::ProviderState,
    pub(super) queue_hold: &'s mut Option<claim_hold::QueueHold>,
}

impl<'s> JobDesk<'s> {
    /// The tokens this supervisor issues for the queue service: a
    /// planner's wrapper's too, as the wrapper starts (ADR-t3102-1
    /// decision 2).
    pub(in crate::application::supervise) fn service_access(
        &self,
    ) -> &'s dyn crate::application::queue_service::ServiceAccess {
        self.service_access
    }

    /// The issue and the revocation of the telemetry token of a target of
    /// no run whose agent this supervisor starts (a planner, a plan or goal
    /// review), under its lease
    /// ([`crate::application::telemetry_tokens::TargetTelemetry`]).
    pub(in crate::application::supervise) fn target_telemetry<'e>(
        &'e self,
        env: &'e PassEnv<'_>,
    ) -> crate::application::telemetry_tokens::TargetTelemetry<'e> {
        crate::application::telemetry_tokens::TargetTelemetry {
            tokens: self.telemetry_tokens,
            db: &env.layout.db,
            lease: Some(env.token),
        }
    }

    /// What a job of `target` on `provider` with `model` sends of its
    /// session's steps ([`job_steps`]).
    pub(in crate::application::supervise) fn job_steps(
        &self,
        env: &PassEnv<'_>,
        (provider, model): (Provider, Option<&str>),
        target: crate::domain::session_step::StepTarget,
    ) -> Option<JobStepsOn> {
        Some(job_steps(
            (
                self.job_agent(provider)?,
                model,
                known_subcommands(&**env.verifier),
            ),
            (self.telemetry_sinks.clone(), env.files.clone()),
            &env.layout.db,
            target,
            self.unsent_steps,
        ))
    }

    /// Send again what the jobs could not send yet, before a target's token
    /// is revoked (after a Codex thread's registration at its end).
    pub(in crate::application::supervise) fn resend_steps(&self) {
        self.unsent_steps.resend();
    }

    /// The agent that runs the jobs on Claude.
    pub(in crate::application::supervise) fn reviewer(&self) -> &'s dyn AgentProvider {
        self.reviewer
    }

    /// What the sessions show and write.
    pub(in crate::application::supervise) fn signals(&self) -> &'s dyn AgentSignals {
        self.signals
    }

    /// Whether no job runs on Claude (`--no-claude`).
    pub(in crate::application::supervise) const fn no_claude(&self) -> bool {
        self.no_claude
    }

    /// [`job_agent_of`] on the supervisor's agents.
    pub(in crate::application::supervise) fn job_agent(
        &self,
        provider: Provider,
    ) -> Option<&'s dyn AgentProvider> {
        job_agent_of(provider, self.no_claude, self.reviewer, self.codex_jobs)
    }

    /// Whether the queue's open authentication or usage-limit ask holds
    /// the jobs (task 437).
    pub(in crate::application::supervise) const fn held(&self) -> bool {
        self.queue_hold.is_some()
    }

    /// `[provider_fallback] jobs` as last read (ADR-t1857-1).
    pub(in crate::application::supervise) const fn fallback_jobs(&self) -> bool {
        self.provider.fallback.jobs
    }

    /// Why `provider` is held now ([`provider_held_of`]).
    pub(in crate::application::supervise) fn provider_held(
        &self,
        provider: Provider,
    ) -> Option<SwitchReason> {
        provider_held_of(
            provider,
            self.no_claude,
            self.queue_hold
                .and_then(|hold| SwitchReason::of_hold(hold.reason)),
            crate::domain::provider_switch::own_hold(&self.provider.holds, provider),
        )
    }

    /// Why a headless job cannot start on `provider` now
    /// ([`job_unusable_of`]).
    pub(in crate::application::supervise) fn job_unusable(
        &self,
        provider: Provider,
    ) -> Option<SwitchReason> {
        job_unusable_of(
            self.provider_held(provider),
            self.job_agent(provider).is_some(),
        )
    }

    /// The actor executor that starts a job on `agent`.
    pub(in crate::application::supervise) fn actors_on<'e>(
        &'e self,
        env: &'e PassEnv<'_>,
        agent: &'e dyn AgentProvider,
    ) -> HostActorExecutor<'e> {
        HostActorExecutor::new(&env.layout.db)
            .with_no_claude(self.no_claude)
            .with_sessions(env.sessions)
            .with_provider(agent)
            .with_spawner(env.spawner)
            .with_queue_service(self.service_access)
            .with_events(&*env.queue)
    }

    /// The job whose process `child` just started ([`record_job`]), under
    /// its timeout (`[review.jobs]` and its provider's review timeout).
    pub(in crate::application::supervise) fn headless_job(
        &self,
        env: &mut PassEnv<'_>,
        what: &'static str,
        child: Box<dyn Spawned>,
        stdout: PathBuf,
        stderr: PathBuf,
        subject: JobSubject,
    ) -> HeadlessJob {
        let timeout = timeout_of(
            &self.review_jobs,
            self.reviewer,
            self.job_agent(subject.provider),
            subject.review_stage.then_some(subject.job),
        );
        let ports = JobPorts {
            store: &*env.queue,
            processes: env.processes.clone(),
            supervisor_token: env.token,
            clock: &env.generators.clock,
            ends: self.job_ends,
            children: self.run_children,
        };
        record_job(&ports, what, child, (stdout, stderr), subject, timeout)
    }

    /// Why `job` failed ([`job_failure_of`]). Only a job that failed is
    /// read (a non-zero exit, a timeout, a verdict that cannot be read;
    /// not one whose verdict was read and could not be applied), and the
    /// output of a job that succeeded never is: a job that quotes a wall's
    /// words in its work is no wall.
    pub(in crate::application::supervise) fn job_failure(
        &self,
        files: &dyn RunFiles,
        job: &HeadlessJob,
    ) -> JobFailure {
        match job.failure_reader() {
            None => JobFailure::Other,
            Some(provider) => job_failure_by(files, self.signals, self.job_agent(provider), job),
        }
    }

    /// Raise a headless job that failed at `wall` (task 438): the job joins
    /// the queue's open `authentication` ask (or usage-limit `cost` ask),
    /// or opens it, listed in its `affected` next to the runs, and records
    /// `auth_required` (or `usage_limited`) with `job` and its `entry` on
    /// its run, or on the queue for a job with none. The hold takes effect
    /// at once: no other headless job starts in this pass either. Its
    /// failure is no attention while the ask is unclosed
    /// ([`crate::domain::queue_hold::job_held`]). Whether it was raised: a
    /// hold that could not be written is logged, and the caller records
    /// the job's failure as any other.
    pub(super) fn raise_job_wall(
        &mut self,
        env: &mut PassEnv<'_>,
        wall: Wall,
        job: &HoldJob,
        error: &str,
    ) -> bool {
        match self.hold_job(env, wall, job, error) {
            Ok(()) => true,
            Err(hold_error) => {
                warn!(error = %format_args!("{hold_error:#}"), "the headless {} stopped at the {} wall, and its hold ask could not be written: {hold_error:#}", job.entry(), wall.as_str());
                false
            }
        }
    }

    fn hold_job(
        &mut self,
        env: &mut PassEnv<'_>,
        wall: Wall,
        job: &HoldJob,
        error: &str,
    ) -> Result<()> {
        let run = job.run_id().cloned();
        let (outcome, _) = ask::hold(
            &mut *env.queue,
            NewHold::wall(wall, run.clone(), Some(job.clone())),
        )?;
        if outcome.joined {
            let payload = json!({
                "job": job.kind(),
                "entry": job.entry(),
                "error": tail(error, 500),
                "ask_id": outcome.ask.id,
            });
            match &run {
                Some(run) => env
                    .queue
                    .record_runtime_event(run, wall.event_kind(), payload)?,
                None => {
                    env.queue.record_queue_event(wall.event_kind(), payload)?;
                }
            }
        }
        if self.queue_hold.is_none() {
            *self.queue_hold = crate::domain::queue_hold::hold_of(&outcome.ask);
        }
        warn!(ask_id = %outcome.ask.id, "the headless {} stopped at the {} wall: ask {} holds {} run(s) and job(s)", job.entry(), wall.as_str(), outcome.ask.id, outcome.ask.affected.len());
        Ok(())
    }

    /// A headless job of `provider` that failed with `failure` (its start
    /// or its output): Claude's login or usage limit raises the queue's
    /// hold ask as before (task 438), and, for a role that names its
    /// provider (`switchable`), a provider that cannot be used for another
    /// reason is held like a worker's (Codex's walls and any agent that
    /// did not start, ADR-t1063-1 decision 5). The provider and why, when
    /// the job moves to the other provider (ADR-t1063-1 decision 4), or,
    /// with `[provider_fallback] jobs` off, waits for this one to be
    /// usable again (ADR-t1857-1).
    /// `error` is the job's failure, `said` it with the job's output,
    /// which may say when a usage limit resets. A job runs once, so its
    /// route is chosen before it starts; one found unusable after it
    /// started is recorded as failed and starts again at a later pass, on
    /// the other provider or, with the fallback off, once this one can be
    /// used.
    pub(in crate::application::supervise) fn job_provider_failed(
        &mut self,
        env: &mut PassEnv<'_>,
        provider: Provider,
        failure: JobFailure,
        (error, said): (&str, &str),
        job: &HoldJob,
        switchable: bool,
    ) -> Option<(Provider, SwitchReason)> {
        let (wall, unusable) = job_failure_route(provider, failure, switchable);
        if let Some(wall) = wall {
            self.raise_job_wall(env, wall, job, error);
        }
        let (reason, hold) = unusable?;
        if hold && let Err(held) = self.hold_provider(env, provider, reason, None, said) {
            warn!(error = %format_args!("{held:#}"), "{} could not be held after the headless {} failed: {held:#}", provider.as_str(), job.entry());
        }
        Some((provider, reason))
    }
}

/// Why `job` failed, read by `agent` (the provider it ran on, when this
/// supervisor has it) for Codex or by Claude Code's `signals` otherwise.
fn job_failure_by(
    files: &dyn RunFiles,
    signals: &dyn AgentSignals,
    agent: Option<&dyn AgentProvider>,
    job: &HeadlessJob,
) -> JobFailure {
    match (job.provider, agent) {
        (Provider::Codex, Some(agent)) => {
            let read = |path: &Path| files.read_to_string(path).unwrap_or_default();
            agent.job_failure(&read(&job.stdout), &read(&job.stderr))
        }
        _ => signals.job_failure(&job.output(files)),
    }
}

/// Why a headless job cannot start on a provider `held` for that reason,
/// if it is, and whose agent this supervisor has (`has_agent`): its hold,
/// else no agent for it (no Codex found that runs).
pub(in crate::application::supervise) fn job_unusable_of(
    held: Option<SwitchReason>,
    has_agent: bool,
) -> Option<SwitchReason> {
    held.or((!has_agent).then_some(SwitchReason::ExecutableMissing))
}

/// Where a job whose `provider` could not be used is started again, as a
/// log line says it: on the other provider, or, with `[provider_fallback]
/// jobs` off (`fallback` false), on `provider` once its hold ends
/// (ADR-t1857-1).
pub(in crate::application::supervise) fn again_on(fallback: bool, provider: Provider) -> String {
    if fallback {
        "on the other provider".to_owned()
    } else {
        format!(
            "on {} once its hold ends ([provider_fallback] jobs is false)",
            provider.as_str()
        )
    }
}

/// What a headless job of `provider` that failed with `failure` leads to
/// (task 438, ADR-t1063-1 decisions 4 and 5): the wall Claude's job raises
/// the queue's hold ask for, and, for a role that names its provider
/// (`switchable`), why `provider` cannot be used and whether it is held for
/// that (not when the hold ask holds it already). `[provider_fallback] jobs`
/// (ADR-t1857-1) does not change either: whether the job moves to the other
/// provider or waits for `provider` is the caller's route.
pub(in crate::application::supervise) fn job_failure_route(
    provider: Provider,
    failure: JobFailure,
    switchable: bool,
) -> (Option<Wall>, Option<(SwitchReason, bool)>) {
    let wall = failure.wall().filter(|_| provider == Provider::Claude);
    let unusable = failure
        .switch_reason()
        .filter(|_| switchable)
        .map(|reason| (reason, wall.is_none()));
    (wall, unusable)
}

impl Supervisor<'_> {
    /// The loop's shared parts with 実行と着地's jobs and providers
    /// ([`JobDesk`]), borrowed apart.
    pub(super) fn job_desk(&mut self) -> (PassEnv<'_>, JobDesk<'_>) {
        (
            PassEnv {
                queue: &mut *self.queue,
                generators: &self.generators,
                layout: self.layout,
                processes: &self.processes,
                files: &self.files,
                repository: &self.repository,
                verifier: &self.verifier,
                spawner: self.spawner,
                sessions: self.sessions,
                token: self.registration.token(),
            },
            JobDesk {
                reviewer: self.reviewer,
                codex_jobs: self.codex_jobs,
                signals: self.signals,
                service_access: self.service_access,
                telemetry_tokens: self.telemetry_tokens,
                telemetry_sinks: self.telemetry_sinks.clone(),
                unsent_steps: &self.unsent_steps,
                no_claude: self.no_claude,
                job_ends: &self.job_ends,
                run_children: &self.run_children,
                review_jobs: self.review_jobs,
                provider: &mut self.provider,
                queue_hold: &mut self.queue_hold,
            },
        )
    }

    /// [`JobDesk::job_unusable`].
    pub(super) fn job_unusable(&self, provider: Provider) -> Option<SwitchReason> {
        job_unusable_of(
            self.provider_held(provider),
            self.job_agent(provider).is_some(),
        )
    }

    /// [`JobDesk::job_provider_failed`].
    pub(super) fn job_provider_failed(
        &mut self,
        provider: Provider,
        failure: JobFailure,
        said: (&str, &str),
        job: &HoldJob,
        switchable: bool,
    ) -> Option<(Provider, SwitchReason)> {
        let (mut env, mut jobs) = self.job_desk();
        jobs.job_provider_failed(&mut env, provider, failure, said, job, switchable)
    }

    /// The ports this supervisor's jobs start through ([`JobPorts`]).
    pub(super) fn job_ports(&self) -> JobPorts<'_> {
        JobPorts {
            store: &*self.queue,
            processes: self.processes.clone(),
            supervisor_token: self.registration.token(),
            clock: &self.generators.clock,
            ends: &self.job_ends,
            children: &self.run_children,
        }
    }

    /// The job whose process `child` just started ([`record_job`]), under
    /// its timeout ([`Self::job_timeout`]).
    pub(super) fn headless_job(
        &mut self,
        what: &'static str,
        child: Box<dyn Spawned>,
        stdout: PathBuf,
        stderr: PathBuf,
        subject: JobSubject,
    ) -> HeadlessJob {
        let timeout = self.job_timeout(&subject);
        // An agent job of no run of an eval's (its cases' labeling too) is a
        // target of no run by its row (ADR-t1662-3 decision 10): issued its
        // telemetry token once the row is written; the sweep revokes it once
        // the row ended or this supervisor is gone.
        let eval = subject.run_id.is_none()
            && subject.job == JobKind::Agent
            && subject.kind == headless_job::AGENT_EVAL;
        let (run_id, provider, agent_job, model) = (
            subject.run_id.clone(),
            subject.provider,
            subject.job == JobKind::Agent,
            subject.model.clone(),
        );
        let job = record_job(
            &self.job_ports(),
            what,
            child,
            (stdout, stderr),
            subject,
            timeout,
        );
        let mut job = job;
        // Its agent's session's steps are of its run, or of its eval's row.
        let target = match (&run_id, job.row_id()) {
            (Some(run), _) => Some(crate::domain::session_step::StepTarget::run(run)),
            (None, Some(id)) if eval => Some(crate::domain::session_step::StepTarget::of(
                crate::domain::session_step::TargetKind::EvalJob,
                id,
            )),
            _ => None,
        };
        if agent_job && let Some(target) = target {
            let steps = self.job_agent(provider).map(|agent| {
                job_steps(
                    (agent, model.as_deref(), known_subcommands(&*self.verifier)),
                    (self.telemetry_sinks.clone(), self.files.clone()),
                    &self.layout.db,
                    target.clone(),
                    &self.unsent_steps,
                )
            });
            // An eval's job's session is named by its output alone: it is
            // registered to the row once named (Self::register_named_session).
            job.send_steps(steps.map(|mut on| {
                on.registers = eval.then_some(target);
                on
            }));
        }
        if eval && let Some(id) = job.row_id() {
            crate::application::telemetry_tokens::TargetTelemetry {
                tokens: self.telemetry_tokens,
                db: &self.layout.db,
                lease: Some(self.registration.token()),
            }
            .issue(
                &*self.queue,
                &crate::domain::session_step::StepTarget::of(
                    crate::domain::session_step::TargetKind::EvalJob,
                    id,
                ),
            );
        }
        job
    }

    /// Register to its target of no run (an eval's job row) the session
    /// `job`'s agent named so far, once: at each poll of the job and before
    /// it is stopped, while the row runs and its token holds, so that the
    /// steps its end sends are taken (ADR-t1662-3 decision 10). The steps
    /// sent before are refused and kept, and its end sends them again. A
    /// registration that failed is tried at the next poll; the job's outcome
    /// does not hang on it.
    pub(super) fn register_named_session(&self, job: &mut HeadlessJob) {
        if let Some((target, session)) = job.session_to_register()
            && crate::application::telemetry_tokens::register_session(
                self.telemetry_tokens,
                &self.layout.db,
                &target,
                &session,
            )
            && let Some(on) = job.steps.as_mut()
        {
            on.registered = true;
        }
    }

    /// How long a job about `subject` may run
    /// ([`crate::domain::headless_job::ReviewJobs::job_timeout`]).
    pub(super) fn job_timeout(&self, subject: &JobSubject) -> Duration {
        self.timeout_of(
            subject.review_stage.then_some(subject.job),
            subject.provider,
        )
    }

    /// The timeout of a `kind` job of a run's review on `provider`
    /// ([`Self::job_timeout`]).
    pub(super) fn review_job_timeout(&self, kind: JobKind, provider: Provider) -> Duration {
        self.timeout_of(Some(kind), provider)
    }

    /// The timeout of a job of the review stage's `stage` kind (`None`
    /// outside the stage) on `provider`, from `[review.jobs]` and the
    /// review timeouts of `provider`'s agent and of the reviewer.
    fn timeout_of(&self, stage: Option<JobKind>, provider: Provider) -> Duration {
        timeout_of(
            &self.review_jobs,
            self.reviewer,
            self.job_agent(provider),
            stage,
        )
    }

    /// Why a headless job failed, in the classes shared by every provider
    /// (ADR-t1063-1 decision 4), as its provider's adapter reads the job's
    /// output: Claude Code's by its signals (task 438), another's by its
    /// agent.
    pub(super) fn job_failure(&self, job: &HeadlessJob) -> JobFailure {
        match job.failure_reader() {
            None => JobFailure::Other,
            Some(provider) => {
                job_failure_by(&*self.files, self.signals, self.job_agent(provider), job)
            }
        }
    }

    /// The wall only a person moves (a login that ran out, the usage
    /// limit) that the output of a headless job that failed shows it
    /// stopped at (ADR-0047 decision 42, task 438). Only Claude's walls
    /// hold the queue: a Codex job that stopped at one fails, and its
    /// failure takes the job's own manual path (ADR-t1207-1).
    pub(super) fn job_wall(&self, job: &HeadlessJob) -> Option<Wall> {
        self.job_failure(job)
            .wall()
            .filter(|_| job.provider == Provider::Claude)
    }

    /// [`JobDesk::raise_job_wall`].
    pub(super) fn raise_job_wall(&mut self, wall: Wall, job: &HoldJob, error: &str) -> bool {
        let (mut env, mut jobs) = self.job_desk();
        jobs.raise_job_wall(&mut env, wall, job, error)
    }

    /// Write the ends of this process's jobs, then stop the jobs gone
    /// supervisors left running (task 443): on every pass before any job
    /// starts, and the first time also those of this process's token,
    /// which an exec'd process knows nothing of.
    pub(super) fn tend_headless_jobs(&mut self) {
        self.write_job_ends();
        if self.held_left {
            // A kept end of one of them goes first (ADR-t3102-3 decision 5).
            info!(
                "the jobs of gone supervisors are taken over once the records kept for the queue service are sent"
            );
            return;
        }
        let own = !self.jobs_swept;
        let orphans = match self
            .queue
            .orphaned_headless_jobs(self.registration.token(), own)
        {
            Ok(orphans) => orphans,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the headless jobs of gone supervisors could not be read: {error:#}");
                return;
            }
        };
        self.jobs_swept = true;
        for orphan in orphans {
            if let Err(error) = self.take_over_job(&orphan) {
                warn!(error = %format_args!("{error:#}"), "the headless {} job {} (pid {}) of supervisor {} could not be taken over: {error:#}", orphan.kind, orphan.id, orphan.pid, orphan.supervisor_token);
            }
        }
    }

    /// Write the ends of this process's jobs: one the queue service did
    /// not answer, or a queue busy past its wait, is kept and written again
    /// on the next pass, and the queue ends a row once however often its
    /// end comes (ADR-t3102-3 decision 5). Whether none was kept.
    pub(super) fn write_job_ends(&mut self) -> bool {
        let (rows, abandoned) = self.job_ends.take();
        if !rows.is_empty() || !abandoned.is_empty() {
            // What their ends kept goes while their rows are open.
            self.resend_kept_steps();
        }
        let mut written = true;
        for (id, outcome) in rows {
            match self.queue.end_headless_job(id, outcome) {
                Err(error)
                    if crate::application::supervisor_service::unreachable(&error)
                        || crate::application::QueueBusy::is(&error) =>
                {
                    warn!(error = %format_args!("{error:#}"), "the end of headless job {id} is kept until the queue service answers: {error:#}");
                    self.job_ends.push(id, outcome);
                    written = false;
                }
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the end of headless job {id} could not be recorded: {error:#}");
                }
                Ok(_) => {}
            }
        }
        for job in abandoned {
            self.write_abandoned(&job);
        }
        written
    }

    /// Keep the ends not written yet with the records the queue service
    /// did not take, for the next supervisor to send (ADR-t3102-3 decision
    /// 5): the supervisor ends, and the queue ends each row once. The
    /// `headless_job_stopped` of the jobs just abandoned is kept with them,
    /// unsent, when the end of the loop gave up before it wrote them (an
    /// owner gone past its grace).
    pub(super) fn keep_job_ends(&mut self) {
        use crate::domain::queue_service::UseCase;
        let (rows, abandoned) = self.job_ends.take();
        for (id, outcome) in rows {
            let params = json!({"op": crate::application::supervisor_service::resend::JOB_END_OP, "args": [id, outcome]});
            match self.service.keep(UseCase::Supervisor, params) {
                Ok(()) => warn!("the end of headless job {id} is kept for the next supervisor"),
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the end of headless job {id} is lost as the supervisor ends: {error:#}");
                }
            }
        }
        for job in abandoned {
            let params = abandoned_record(&job, self.abandoned_payload(&job));
            if let Err(error) = self.service.keep(UseCase::Supervisor, params) {
                warn!(error = %format_args!("{error:#}"), "the stopped headless {} job (attempt {}) is lost as the supervisor ends: {error:#}", job.subject.kind, job.subject.attempt);
            }
        }
    }

    /// The payload of `headless_job_stopped` of `job`, with the Execution
    /// its output names ([`abandoned_end`]).
    fn abandoned_payload(&self, job: &Abandoned) -> serde_json::Value {
        let agent = self
            .job_agent(job.subject.provider)
            .unwrap_or(self.reviewer);
        let stdout = self.files.read_to_string(&job.stdout).unwrap_or_default();
        let session = agent.job_session(&stdout, job.started_at);
        abandoned_end(job, self.registration.token(), session.as_ref())
    }

    /// Record `headless_job_stopped` for a job this process stopped with
    /// no event to end it, on its run, or on the queue for a job with
    /// none, with the Execution its output names ([`abandoned_end`]).
    fn write_abandoned(&mut self, job: &Abandoned) {
        let payload = self.abandoned_payload(job);
        let recorded = match &job.subject.run_id {
            Some(run) => {
                self.queue
                    .record_runtime_event(run, EventKind::HeadlessJobStopped, payload)
            }
            None => self
                .queue
                .record_queue_event(EventKind::HeadlessJobStopped, payload)
                .map(|_| ()),
        };
        if let Err(error) = recorded {
            warn!(error = %format_args!("{error:#}"), "the stopped headless {} job (attempt {}) could not be recorded: {error:#}", job.subject.kind, job.subject.attempt);
        }
    }

    /// Close the row of a gone supervisor's job by what its pid runs now
    /// ([`close_taken_over`]): stop it when its pid still runs the job's
    /// process (the same start), with its descendants: SIGTERM, then
    /// SIGKILL after [`TAKEOVER_GRACE`]. A pid that runs another process
    /// now, or whose start cannot be told, is never signalled. Record
    /// `headless_job_stopped` with the Execution of its agent
    /// ([`Self::taken_over_execution`]); a job found gone or not the job
    /// gets one only to record that Execution.
    fn take_over_job(&mut self, job: &HeadlessJobRecord) -> Result<()> {
        // A heartbeat gone stale while its process lives (every heartbeat
        // is old right after the host wakes from sleep) is waited for: that
        // supervisor may still watch the job. This process's own pid is a
        // registration of this process (an in-process test's).
        if let Some(owner) = job.supervisor_pid
            && owner != self.layout.pid
            && self.processes.alive(owner)
        {
            return Ok(());
        }
        let alive = self.processes.alive(job.pid);
        let start = if alive {
            self.processes.start_identity(job.pid)
        } else {
            None
        };
        let judged = takeover(alive, job.process_start.as_deref(), start.as_deref());
        if judged == Takeover::NotTheJob {
            info!(
                "headless {} job {} of supervisor {}: pid {} runs another process now (started {}, the job's {}); it is left alone",
                job.kind,
                job.id,
                job.supervisor_token,
                job.pid,
                start.as_deref().unwrap_or("unknown"),
                job.process_start.as_deref().unwrap_or("unknown")
            );
        }
        let payload = close_taken_over(
            job,
            judged,
            |outcome| self.queue.end_headless_job(job.id, outcome),
            || {
                let stopped = self.stop_tree(job.pid, job.process_start.as_deref());
                info!(
                    "stopped the headless {} job {} (pid {}, attempt {}) that supervisor {} left running",
                    job.kind, job.id, job.pid, job.attempt, job.supervisor_token
                );
                stopped
            },
            || self.taken_over_execution(job),
        )?;
        let Some(payload) = payload else {
            return Ok(());
        };
        let recorded = match &job.run_id {
            Some(run) => {
                self.queue
                    .record_runtime_event(run, EventKind::HeadlessJobStopped, payload.clone())
            }
            None => Err(anyhow!("no run")),
        };
        if recorded.is_err()
            && let Err(error) = self
                .queue
                .record_queue_event(EventKind::HeadlessJobStopped, payload)
        {
            warn!(error = %format_args!("{error:#}"), "the end of headless job {} could not be recorded: {error:#}", job.id);
        }
        Ok(())
    }

    /// The Execution of the agent of `job`, a gone supervisor's job this
    /// one closed (ADR-t1486-1), as the provider of its row reads it out
    /// of the stdout the row names: `Some(None)` (not measured) when the
    /// row names none or it cannot be read; `None` when it is a program
    /// job, or an event already ended it with its Execution.
    fn taken_over_execution(
        &self,
        job: &HeadlessJobRecord,
    ) -> Option<Option<crate::domain::headless_job::JobSession>> {
        let provider = job.provider.parse::<Provider>().ok()?;
        let on = headless_job::JobOn {
            kind: &job.kind,
            label: job.label.as_deref(),
            run_id: job.run_id.as_ref(),
            proposal_id: job.proposal_id,
            goal_id: job.goal_id,
            attempt: job.attempt,
            started_at: job.started_at,
        };
        let ends = match &job.run_id {
            Some(run) => self.queue.run_events(run),
            None => on
                .ending_kinds()
                .into_iter()
                .try_fold(Vec::new(), |mut all, kind| {
                    all.extend(self.queue.latest_events_of(kind, TAKEOVER_ENDS_READ)?);
                    Ok(all)
                }),
        };
        match ends {
            Ok(ends) if on.execution_recorded(&ends) => return None,
            Ok(_) => {}
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the ends of headless job {} could not be read; its Execution is recorded: {error:#}", job.id);
            }
        }
        let agent = self.job_agent(provider).unwrap_or(self.reviewer);
        Some(taken_over_session(agent, &*self.files, job))
    }

    /// SIGTERM `pid` (whose process started at `start`) and its
    /// descendants, then SIGKILL what still runs after the grace, each only
    /// while its pid runs the process it ran when listed; the descendants
    /// and the pids killed.
    fn stop_tree(&self, pid: u32, start: Option<&str>) -> (Vec<u32>, Vec<u32>) {
        let descendants = self.processes.descendants(pid);
        let all: Vec<(u32, Option<String>)> = std::iter::once((pid, start.map(str::to_owned)))
            .chain(
                descendants
                    .iter()
                    .map(|&pid| (pid, self.processes.start_identity(pid))),
            )
            .collect();
        let same = |pid: u32, start: &Option<String>| {
            start.is_some() && self.processes.start_identity(pid) == *start
        };
        for (pid, start) in &all {
            if same(*pid, start) {
                let _ = self.processes.terminate(*pid);
            }
        }
        let started = self.generators.clock.monotonic();
        let running = |pid: u32| {
            // A job this process started before its exec is its child.
            self.processes.reap(pid);
            self.processes.alive(pid)
        };
        while all.iter().any(|(pid, _)| running(*pid))
            && !crate::application::waited_out(self.generators.clock.since(started), TAKEOVER_GRACE)
        {
            thread::sleep(Duration::from_millis(50));
        }
        let mut killed = Vec::new();
        for (pid, start) in &all {
            if running(*pid) && same(*pid, start) {
                let _ = self.processes.kill(*pid);
                self.processes.reap(*pid);
                killed.push(*pid);
            }
        }
        (descendants, killed)
    }
}

/// The agent jobs of a run's review in progress (ADR-t1895-1 decisions 2
/// to 5): its whole review's job and each selected agent's, each waiting,
/// running, answered or missed, with their output in the run directory
/// ([`ReviewJobId::output`]).
pub(super) struct ReviewWatch {
    pub(super) session: Option<SessionRef>,
    pub(super) attempt: usize,
    /// Whether this review is the retry of its program reviews (they could
    /// not start or ran past their time): it is the review's one retry, so
    /// none of its jobs runs once more ([`review_jobs::runs_again`]). The
    /// supervisor alone holds this, so a review it adopts counts afresh. A
    /// wait (`Phase::ReviewHeld`) keeps it.
    pub(super) retried: bool,
    /// When the review's jobs were first started, for its duration.
    pub(super) started: Instant,
    /// Its jobs: the whole review's first, then the agents' in their
    /// configured order.
    pub(super) jobs: Vec<ReviewJob>,
}

/// One agent job of a run's review.
pub(super) struct ReviewJob {
    pub(super) id: ReviewJobId,
    /// The provider an agent's `[review.subagents.<agent>]` names, which
    /// it runs on instead of where `[roles.review]` sends the review; `None`
    /// for the whole review's and an agent that names none.
    pub(super) provider: Option<Provider>,
    /// What starts it.
    pub(super) input: ReviewJobInput,
    /// A built-in's record of which built-in judges: the whole review's,
    /// `builtin:acceptance` (`review::acceptance_record`), and a built-in
    /// agent's (`SnapshotAgent::builtin_record`), which each of its
    /// `review_job_started` carries; `None` for a project's agent.
    pub(super) builtin: Option<serde_json::Value>,
    /// Whether it ran once more already, or starts as its run once more.
    /// The supervisor's state keeps it across the job's waits (for a held
    /// provider, for room under the limit of jobs at once), so a wait never
    /// gives a job a second run once more.
    pub(super) again: bool,
    pub(super) state: ReviewJobState,
    /// The session of its last run that ended, without its tokens (its
    /// `review_job_finished` holds its Execution): the whole review's names
    /// the review's end and span.
    pub(super) session: Option<crate::domain::headless_job::JobSession>,
}

/// What starts a job of a review: the whole review's prompt (and the
/// session id `review_started` named for its first start on Claude), or
/// an agent's job as `application::agent_job::build` built it from the
/// definition the landing branch's snapshot returned.
pub(super) enum ReviewJobInput {
    Whole {
        prompt: String,
        prompt_bytes: crate::application::prompt::PromptBytes,
        session_id: Option<String>,
    },
    Agent(crate::application::agent_job::BuiltAgentJob),
}

/// Where a job of a review is.
pub(super) enum ReviewJobState {
    /// Not started yet: it waits for the limit of jobs at once or for a
    /// provider it can use (why, once known, so a wait is logged once).
    Waiting(Option<String>),
    /// Its process runs; `switchable` when `[roles.review]`, or the
    /// agent's `[review.subagents.<agent>]`, names its provider, so one it
    /// cannot use moves it or waits for it (ADR-t1207-1, ADR-t1857-1).
    Running {
        job: Box<HeadlessJob>,
        switchable: bool,
    },
    /// It answered.
    Answered(ReviewAnswer),
    /// It gave no answer and will not run again: why, with where its
    /// output is, and whether a run of it wrote output (none did when it
    /// never started or no provider was left).
    Missed { why: String, ran: bool },
}

/// What a job of a review answered.
pub(super) enum ReviewAnswer {
    Whole(ReviewVerdict),
    Agent(crate::domain::review_subagents::AgentResult),
}

impl ReviewAnswer {
    /// The judgment it gave.
    pub(super) fn decision(&self) -> Option<crate::domain::ReviewDecision> {
        match self {
            Self::Whole(verdict) => Some(verdict.verdict),
            Self::Agent(result) => result.verdict,
        }
    }
}

impl ReviewWatch {
    /// How many of its jobs' processes run now, which the limit of jobs at
    /// once counts.
    pub(super) fn running(&self) -> usize {
        self.jobs
            .iter()
            .filter(|job| matches!(job.state, ReviewJobState::Running { .. }))
            .count()
    }

    /// Stop every job that runs with no event to end it
    /// ([`HeadlessJob::abandon`]): the supervisor stops watching the
    /// review.
    pub(super) fn abandon(&mut self) {
        for job in &mut self.jobs {
            if let ReviewJobState::Running { job, .. } = &mut job.state {
                job.abandon();
            }
        }
    }

    /// The sessions its running jobs' agents named so far
    /// ([`HeadlessJob::named_session`]).
    pub(super) fn named_sessions(&mut self) -> Vec<String> {
        self.jobs
            .iter_mut()
            .filter_map(|job| match &mut job.state {
                ReviewJobState::Running { job, .. } => job.named_session(),
                _ => None,
            })
            .collect()
    }

    /// Where the review stands from its jobs' ([`review_jobs::standing`]).
    pub(super) fn standing(&self) -> review_jobs::ReviewStanding {
        review_jobs::standing(self.jobs.iter().map(|job| match &job.state {
            ReviewJobState::Waiting(_) | ReviewJobState::Running { .. } => {
                review_jobs::JobStanding::Open
            }
            ReviewJobState::Answered(_) => review_jobs::JobStanding::Answered,
            ReviewJobState::Missed { why, .. } => review_jobs::JobStanding::Missed(why),
        }))
    }
}

/// The program reviews of a run's review that run before its agents
/// (ADR-t1895-2), one program job at a time.
pub(super) struct ProgramsWatch {
    pub(super) programs: ReviewPrograms,
    /// The job of the program at `programs.outcomes.len()`.
    pub(super) job: HeadlessJob,
}

/// Where a run's program reviews are: what they go before and run, and how
/// the programs that ran ended.
pub(super) struct ReviewPrograms {
    pub(super) session: Option<SessionRef>,
    /// The review attempt they go before: the next `review_started`'s.
    pub(super) attempt: usize,
    /// Which run of the run's program reviews this is, from 1 (one per
    /// `review_programs_started`): their jobs and output files are numbered
    /// by it, so a run after a revise or a retry keeps the earlier output.
    pub(super) round: usize,
    /// Whether this is their one retry after a program that could not
    /// start or ran past its time; it is the review's one retry
    /// ([`ReviewWatch::retried`]) for the agents' review after them too.
    pub(super) retried: bool,
    pub(super) snapshot: crate::application::review_programs::ProgramSnapshot,
    /// How each program that ran ended, in order.
    pub(super) outcomes: Vec<crate::domain::review_programs::ProgramOutcome>,
    /// What the worker or the person reads of the last program that did
    /// not pass: the reason it is sent back with, or why it failed.
    pub(super) detail: Option<String>,
}

impl ReviewPrograms {
    /// What the watch holds, for the step after its program ended. The
    /// session stays in the watch too until the step replaces it, so an
    /// error before that still finds the session to ask to exit.
    pub(super) fn take(&mut self) -> Self {
        Self {
            session: self.session.clone(),
            attempt: self.attempt,
            round: self.round,
            retried: self.retried,
            snapshot: self.snapshot.clone(),
            outcomes: std::mem::take(&mut self.outcomes),
            detail: self.detail.take(),
        }
    }
}

/// How a headless job ended without a reply, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobFailed {
    /// Its process exited non-zero.
    Exited(String),
    /// It did not finish within its timeout and was stopped.
    TimedOut(String),
}

impl JobFailed {
    /// How it ended, for whether it is retried ([`JobKind::retries`]).
    pub fn stop(&self) -> JobStop {
        match self {
            Self::Exited(_) => JobStop::Exited,
            Self::TimedOut(_) => JobStop::TimedOut,
        }
    }

    pub fn error(&self) -> &str {
        match self {
            Self::Exited(error) | Self::TimedOut(error) => error,
        }
    }

    pub fn into_error(self) -> String {
        match self {
            Self::Exited(error) | Self::TimedOut(error) => error,
        }
    }
}

/// The recovery job of a `failed` or `interrupted` run in progress, with
/// its output next to the run (see [`job_file`]).
pub(super) struct EndedRecovery {
    /// The round (`triage_started`'s `attempt`).
    pub(super) round: usize,
    pub(super) alert: RecoveryAlert,
    /// The job's number for its alert (`recovery_requested`'s `attempt`).
    pub(super) attempt: usize,
    /// Whether `[roles.recovery]` names its provider, so that a provider
    /// that cannot be used is held for the next jobs (ADR-t1063-1
    /// decision 4).
    pub(super) switchable: bool,
    pub(super) job: HeadlessJob,
}

impl EndedRecovery {
    pub(super) fn poll(
        &mut self,
        files: &dyn RunFiles,
        provider: &dyn AgentProvider,
    ) -> Result<Option<std::result::Result<RecoveryVerdict, String>>> {
        Ok(self
            .job
            .poll(files, provider)?
            .map(|output| output.and_then(|stdout| RecoveryVerdict::parse(&stdout))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{CommandSpec, Exit, memory_files::MemoryFiles};

    /// A job is waited for until 1 ms before its timeout and stopped at it.
    #[test]
    fn a_job_runs_on_until_its_timeout() {
        let timeout = Duration::from_secs(60);
        let ms = Duration::from_millis(1);
        assert!(runs_on(timeout - ms, timeout));
        assert!(!runs_on(timeout, timeout));
    }

    /// A process that has exited already, as `success` says.
    struct Ended {
        success: bool,
    }

    impl Spawned for Ended {
        fn id(&self) -> u32 {
            42
        }
        fn try_wait(&mut self) -> Result<Option<Exit>> {
            Ok(Some(Exit {
                success: self.success,
                code: Some(if self.success { 0 } else { 1 }),
                signal: None,
                description: format!("exit status: {}", if self.success { 0 } else { 1 }),
            }))
        }
        fn kill(&mut self) -> Result<()> {
            Ok(())
        }
        fn wait(&mut self) -> Result<Exit> {
            Ok(self.try_wait()?.expect("ended"))
        }
    }

    /// A process that runs until it is killed, and then says the kill.
    struct RunsUntilKilled {
        killed: bool,
    }

    impl Spawned for RunsUntilKilled {
        fn id(&self) -> u32 {
            43
        }
        fn try_wait(&mut self) -> Result<Option<Exit>> {
            Ok(self.killed.then(|| Exit {
                success: false,
                code: None,
                signal: Some(9),
                description: "signal: 9 (SIGKILL)".into(),
            }))
        }
        fn kill(&mut self) -> Result<()> {
            self.killed = true;
            Ok(())
        }
        fn kill_group(&mut self) -> Result<()> {
            self.kill()
        }
        fn wait(&mut self) -> Result<Exit> {
            Ok(self.try_wait()?.expect("killed"))
        }
    }

    /// A job runs on 1 ms before its timeout; at it, it is stopped and
    /// reads as timed out each time its end is read, not as the kill's
    /// exit: a step the queue service did not answer reads it again
    /// ([`super::super::reconcile`]). A program job gives no exit then, as
    /// when it was stopped.
    #[test]
    fn a_job_stopped_at_its_timeout_reads_as_timed_out_again() {
        let files = MemoryFiles::default();
        let ms = Duration::from_millis(1);
        let mut agent = job(&files, true, "");
        agent.child = Box::new(RunsUntilKilled { killed: false });
        agent.clock = Arc::new(Frozen(agent.started + agent.timeout - ms));
        assert!(agent.poll_output(&files).unwrap().is_none());
        agent.clock = Arc::new(Frozen(agent.started + agent.timeout));
        for _ in 0..2 {
            let end = agent.poll_output(&files).unwrap().unwrap();
            assert!(
                matches!(end, Err(JobFailed::TimedOut(_))),
                "{:?}",
                end.err()
            );
        }
        let mut program = job(&files, true, "");
        program.child = Box::new(RunsUntilKilled { killed: false });
        program.clock = Arc::new(Frozen(program.started + program.timeout - ms));
        assert!(program.poll_program(&files).unwrap().is_none());
        program.clock = Arc::new(Frozen(program.started + program.timeout));
        for _ in 0..2 {
            assert!(
                program
                    .poll_program(&files)
                    .unwrap()
                    .unwrap()
                    .exit
                    .is_none()
            );
        }
    }

    struct NoProcesses;

    impl ProcessControl for NoProcesses {
        fn alive(&self, _: u32) -> bool {
            false
        }
        fn terminate(&self, _: u32) -> Result<()> {
            Ok(())
        }
        fn interrupt(&self, _: u32) -> Result<()> {
            Ok(())
        }
        fn kill(&self, _: u32) -> Result<()> {
            Ok(())
        }
    }

    /// A provider whose job writes a line of JSON around its reply, the
    /// way an agent with a JSONL output does.
    struct Wrapped;

    impl AgentProvider for Wrapped {
        fn preflight(&self) -> Result<()> {
            Ok(())
        }
        fn review_command(
            &self,
            _: &TaskRun,
            _: &Path,
            _: &str,
            _: crate::domain::headless_job::JobAccess,
        ) -> Result<CommandSpec> {
            unreachable!()
        }
        fn job_reply(&self, stdout: &str) -> String {
            stdout
                .lines()
                .rev()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find_map(|line| line["text"].as_str().map(str::to_owned))
                .unwrap_or_default()
        }
    }

    /// The provider-independent reply of a Claude job.
    struct Plain;

    impl AgentProvider for Plain {
        fn preflight(&self) -> Result<()> {
            Ok(())
        }
        fn review_command(
            &self,
            _: &TaskRun,
            _: &Path,
            _: &str,
            _: crate::domain::headless_job::JobAccess,
        ) -> Result<CommandSpec> {
            unreachable!()
        }
    }

    /// A job started at [`origin`] on [`At`], to run for 60 s.
    fn job(files: &MemoryFiles, success: bool, stdout: &str) -> HeadlessJob {
        let clock = At(std::time::UNIX_EPOCH);
        let (out, err) = (PathBuf::from("/job/out"), PathBuf::from("/job/err"));
        files.put(&out, std::time::SystemTime::UNIX_EPOCH, stdout);
        files.put(&err, std::time::SystemTime::UNIX_EPOCH, "it broke\n");
        HeadlessJob {
            what: "review",
            kind: JobKind::Agent,
            child: Box::new(Ended { success }),
            started: clock.monotonic(),
            clock: Arc::new(clock),
            timeout: Duration::from_secs(60),
            stdout: out,
            stderr: err,
            processes: Arc::new(NoProcesses),
            record: None,
            provider: Provider::Claude,
            started_at: None,
            unended: None,
            run_child: None,
            steps: None,
            timed_out: false,
        }
    }

    /// A reader that makes a tool's call of each line and leaves a model's
    /// turn open until the end.
    struct LineSteps;

    impl crate::application::StepReader for LineSteps {
        fn line(&mut self, line: &str, at_ms: i64) -> Vec<crate::domain::session_step::Step> {
            let mut step = crate::domain::session_step::Step::new(
                0,
                crate::domain::session_step::StepKind::ToolCall,
                at_ms,
                0,
            );
            step.tool = Some(line.to_owned());
            vec![step]
        }
        fn finish(&mut self, at_ms: i64) -> Vec<crate::domain::session_step::Step> {
            vec![crate::domain::session_step::Step::new(
                0,
                crate::domain::session_step::StepKind::ModelTurn,
                0,
                at_ms,
            )]
        }
    }

    /// The reports a sink took, refusing while `down`.
    #[derive(Default)]
    struct Reports {
        taken: Mutex<Vec<crate::domain::telemetry::TelemetryReport>>,
        down: Mutex<bool>,
    }

    impl crate::application::TelemetrySink for Arc<Reports> {
        fn report(
            &self,
            report: &crate::domain::telemetry::TelemetryReport,
        ) -> Result<crate::domain::telemetry::ReportRecorded> {
            anyhow::ensure!(!*self.down.lock().unwrap(), "the service is unreachable");
            self.taken.lock().unwrap().push(report.clone());
            Ok(crate::domain::telemetry::ReportRecorded::default())
        }
    }

    impl crate::application::TelemetrySinks for Arc<Reports> {
        fn sink(
            &self,
            _: &Path,
            _: &crate::domain::session_step::StepTarget,
        ) -> Option<Box<dyn crate::application::TelemetrySink>> {
            Some(Box::new(self.clone()))
        }

        fn digest_key(&self, _: &Path) -> Option<crate::domain::command_shape::DigestKey> {
            Some(crate::domain::command_shape::DigestKey::test())
        }

        fn steps_file(
            &self,
            _: &Path,
            _: &crate::domain::session_step::StepTarget,
        ) -> Option<std::path::PathBuf> {
            None
        }
    }

    /// A job stopped before it ended (a handoff, the loop's end, a drop)
    /// reads its output to its stop, ends its open steps and sends them at
    /// once, though its interval has not passed; what its sink refused
    /// waits for the resend before its token is revoked.
    #[test]
    fn a_stopped_jobs_steps_are_read_to_its_stop_and_sent() {
        for down in [false, true] {
            let files = Arc::new(MemoryFiles::default());
            let mut job = job(&files, true, "");
            job.clock = Arc::new(Frozen(job.started));
            job.unended = Some((
                JobEnds::default(),
                JobSubject::run("review", &RunId::new("r1").unwrap(), 1),
            ));
            let reports = Arc::new(Reports::default());
            let unsent = Arc::new(crate::application::step_report::UnsentSteps::default());
            job.send_steps(Some(JobStepsOn {
                steps: crate::application::step_report::JobSteps::new(
                    Box::new(LineSteps),
                    crate::application::step_report::VerificationSteps::new(
                        Arc::new(reports.clone()),
                        PathBuf::from("/q/queue.db"),
                        crate::domain::session_step::StepTarget::run(&RunId::new("r1").unwrap()),
                        Some("s".to_owned()),
                    ),
                    unsent.clone(),
                ),
                files: files.clone(),
                registers: None,
                registered: false,
            }));
            files.put(
                &job.stdout.clone(),
                std::time::SystemTime::UNIX_EPOCH,
                "Read\n",
            );
            job.read_steps(false);
            // The rest it wrote before its stop, its last line unended.
            files.put(
                &job.stdout.clone(),
                std::time::SystemTime::UNIX_EPOCH,
                "Read\nBash\nEdit",
            );
            *reports.down.lock().unwrap() = down;
            job.abandon();
            assert!(job.steps.is_none());
            *reports.down.lock().unwrap() = false;
            if down {
                assert!(reports.taken.lock().unwrap().is_empty());
                assert_eq!(unsent.resend(), 0);
            }
            let taken = reports.taken.lock().unwrap();
            let steps: Vec<_> = taken
                .iter()
                .flat_map(|report| report.steps.clone())
                .collect();
            assert_eq!(
                steps
                    .iter()
                    .map(|step| (step.seq, step.tool.clone()))
                    .collect::<Vec<_>>(),
                [
                    (1, Some("Read".to_owned())),
                    (2, Some("Bash".to_owned())),
                    (3, Some("Edit".to_owned())),
                    (4, None),
                ],
                "down: {down}"
            );
            assert_eq!(
                steps[3].kind,
                crate::domain::session_step::StepKind::ModelTurn
            );
        }
    }

    /// A reader whose agent names its thread on its `thread` line.
    #[derive(Default)]
    struct NamingSteps(Option<String>);

    impl crate::application::StepReader for NamingSteps {
        fn line(&mut self, line: &str, at_ms: i64) -> Vec<crate::domain::session_step::Step> {
            if line == "thread" {
                self.0 = Some("t1".to_owned());
            }
            LineSteps.line(line, at_ms)
        }
        fn finish(&mut self, at_ms: i64) -> Vec<crate::domain::session_step::Step> {
            LineSteps.finish(at_ms)
        }
        fn session(&self) -> Option<String> {
            self.0.clone()
        }
    }

    /// Before a stop, the control side learns the thread the job's agent
    /// named in its output so far, read to now and not sent before its
    /// interval, so that its stop sends the steps under it.
    #[test]
    fn a_jobs_named_thread_is_read_before_its_stop_and_its_steps_go_under_it() {
        let files = Arc::new(MemoryFiles::default());
        let mut job = job(&files, true, "");
        job.clock = Arc::new(Frozen(job.started));
        let reports = Arc::new(Reports::default());
        job.send_steps(Some(JobStepsOn {
            steps: crate::application::step_report::JobSteps::new(
                Box::new(NamingSteps::default()),
                crate::application::step_report::VerificationSteps::new(
                    Arc::new(reports.clone()),
                    PathBuf::from("/q/queue.db"),
                    crate::domain::session_step::StepTarget::run(&RunId::new("r1").unwrap()),
                    None,
                ),
                Arc::new(crate::application::step_report::UnsentSteps::default()),
            ),
            files: files.clone(),
            registers: None,
            registered: false,
        }));
        assert_eq!(job.named_session(), None);
        files.put(
            &job.stdout.clone(),
            std::time::SystemTime::UNIX_EPOCH,
            "thread\nBash\n",
        );
        assert_eq!(job.named_session().as_deref(), Some("t1"));
        assert!(reports.taken.lock().unwrap().is_empty());
        job.stop();
        let taken = reports.taken.lock().unwrap();
        assert!(!taken.is_empty());
        assert!(taken.iter().all(|report| report.session_id == "t1"));
        assert_eq!(
            taken.iter().map(|report| report.steps.len()).sum::<usize>(),
            3
        );
    }

    /// An eval's job's session, named by its output alone, is the one the
    /// control side registers to the job's row, once named and until it is
    /// registered; a job of a target the control side registered ahead has
    /// none to register.
    #[test]
    fn an_eval_jobs_named_session_is_registered_to_its_row_once() {
        let files = Arc::new(MemoryFiles::default());
        let eval = crate::domain::session_step::StepTarget::of(
            crate::domain::session_step::TargetKind::EvalJob,
            7,
        );
        let on = |registers: Option<crate::domain::session_step::StepTarget>| JobStepsOn {
            steps: crate::application::step_report::JobSteps::new(
                Box::new(NamingSteps::default()),
                crate::application::step_report::VerificationSteps::new(
                    Arc::new(Arc::new(Reports::default())),
                    PathBuf::from("/q/queue.db"),
                    eval.clone(),
                    None,
                ),
                Arc::new(crate::application::step_report::UnsentSteps::default()),
            ),
            files: files.clone(),
            registers,
            registered: false,
        };
        let mut job = job(&files, true, "");
        job.clock = Arc::new(Frozen(job.started));
        job.send_steps(Some(on(Some(eval.clone()))));
        assert_eq!(job.session_to_register(), None);
        files.put(
            &job.stdout.clone(),
            std::time::SystemTime::UNIX_EPOCH,
            "thread\nBash\n",
        );
        assert_eq!(
            job.session_to_register(),
            Some((eval.clone(), "t1".to_owned()))
        );
        job.steps.as_mut().unwrap().registered = true;
        assert_eq!(job.session_to_register(), None);
        job.send_steps(Some(on(None)));
        assert_eq!(job.session_to_register(), None);
    }

    /// The read that learns the session to register sends nothing, even
    /// past the interval since the job's start: its steps go once it is
    /// registered, at the next look, never refused before.
    #[test]
    fn the_read_that_learns_a_jobs_session_sends_none_of_its_steps() {
        let files = Arc::new(MemoryFiles::default());
        let mut job = job(&files, true, "");
        job.clock = Arc::new(Frozen(
            job.started + Duration::from_secs(crate::domain::telemetry::REPORT_INTERVAL_SECS + 1),
        ));
        let reports = Arc::new(Reports::default());
        let eval = crate::domain::session_step::StepTarget::of(
            crate::domain::session_step::TargetKind::EvalJob,
            7,
        );
        job.send_steps(Some(JobStepsOn {
            steps: crate::application::step_report::JobSteps::new(
                Box::new(NamingSteps::default()),
                crate::application::step_report::VerificationSteps::new(
                    Arc::new(reports.clone()),
                    PathBuf::from("/q/queue.db"),
                    eval.clone(),
                    None,
                ),
                Arc::new(crate::application::step_report::UnsentSteps::default()),
            ),
            files: files.clone(),
            registers: Some(eval.clone()),
            registered: false,
        }));
        files.put(
            &job.stdout.clone(),
            std::time::SystemTime::UNIX_EPOCH,
            "thread\nBash\n",
        );
        assert_eq!(job.session_to_register(), Some((eval, "t1".to_owned())));
        assert!(reports.taken.lock().unwrap().is_empty());
        job.steps.as_mut().unwrap().registered = true;
        job.read_steps(false);
        let taken = reports.taken.lock().unwrap();
        assert!(!taken.is_empty());
        assert!(taken.iter().all(|report| report.session_id == "t1"));
    }

    /// A clock whose monotonic time stays at the time it is given: the
    /// time since a job started is a value, whatever the time the test
    /// takes.
    struct Frozen(Instant);

    impl crate::application::Clock for Frozen {
        fn system_time(&self) -> std::time::SystemTime {
            std::time::UNIX_EPOCH
        }

        fn monotonic(&self) -> Instant {
            self.0
        }
    }

    /// A provider whose reader makes a model's turn of each line, on the
    /// model it was given, and names the thread `th`, as Codex's output
    /// names no model.
    struct ModelTurns;

    impl AgentProvider for ModelTurns {
        fn preflight(&self) -> Result<()> {
            Ok(())
        }
        fn review_command(
            &self,
            _: &TaskRun,
            _: &Path,
            _: &str,
            _: crate::domain::headless_job::JobAccess,
        ) -> Result<CommandSpec> {
            unreachable!()
        }
        fn step_reader(
            &self,
            model: Option<&str>,
            _: crate::domain::command_shape::Subcommands,
        ) -> Box<dyn crate::application::StepReader> {
            Box::new(TurnsOn(model.map(str::to_owned)))
        }
    }

    struct TurnsOn(Option<String>);

    impl crate::application::StepReader for TurnsOn {
        fn line(&mut self, _: &str, at_ms: i64) -> Vec<crate::domain::session_step::Step> {
            let mut step = crate::domain::session_step::Step::new(
                0,
                crate::domain::session_step::StepKind::ModelTurn,
                at_ms,
                0,
            );
            step.model.clone_from(&self.0);
            vec![step]
        }
        fn finish(&mut self, _: i64) -> Vec<crate::domain::session_step::Step> {
            Vec::new()
        }
        fn session(&self) -> Option<String> {
            Some("th".to_owned())
        }
    }

    /// A Codex job's model's turns carry the model its launch names, which
    /// its output does not, and no step carries a token's usage (the
    /// Execution keeps it, ADR-t1486-1).
    #[test]
    fn a_codex_jobs_model_turns_carry_its_launchs_model_and_no_usage() {
        let files = Arc::new(MemoryFiles::default());
        let mut job = job(&files, true, "");
        job.clock = Arc::new(Frozen(job.started));
        job.provider = Provider::Codex;
        let reports = Arc::new(Reports::default());
        job.send_steps(Some(job_steps(
            (
                &ModelTurns,
                Some("gpt-5.5-codex"),
                crate::domain::command_shape::Subcommands::common(),
            ),
            (Arc::new(reports.clone()), files.clone()),
            Path::new("/q/queue.db"),
            crate::domain::session_step::StepTarget::run(&RunId::new("r1").unwrap()),
            &Arc::new(crate::application::step_report::UnsentSteps::default()),
        )));
        files.put(
            &job.stdout.clone(),
            std::time::SystemTime::UNIX_EPOCH,
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":9}}\n",
        );
        job.stop();
        let taken = reports.taken.lock().unwrap();
        let steps: Vec<_> = taken
            .iter()
            .flat_map(|report| report.steps.clone())
            .collect();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].model.as_deref(), Some("gpt-5.5-codex"));
        let sent = serde_json::to_string(&*taken).unwrap();
        for usage in ["usage", "tokens", "input_tokens"] {
            assert!(!sent.contains(usage), "{usage} in {sent}");
        }
    }

    /// The `headless_jobs` rows written, and nothing else.
    #[derive(Default)]
    struct Rows(Mutex<Vec<NewHeadlessJob>>);

    impl HeadlessJobStore for Rows {
        fn record_headless_job(&self, job: &NewHeadlessJob) -> Result<i64> {
            let mut rows = self.0.lock().unwrap();
            rows.push(job.clone());
            Ok(rows.len() as i64)
        }
        fn end_headless_job(&self, _: i64, _: &str) -> Result<bool> {
            Ok(true)
        }
        fn orphaned_headless_jobs(
            &self,
            _: &LeaseToken,
            _: bool,
        ) -> Result<Vec<HeadlessJobRecord>> {
            Ok(Vec::new())
        }
    }

    /// A run's job is its run's process from its start until the job is
    /// dropped, so the measure of the run's resources counts it with the
    /// run's; a job of no run (an agent eval's program) is no run's.
    #[test]
    fn a_runs_job_is_its_process_until_the_job_is_dropped() {
        let rows = Rows::default();
        let ends = JobEnds::default();
        let children = RunChildren::default();
        let token = LeaseToken::new("me");
        let clock: Arc<dyn Clock> = Arc::new(At(std::time::UNIX_EPOCH));
        let ports = JobPorts {
            store: &rows,
            processes: Arc::new(NoProcesses),
            supervisor_token: &token,
            clock: &clock,
            ends: &ends,
            children: &children,
        };
        let run = RunId::new("run-1").unwrap();
        let output = (PathBuf::from("/job/out"), PathBuf::from("/job/err"));
        let start = |subject| {
            record_job(
                &ports,
                "review program",
                Box::new(Ended { success: true }),
                output.clone(),
                subject,
                Duration::from_secs(30),
            )
        };
        let job = start(JobSubject::review_program(&run, 1, "fmt"));
        assert_eq!(children.of(&run), [42]);
        let eval = start(JobSubject::eval_program("case".to_owned(), 1));
        assert_eq!(children.of(&run), [42]);
        drop(eval);
        drop(job);
        assert!(children.of(&run).is_empty());
    }

    /// A program job of the review (ADR-t1895-1 decision 1) goes the
    /// common way with what its kind changes: its row names the program
    /// and no provider, it is bounded by the timeout it is given, its
    /// reply is its stdout as it is (no provider reads it out), and its
    /// failure is no provider's: it holds no provider and stops at no wall.
    #[test]
    fn a_program_job_is_recorded_read_and_classed_as_no_providers() {
        let rows = Rows::default();
        let ends = JobEnds::default();
        let token = LeaseToken::new("me");
        let clock: Arc<dyn Clock> = Arc::new(At(std::time::UNIX_EPOCH));
        let ports = JobPorts {
            store: &rows,
            processes: Arc::new(NoProcesses),
            supervisor_token: &token,
            clock: &clock,
            ends: &ends,
            children: &RunChildren::default(),
        };
        let run = RunId::new("run-1").unwrap();
        let files = MemoryFiles::default();
        let output = (PathBuf::from("/job/out"), PathBuf::from("/job/err"));
        files.put(
            &output.0,
            std::time::SystemTime::UNIX_EPOCH,
            "{\"text\": \"ok\"}\n",
        );
        let mut program = record_job(
            &ports,
            "review program",
            Box::new(Ended { success: true }),
            output.clone(),
            JobSubject::review_program(&run, 2, "fmt"),
            Duration::from_secs(30),
        );
        let agent = record_job(
            &ports,
            "review",
            Box::new(Ended { success: true }),
            output,
            JobSubject {
                provider: Provider::Codex,
                ..JobSubject::review(JobKind::Agent, &run, 2)
            },
            Duration::from_secs(600),
        );
        let written: Vec<_> = rows
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|row| (row.kind, row.label.clone(), row.provider, row.attempt))
            .collect();
        assert_eq!(
            written,
            [
                ("review_program", Some("fmt".to_owned()), None, 2),
                ("review", None, Some(Provider::Codex), 2),
            ]
        );
        assert_eq!(program.timeout, Duration::from_secs(30));
        assert_eq!(agent.timeout, Duration::from_secs(600));
        assert_eq!(program.failure_reader(), None);
        assert_eq!(agent.failure_reader(), Some(Provider::Codex));
        assert_eq!(
            program.poll_output(&files).unwrap().unwrap().unwrap(),
            "{\"text\": \"ok\"}\n"
        );
        // Stopped with no event to end it, an agent job hands its
        // Execution's end over; a program job, which is no Execution, only
        // its row's.
        drop(agent);
        let mut stray = record_job(
            &ports,
            "review program",
            Box::new(Ended { success: true }),
            (PathBuf::from("/job/out"), PathBuf::from("/job/err")),
            JobSubject::review_program(&run, 2, "lint"),
            Duration::from_secs(30),
        );
        stray.abandon();
        let (rows, handed) = ends.take();
        assert_eq!(rows, [(1, "ended"), (2, "stopped"), (3, "stopped")]);
        let handed: Vec<_> = handed.iter().map(|job| job.subject.kind).collect();
        assert_eq!(handed, ["review"]);
        // A program that exits non-zero is its check's result, not retried.
        let mut failed = job(&files, false, "");
        failed.kind = JobKind::Program;
        let end = failed.poll_output(&files).unwrap().unwrap().unwrap_err();
        assert_eq!(end.stop(), JobStop::Exited);
        assert!(!JobKind::Program.retries(end.stop()));
        assert!(JobKind::Agent.retries(end.stop()));
    }

    /// The job reads its verdict from the reply its provider reads out of
    /// the output (ADR-t1063-1 decision 2), whatever that output is.
    #[test]
    fn a_job_reads_the_reply_its_provider_reads_out() {
        let files = MemoryFiles::default();
        let verdict = r#"{"verdict":"pass","summary":"ok"}"#;
        let wrapped = format!(
            "{}\n{}\n",
            serde_json::json!({"type": "started"}),
            serde_json::json!({"type": "reply", "text": verdict})
        );
        let reply = job(&files, true, &wrapped)
            .poll(&files, &Wrapped)
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(reply, verdict);
        // A provider whose job prints its reply only gives stdout back, as
        // before jobs named their provider's reply.
        let plain = format!("Looked at it.\n{verdict}\n");
        let reply = job(&files, true, &plain)
            .poll(&files, &Plain)
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(reply, plain);
        // A job that failed is its failure, not a reply.
        let failed = job(&files, false, &wrapped)
            .poll(&files, &Wrapped)
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(failed.contains("it broke"), "{failed}");
    }

    /// A job stopped with no event to end it (abandoned, or dropped while
    /// it ran) hands its end to the next ends, once; a job stopped by a
    /// caller that records its end, or one that ended, hands nothing.
    #[test]
    fn an_abandoned_or_dropped_job_hands_its_end_over_once() {
        let files = MemoryFiles::default();
        let ends = JobEnds::default();
        let unended = |job: &mut HeadlessJob, attempt| {
            job.started_at = Some(7);
            job.unended = Some((
                ends.clone(),
                JobSubject {
                    provider: Provider::Codex,
                    ..JobSubject::run(headless_job::REVIEW, &RunId::new("r1").unwrap(), attempt)
                },
            ));
        };
        let mut abandoned = job(&files, true, "");
        unended(&mut abandoned, 1);
        abandoned.abandon();
        abandoned.abandon();
        drop(abandoned);
        let mut dropped = job(&files, true, "");
        unended(&mut dropped, 2);
        drop(dropped);
        let mut stopped = job(&files, true, "");
        unended(&mut stopped, 3);
        stopped.stop();
        drop(stopped);
        let mut ended = job(&files, true, "");
        unended(&mut ended, 4);
        ended.poll(&files, &Plain).unwrap().unwrap().unwrap();
        drop(ended);
        let (_, handed) = ends.take();
        let attempts: Vec<usize> = handed.iter().map(|job| job.subject.attempt).collect();
        assert_eq!(attempts, [1, 2]);
        assert_eq!(handed[0].stdout, PathBuf::from("/job/out"));
        assert_eq!(handed[0].started_at, Some(7));
    }

    /// The slots' jobs a loop leaves running are abandoned at its end, so
    /// their ends are handed over before its last write; a slot without a
    /// job hands nothing.
    #[test]
    fn the_slots_jobs_are_abandoned_when_the_loop_ends() {
        let files = MemoryFiles::default();
        let ends = JobEnds::default();
        let run = super::super::recovery::test_run(RunStatus::AwaitingIntegration, None);
        // A review's whole job and an agent's run; a third waits.
        let review_job = |id: ReviewJobId, state: ReviewJobState| ReviewJob {
            id,
            provider: None,
            input: ReviewJobInput::Whole {
                prompt: String::new(),
                prompt_bytes: crate::application::prompt::PromptBytes::default(),
                session_id: None,
            },
            builtin: None,
            again: false,
            state,
            session: None,
        };
        let running = |label: Option<&str>| {
            let mut job = job(&files, true, "");
            job.unended = Some((
                ends.clone(),
                JobSubject {
                    label: label.map(str::to_owned),
                    ..JobSubject::run(headless_job::REVIEW, run.id(), 1)
                },
            ));
            ReviewJobState::Running {
                job: Box::new(job),
                switchable: false,
            }
        };
        let mut slots = super::super::stages::SlotTable::default();
        slots.admit(Slot::new(
            run.clone(),
            Phase::Review(ReviewWatch {
                session: None,
                attempt: 1,
                retried: false,
                started: origin(),
                jobs: vec![
                    review_job(ReviewJobId::Whole, running(None)),
                    review_job(ReviewJobId::Agent("design".into()), running(Some("design"))),
                    review_job(
                        ReviewJobId::Agent("tests".into()),
                        ReviewJobState::Waiting(None),
                    ),
                ],
            }),
        ));
        slots.admit(Slot::new(run, Phase::AwaitingSlot));
        slots.abandon_jobs();
        let (_, handed) = ends.take();
        let labels: Vec<_> = handed
            .iter()
            .map(|job| (job.subject.kind, job.subject.label.clone()))
            .collect();
        assert_eq!(
            labels,
            [
                (headless_job::REVIEW, None),
                (headless_job::REVIEW, Some("design".to_owned()))
            ]
        );
        // The slot dropped later hands nothing again.
        drop(slots);
        assert!(ends.take().1.is_empty());
    }

    /// The `headless_job_stopped` of an abandoned job says what the job
    /// was and records its Execution: the tokens its output gave, or not
    /// measured.
    #[test]
    fn an_abandoned_jobs_end_records_its_execution() {
        use crate::domain::{
            headless_job::JobSession,
            tokens::{ExecutionTokens, TokenSource, TokenUsage},
        };
        let job = Abandoned {
            subject: JobSubject {
                kind: headless_job::PLAN_REVIEW,
                job: JobKind::Agent,
                review_stage: false,
                label: None,
                run_id: None,
                proposal_id: Some(crate::domain::ProposalId::new(4)),
                goal_id: None,
                attempt: 2,
                provider: Provider::Codex,
                model: None,
            },
            stdout: PathBuf::from("/job/out"),
            started_at: Some(1_700_000_000_123),
        };
        let token = LeaseToken::new("sv");
        let measured = JobSession {
            tokens: Some(ExecutionTokens {
                tokens: Some(TokenUsage {
                    input: 20,
                    output: 4,
                    ..TokenUsage::default()
                }),
                source: Some(TokenSource::UsageRecord),
                ..ExecutionTokens::default()
            }),
            ..JobSession::default()
        };
        let end = abandoned_end(&job, &token, Some(&measured));
        assert_eq!(end["kind"], "plan_review");
        assert_eq!(end["proposal_id"], 4);
        assert_eq!(end["attempt"], 2);
        assert_eq!(end["provider"], "codex");
        assert_eq!(end["supervisor"], "sv");
        assert_eq!(end["started_at"], 1_700_000_000);
        assert_eq!(end["tokens"]["input"], 20);
        assert_eq!(end["tokens_source"], "token_usage_record");
        for session in [None, Some(JobSession::default())] {
            let end = abandoned_end(&job, &token, session.as_ref());
            assert!(end["tokens"].is_null(), "{end}");
            assert!(end.get("tokens_source").is_some(), "{end}");
            assert_eq!(end["tokens_reason"], "tokens_not_read");
        }
    }

    /// The `headless_job_stopped` of an abandoned job kept unsent, past an
    /// owner's grace, is the supervisor's operation that records it: on its
    /// run, or on the queue for a job with none, with its arguments as the
    /// service reads them.
    #[test]
    fn an_abandoned_jobs_end_kept_unsent_is_the_record_the_service_reads() {
        let payload = serde_json::json!({"kind": "review", "attempt": 1});
        let run = RunId::new("r1").unwrap();
        let on_run = Abandoned {
            subject: JobSubject::run(headless_job::REVIEW, &run, 1),
            stdout: PathBuf::from("/job/out"),
            started_at: None,
        };
        let params = abandoned_record(&on_run, payload.clone());
        assert_eq!(params["op"], "record_runtime_event");
        let read: (RunId, EventKind, serde_json::Value) =
            serde_json::from_value(params["args"].clone()).unwrap();
        assert_eq!(read, (run, EventKind::HeadlessJobStopped, payload.clone()));
        let on_queue = Abandoned {
            subject: JobSubject {
                run_id: None,
                ..on_run.subject
            },
            ..on_run
        };
        let params = abandoned_record(&on_queue, payload.clone());
        assert_eq!(params["op"], "record_queue_event");
        let read: (EventKind, serde_json::Value) =
            serde_json::from_value(params["args"].clone()).unwrap();
        assert_eq!(read, (EventKind::HeadlessJobStopped, payload));
    }

    /// A clock stopped at a fixed time, its monotonic time at [`origin`].
    struct At(std::time::SystemTime);

    impl crate::application::Clock for At {
        fn system_time(&self) -> std::time::SystemTime {
            self.0
        }

        fn monotonic(&self) -> std::time::Instant {
            origin()
        }
    }

    /// The one monotonic time the tests start from, read once for the
    /// process: a clock gives it, or an offset from it, as a value, so how
    /// long a test takes decides nothing.
    fn origin() -> Instant {
        static ORIGIN: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
        *ORIGIN
    }

    /// A job's `started_at` is the injected clock's time in Unix
    /// milliseconds, not the wall clock's (architecture.md L4).
    #[test]
    fn a_jobs_started_at_comes_from_the_injected_clock() {
        let clock = At(std::time::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123));
        assert_eq!(started_at_ms(&clock), Some(1_700_000_000_123));
        let before = At(std::time::UNIX_EPOCH - Duration::from_secs(1));
        assert_eq!(started_at_ms(&before), None);
    }

    /// The `headless_job_stopped` of a gone supervisor's job this one
    /// stopped says what it stopped and records the job's Execution under
    /// the provider of its row: the tokens its stdout gave, or not
    /// measured when its stdout was unknown, unreadable or gave none. A
    /// program job, or one an end recorded already, gets none.
    #[test]
    fn a_taken_over_jobs_end_records_its_execution() {
        use crate::domain::{
            headless_job::JobSession,
            tokens::{ExecutionTokens, TokenSource, TokenUsage},
        };
        let job = HeadlessJobRecord {
            id: 9,
            kind: headless_job::REVIEW.into(),
            label: None,
            run_id: Some(RunId::new("r1").unwrap()),
            proposal_id: None,
            goal_id: None,
            attempt: 2,
            provider: "codex".into(),
            pid: 40,
            process_start: Some("Sun Sep 27 10:00:00 2026".into()),
            supervisor_token: LeaseToken::new("gone"),
            supervisor_pid: None,
            started_at: 1_700_000_000,
            stdout: Some(PathBuf::from("/runs/r1/review-2.out")),
        };
        let measured = JobSession {
            tokens: Some(ExecutionTokens {
                tokens: Some(TokenUsage {
                    input: 30,
                    output: 5,
                    ..TokenUsage::default()
                }),
                source: Some(TokenSource::UsageRecord),
                ..ExecutionTokens::default()
            }),
            ..JobSession::default()
        };
        let end = taken_over_end(&job, (vec![41], vec![41]), Some(Some(&measured)));
        assert_eq!(end["headless_job_id"], 9);
        assert_eq!(end["kind"], "review");
        assert_eq!(end["descendants"], json!([41]));
        assert_eq!(end["supervisor"], "gone");
        assert_eq!(end["provider"], "codex");
        assert_eq!(end["tokens"]["input"], 30);
        assert_eq!(end["tokens_source"], "token_usage_record");
        for session in [None, Some(JobSession::default())] {
            let end = taken_over_end(&job, (vec![], vec![]), Some(session.as_ref()));
            assert!(end["tokens"].is_null(), "{end}");
            assert!(end.get("tokens_source").is_some(), "{end}");
            assert_eq!(end["tokens_reason"], "tokens_not_read");
        }
        // Recorded already: no second Execution.
        let end = taken_over_end(&job, (vec![], vec![]), None);
        assert!(end.get("tokens_source").is_none(), "{end}");
        let program = HeadlessJobRecord {
            kind: headless_job::REVIEW_PROGRAM.into(),
            provider: headless_job::NO_PROVIDER.into(),
            ..job
        };
        let end = taken_over_end(&program, (vec![], vec![]), None);
        assert!(end.get("provider").is_none(), "{end}");
        assert!(end.get("tokens_source").is_none(), "{end}");
    }

    /// A gone supervisor's agent job found gone, or not the job (its pid
    /// runs another process, or its start cannot be told), is closed with
    /// its outcome and never stopped, and its end records its Execution:
    /// the tokens its output gave, or not measured. One whose Execution an
    /// end recorded already, or a program job (no provider to read it),
    /// gets no end; nor does a row another supervisor closed first, which
    /// is neither stopped nor read.
    #[test]
    fn a_gone_or_not_the_jobs_end_records_its_execution_and_is_not_stopped() {
        use crate::domain::{
            headless_job::JobSession,
            tokens::{ExecutionTokens, TokenSource, TokenUsage},
        };
        use std::cell::{Cell, RefCell};
        let job = HeadlessJobRecord {
            id: 9,
            kind: headless_job::RECOVERY.into(),
            label: None,
            run_id: Some(RunId::new("r1").unwrap()),
            proposal_id: None,
            goal_id: None,
            attempt: 1,
            provider: "claude".into(),
            pid: 40,
            process_start: Some("Sun Sep 27 10:00:00 2026".into()),
            supervisor_token: LeaseToken::new("gone"),
            supervisor_pid: None,
            started_at: 1_700_000_000,
            stdout: Some(PathBuf::from("/runs/r1/recovery-1.out")),
        };
        let measured = JobSession {
            tokens: Some(ExecutionTokens {
                tokens: Some(TokenUsage {
                    input: 30,
                    output: 5,
                    ..TokenUsage::default()
                }),
                source: Some(TokenSource::ModelUsage),
                ..ExecutionTokens::default()
            }),
            ..JobSession::default()
        };
        let close = |judged: Takeover,
                     closed: bool,
                     ran: Option<Option<JobSession>>|
         -> (Option<Value>, Vec<&'static str>, bool) {
            let ends = RefCell::new(Vec::new());
            let stopped = Cell::new(false);
            let payload = close_taken_over(
                &job,
                judged,
                |outcome| {
                    ends.borrow_mut().push(outcome);
                    Ok(closed)
                },
                || {
                    stopped.set(true);
                    (vec![41], vec![])
                },
                || ran,
            )
            .unwrap();
            (payload, ends.into_inner(), stopped.get())
        };
        for (judged, outcome) in [
            (Takeover::Gone, headless_job::GONE),
            (Takeover::NotTheJob, headless_job::NOT_THE_JOB),
        ] {
            let (end, ends, stopped) = close(judged, true, Some(Some(measured.clone())));
            let end = end.expect("an end with the Execution");
            assert_eq!(ends, [outcome]);
            assert!(!stopped, "{outcome}: stop_tree was called");
            assert_eq!(end["outcome"], outcome);
            assert_eq!(end["headless_job_id"], 9);
            assert_eq!(end["kind"], "recovery");
            assert_eq!(end["provider"], "claude");
            assert_eq!(end["descendants"], json!([]));
            assert_eq!(end["killed"], json!([]));
            assert_eq!(end["tokens"]["input"], 30);
            assert_eq!(end["tokens_source"], "model_usage");
            for session in [None, Some(JobSession::default())] {
                let (end, _, stopped) = close(judged, true, Some(session));
                let end = end.expect("an end, not measured");
                assert!(!stopped, "{outcome}: stop_tree was called");
                assert!(end["tokens"].is_null(), "{end}");
                assert!(end.get("tokens_source").is_some(), "{end}");
                assert_eq!(end["tokens_reason"], "tokens_not_read");
            }
            // Recorded already, or a program job: the row is only closed.
            let (end, ends, stopped) = close(judged, true, None);
            assert!(end.is_none(), "{end:?}");
            assert_eq!(ends, [outcome]);
            assert!(!stopped);
            // Closed by another supervisor first: no end, nothing read.
            let (end, _, stopped) = close(judged, false, Some(Some(measured.clone())));
            assert!(end.is_none(), "{end:?}");
            assert!(!stopped);
        }
        assert!(headless_job::NO_PROVIDER.parse::<Provider>().is_err());
        // A job its pid still runs is stopped, and its end records its
        // Execution or, recorded already, none.
        let (end, ends, stopped) = close(Takeover::Stop, true, Some(Some(measured)));
        let end = end.unwrap();
        assert_eq!(ends, [headless_job::TAKEN_OVER]);
        assert!(stopped);
        assert_eq!(end["outcome"], "taken_over");
        assert_eq!(end["descendants"], json!([41]));
        assert_eq!(end["tokens"]["input"], 30);
        let (end, _, _) = close(Takeover::Stop, true, None);
        assert!(end.unwrap().get("tokens_source").is_none());
        let (end, _, stopped) = close(Takeover::Stop, false, None);
        assert!(end.is_none() && !stopped);
    }

    /// A provider whose job's output is the number of input tokens it
    /// used, read from when the job started.
    struct Counting(Mutex<Option<i64>>);

    impl AgentProvider for Counting {
        fn preflight(&self) -> Result<()> {
            Ok(())
        }
        fn review_command(
            &self,
            _: &TaskRun,
            _: &Path,
            _: &str,
            _: crate::domain::headless_job::JobAccess,
        ) -> Result<CommandSpec> {
            unreachable!()
        }
        fn job_session(
            &self,
            stdout: &str,
            since: Option<i64>,
        ) -> Option<crate::domain::headless_job::JobSession> {
            use crate::domain::tokens::{ExecutionTokens, TokenSource, TokenUsage};
            *self.0.lock().unwrap() = since;
            Some(crate::domain::headless_job::JobSession {
                tokens: Some(ExecutionTokens {
                    tokens: Some(TokenUsage {
                        input: stdout.trim().parse().ok()?,
                        ..TokenUsage::default()
                    }),
                    source: Some(TokenSource::UsageRecord),
                    ..ExecutionTokens::default()
                }),
                ..crate::domain::headless_job::JobSession::default()
            })
        }
    }

    /// The supervisor that stops a gone one's job reads its Execution out
    /// of the stdout its row names, with the row's provider, from when the
    /// row started (milliseconds); a row that names no stdout, or one that
    /// cannot be read, gives none, which its end records as not measured.
    #[test]
    fn a_taken_over_jobs_session_is_read_from_the_stdout_its_row_names() {
        let files = MemoryFiles::default();
        let out = PathBuf::from("/runs/r1/review-2.out");
        files.put(&out, std::time::SystemTime::UNIX_EPOCH, "42\n");
        let job = HeadlessJobRecord {
            id: 9,
            kind: headless_job::REVIEW.into(),
            label: None,
            run_id: Some(RunId::new("r1").unwrap()),
            proposal_id: None,
            goal_id: None,
            attempt: 2,
            provider: "codex".into(),
            pid: 40,
            process_start: None,
            supervisor_token: LeaseToken::new("gone"),
            supervisor_pid: None,
            started_at: 1_700_000_000,
            stdout: Some(out),
        };
        let agent = Counting(Mutex::new(None));
        let session = taken_over_session(&agent, &files, &job).unwrap();
        let mut end = json!({});
        crate::domain::headless_job::JobSession::record_execution(Some(&session), &mut end);
        assert_eq!(end["tokens"]["input"], 42);
        assert_eq!(end["tokens_source"], "token_usage_record");
        assert_eq!(*agent.0.lock().unwrap(), Some(1_700_000_000_000));
        for stdout in [None, Some(PathBuf::from("/runs/r1/missing.out"))] {
            let job = HeadlessJobRecord {
                stdout,
                ..job.clone()
            };
            assert!(taken_over_session(&agent, &files, &job).is_none());
        }
    }
}
