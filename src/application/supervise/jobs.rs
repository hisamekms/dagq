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
use crate::domain::EventKind;
use crate::domain::headless_job::{JobFailure, JobKind, JobStop};
use crate::domain::provider_switch::SwitchReason;
use crate::{
    application::{Clock, CommandSpec, Exit, HeadlessJobRecord, HeadlessJobStore, NewHeadlessJob},
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
    pub(super) kind: &'static str,
    /// What runs it: an agent, unless it is a program job of a run's
    /// review.
    pub(super) job: JobKind,
    /// Whether it is a job of a run's review stage, whose timeout
    /// `[review.jobs]` sets by its kind.
    pub(super) review_stage: bool,
    pub(super) label: Option<String>,
    pub(super) run_id: Option<RunId>,
    pub(super) proposal_id: Option<crate::domain::ProposalId>,
    pub(super) goal_id: Option<crate::domain::GoalId>,
    pub(super) attempt: usize,
    /// The provider the job was started on (ADR-t1063-1).
    pub(super) provider: Provider,
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
}

/// What every headless job starts through, whatever its kind: where its
/// row is written, how its process is found and stopped, whose job it is
/// and where its end waits to be written.
pub struct JobPorts<'a> {
    pub store: &'a dyn HeadlessJobStore,
    pub processes: Arc<dyn ProcessControl + Send + Sync>,
    pub supervisor_token: &'a LeaseToken,
    pub clock: &'a dyn Clock,
    pub ends: &'a JobEnds,
}

/// The job whose process `child` just started, recorded in
/// `headless_jobs` with its kind, its pid and the start of its process,
/// waited for at most `timeout`. A record that fails is only noted: the
/// job runs either way.
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
        started: Instant::now(),
        timeout,
        stdout,
        stderr,
        processes: ports.processes.clone(),
        record,
        provider: subject.provider,
        started_at: started_at_ms(ports.clock),
        // Only an agent's job is an Execution whose end is recorded when it
        // is abandoned (ADR-t1486-1); a program job's ends with its row.
        unended: (subject.job == JobKind::Agent).then(|| (ports.ends.clone(), subject)),
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
    let mut program = program.clone();
    program.new_session();
    let child = spawner.spawn(
        &program,
        Streams::Files {
            stdout: &output.0,
            stderr: &output.1,
        },
    )?;
    Ok(record_job(
        ports,
        "review program",
        child,
        output,
        subject,
        timeout,
    ))
}

/// The most of each of a program job's stdout and stderr its end keeps
/// ([`ProgramEnd`]), from their ends.
pub const PROGRAM_OUTPUT_TAIL: usize = 4000;

/// Start the program review `program` of `run`'s review `attempt` as a
/// program job against `worktree` (ADR-t1895-2): `backend` makes its
/// command (the host's, or another backend's of the review's actor), its
/// output goes under `output` (the run's directory) and what it runs is
/// written under `scratch`, each by attempt and name, and it is stopped
/// with its group past the program's own `timeout_secs`, or `timeout`
/// without one. `scratch` is a directory the runtime owns, outside what
/// the worker can write (not the run's directory): a script written where
/// the worker can replace it would run the worker's text.
#[allow(clippy::too_many_arguments)]
pub fn start_review_program(
    ports: &JobPorts<'_>,
    spawner: &dyn Spawner,
    backend: &dyn crate::application::ReviewProgramBackend,
    program: &crate::application::review_programs::SnapshotProgram,
    worktree: &Path,
    (output, scratch): (&Path, &Path),
    (run, attempt): (&RunId, usize),
    timeout: Duration,
) -> Result<HeadlessJob> {
    let name = &program.program.name;
    let stem = format!("review-program-{attempt}-{name}");
    let command = backend
        .command(program, worktree, output, &scratch.join(&stem))
        .with_context(|| format!("prepare the review program {name}"))?;
    let timeout = program
        .program
        .timeout_secs
        .map_or(timeout, Duration::from_secs);
    start_program_job(
        ports,
        spawner,
        &command,
        (
            output.join(format!("{stem}.out")),
            output.join(format!("{stem}.err")),
        ),
        JobSubject::review_program(run, attempt, name),
        timeout,
    )
}

/// How a program job ended: its exit (`None` when it ran past its timeout
/// and was stopped with its group) and the ends of its stdout and stderr,
/// [`PROGRAM_OUTPUT_TAIL`] bytes of each at most.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramEnd {
    pub exit: Option<Exit>,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

/// A headless job's process (a review or a recovery job) whose stdout and stderr
/// go to files, waited for at most `timeout`.
pub struct HeadlessJob {
    /// What the job is, for its failure messages: `review`, `recovery job`.
    pub(super) what: &'static str,
    /// What runs it.
    pub(super) kind: JobKind,
    pub(super) child: Box<dyn Spawned>,
    pub(super) started: Instant,
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
}

impl HeadlessJob {
    /// `Some` once the job ended: its reply, as `provider` (the one that
    /// started it) reads it out of its stdout (ADR-t1063-1 decision 2), or
    /// why it failed (a non-zero exit, or the timeout, after which the
    /// process is killed).
    pub(super) fn poll(
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
        let status = match self.child.try_wait()? {
            Some(status) => status,
            None if self.started.elapsed() < self.timeout => return Ok(None),
            None => {
                self.stop();
                return Ok(Some(Err(JobFailed::TimedOut(format!(
                    "the headless {} did not finish within {} seconds",
                    self.what,
                    self.timeout.as_secs()
                )))));
            }
        };
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
    /// ends of its output either way.
    pub fn poll_program(&mut self, files: &dyn RunFiles) -> Result<Option<ProgramEnd>> {
        let exit = match self.child.try_wait()? {
            Some(status) => {
                self.ended(headless_job::ENDED);
                Some(status)
            }
            None if self.started.elapsed() < self.timeout => return Ok(None),
            None => {
                self.stop();
                None
            }
        };
        // Only the end is read, and bytes that are not UTF-8 are replaced,
        // so a long or binary output still shows how it ended.
        let read = |path: &Path| {
            let bytes = files
                .read_tail(path, PROGRAM_OUTPUT_TAIL as u64)
                .unwrap_or_default();
            tail(&String::from_utf8_lossy(&bytes), PROGRAM_OUTPUT_TAIL).to_owned()
        };
        Ok(Some(ProgramEnd {
            exit,
            stdout_tail: read(&self.stdout),
            stderr_tail: read(&self.stderr),
        }))
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
        self.ended(headless_job::STOPPED);
    }

    /// Stop a job whose end no event records (a handoff, a slot the
    /// supervisor stops watching, a drop): as [`Self::stop`], and its end
    /// is written with the next ends as `headless_job_stopped` with the
    /// Execution its agent was so far (ADR-t1486-1). A job that ended
    /// already is left as it is.
    pub(super) fn abandon(&mut self) {
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
    review_jobs: &crate::domain::headless_job::JobTimeouts,
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
pub(super) struct JobDesk<'s> {
    pub(super) reviewer: &'s dyn AgentProvider,
    pub(super) codex_jobs: Option<&'s dyn AgentProvider>,
    pub(super) signals: &'s dyn AgentSignals,
    pub(super) service_access: &'s dyn crate::application::queue_service::ServiceAccess,
    pub(super) no_claude: bool,
    pub(super) job_ends: &'s JobEnds,
    /// `[review.jobs]` as read at the start.
    pub(super) review_jobs: crate::domain::headless_job::JobTimeouts,
    pub(super) provider: &'s mut stages::ProviderState,
    pub(super) queue_hold: &'s mut Option<claim_hold::QueueHold>,
}

impl<'s> JobDesk<'s> {
    /// [`job_agent_of`] on the supervisor's agents.
    pub(super) fn job_agent(&self, provider: Provider) -> Option<&'s dyn AgentProvider> {
        job_agent_of(provider, self.no_claude, self.reviewer, self.codex_jobs)
    }

    /// Whether the queue's open authentication or usage-limit ask holds
    /// the jobs (task 437).
    pub(super) const fn held(&self) -> bool {
        self.queue_hold.is_some()
    }

    /// `[provider_fallback] jobs` as last read (ADR-t1857-1).
    pub(super) const fn fallback_jobs(&self) -> bool {
        self.provider.fallback.jobs
    }

    /// Why `provider` is held now ([`provider_held_of`]).
    pub(super) fn provider_held(&self, provider: Provider) -> Option<SwitchReason> {
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
    pub(super) fn job_unusable(&self, provider: Provider) -> Option<SwitchReason> {
        job_unusable_of(
            self.provider_held(provider),
            self.job_agent(provider).is_some(),
        )
    }

    /// The actor executor that starts a job on `agent`.
    pub(super) fn actors_on<'e>(
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
    pub(super) fn headless_job(
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
            clock: &*env.generators.clock,
            ends: self.job_ends,
        };
        record_job(&ports, what, child, (stdout, stderr), subject, timeout)
    }

    /// Why `job` failed ([`job_failure_of`]).
    pub(super) fn job_failure(&self, files: &dyn RunFiles, job: &HeadlessJob) -> JobFailure {
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
    /// which may say when a usage limit resets.
    pub(super) fn job_provider_failed(
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
pub(super) fn job_unusable_of(held: Option<SwitchReason>, has_agent: bool) -> Option<SwitchReason> {
    held.or((!has_agent).then_some(SwitchReason::ExecutableMissing))
}

/// Where a job whose `provider` could not be used is started again, as a
/// log line says it: on the other provider, or, with `[provider_fallback]
/// jobs` off (`fallback` false), on `provider` once its hold ends
/// (ADR-t1857-1).
pub(super) fn again_on(fallback: bool, provider: Provider) -> String {
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
pub(super) fn job_failure_route(
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
                queues: &self.queues,
                generators: &self.generators,
                layout: self.layout,
                processes: &self.processes,
                files: &self.files,
                repository: &self.repository,
                verifier: &self.verifier,
                spawner: self.spawner,
                sessions: self.sessions,
                token: &self.registration.token,
            },
            JobDesk {
                reviewer: self.reviewer,
                codex_jobs: self.codex_jobs,
                signals: self.signals,
                service_access: self.service_access,
                no_claude: self.no_claude,
                job_ends: &self.job_ends,
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
    fn job_ports(&self) -> JobPorts<'_> {
        JobPorts {
            store: &*self.queue,
            processes: self.processes.clone(),
            supervisor_token: &self.registration.token,
            clock: &*self.generators.clock,
            ends: &self.job_ends,
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
        record_job(
            &self.job_ports(),
            what,
            child,
            (stdout, stderr),
            subject,
            timeout,
        )
    }

    /// How long a job about `subject` may run
    /// ([`crate::domain::headless_job::JobTimeouts::job_timeout`]).
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
        let own = !self.jobs_swept;
        let orphans = match self
            .queue
            .orphaned_headless_jobs(&self.registration.token, own)
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

    pub(super) fn write_job_ends(&mut self) {
        let (rows, abandoned) = self.job_ends.take();
        for (id, outcome) in rows {
            if let Err(error) = self.queue.end_headless_job(id, outcome) {
                warn!(error = %format_args!("{error:#}"), "the end of headless job {id} could not be recorded: {error:#}");
            }
        }
        for job in abandoned {
            self.write_abandoned(&job);
        }
    }

    /// Record `headless_job_stopped` for a job this process stopped with
    /// no event to end it, on its run, or on the queue for a job with
    /// none, with the Execution its output names ([`abandoned_end`]).
    fn write_abandoned(&mut self, job: &Abandoned) {
        let agent = self
            .job_agent(job.subject.provider)
            .unwrap_or(self.reviewer);
        let stdout = self.files.read_to_string(&job.stdout).unwrap_or_default();
        let session = agent.job_session(&stdout, job.started_at);
        let payload = abandoned_end(job, &self.registration.token, session.as_ref());
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
        let deadline = Instant::now() + TAKEOVER_GRACE;
        let running = |pid: u32| {
            // A job this process started before its exec is its child.
            self.processes.reap(pid);
            self.processes.alive(pid)
        };
        while all.iter().any(|(pid, _)| running(*pid)) && Instant::now() < deadline {
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

/// The headless review in progress, with its output in `review-N.out` /
/// `review-N.err` in the run directory.
pub(super) struct ReviewWatch {
    pub(super) session: Option<SessionRef>,
    pub(super) attempt: usize,
    /// Whether this review is the retry of one whose stdout held no
    /// readable verdict or whose job exited non-zero: it is not retried
    /// again for either cause ([`super::landing::retries_review`]). The
    /// supervisor alone holds this, so a review it adopts counts afresh. A
    /// wait (`Phase::ReviewHeld`) keeps it: a retry that waits records
    /// `review_retried` before the wait, and the review after the wait
    /// starts as the retry, so it is not retried again either.
    pub(super) retried: bool,
    /// Whether `[roles.review]` names its provider: a provider that cannot
    /// be used then moves the review to the other (ADR-t1207-1).
    pub(super) switchable: bool,
    /// The review's required subagents (ADR-t1453-1): the verdict must
    /// carry the completed result of each and of no other agent; none for
    /// a review that requires none.
    pub(super) required: Vec<String>,
    pub(super) job: HeadlessJob,
}

/// How a headless review ended.
pub(super) enum ReviewEnd {
    Verdict(ReviewVerdict),
    /// The job ended well but its stdout held no readable verdict JSON
    /// (task 328), or a verdict without the completed results of its
    /// required subagents (ADR-t1453-1 decision 6): worth one more review
    /// with the same input.
    Unreadable(String),
    /// The job itself failed: it exited non-zero or timed out.
    Failed(JobFailed),
}

impl ReviewEnd {
    /// Why a review that gave no verdict failed; `None` for a verdict.
    pub(super) fn error(&self) -> Option<&str> {
        match self {
            Self::Verdict(_) => None,
            Self::Unreadable(error) => Some(error),
            Self::Failed(failed) => Some(failed.error()),
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

impl ReviewWatch {
    /// `Some` once the review ended: its verdict, or why there is none.
    pub(super) fn poll(
        &mut self,
        files: &dyn RunFiles,
        provider: &dyn AgentProvider,
    ) -> Result<Option<ReviewEnd>> {
        Ok(self
            .job
            .poll_end(files, provider)?
            .map(|output| match output {
                Ok(stdout) => match ReviewVerdict::parse(&stdout) {
                    Ok(verdict) => review_end(verdict, &self.required),
                    Err(error) => ReviewEnd::Unreadable(error),
                },
                Err(error) => ReviewEnd::Failed(error),
            }))
    }
}

/// How a review whose stdout held `verdict` ended: as its verdict when it
/// carries the completed results of exactly the `required` agents, else
/// as an unreadable one, which is never a pass (ADR-t1453-1 decision 6).
pub(super) fn review_end(verdict: ReviewVerdict, required: &[String]) -> ReviewEnd {
    match crate::domain::review_subagents::incomplete(required, &verdict.agents) {
        None => ReviewEnd::Verdict(verdict),
        Some(why) => ReviewEnd::Unreadable(why),
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
            _: &str,
            _: crate::domain::headless_job::JobAccess,
        ) -> Result<CommandSpec> {
            unreachable!()
        }
    }

    fn job(files: &MemoryFiles, success: bool, stdout: &str) -> HeadlessJob {
        let (out, err) = (PathBuf::from("/job/out"), PathBuf::from("/job/err"));
        files.put(&out, std::time::SystemTime::UNIX_EPOCH, stdout);
        files.put(&err, std::time::SystemTime::UNIX_EPOCH, "it broke\n");
        HeadlessJob {
            what: "review",
            kind: JobKind::Agent,
            child: Box::new(Ended { success }),
            started: Instant::now(),
            timeout: Duration::from_secs(60),
            stdout: out,
            stderr: err,
            processes: Arc::new(NoProcesses),
            record: None,
            provider: Provider::Claude,
            started_at: None,
            unended: None,
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
        let clock = At(std::time::UNIX_EPOCH);
        let ports = JobPorts {
            store: &rows,
            processes: Arc::new(NoProcesses),
            supervisor_token: &token,
            clock: &clock,
            ends: &ends,
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
        let mut review = job(&files, true, "");
        review.unended = Some((
            ends.clone(),
            JobSubject::run(headless_job::REVIEW, run.id(), 1),
        ));
        let mut slots = super::super::stages::SlotTable::default();
        slots.admit(Slot::new(
            run.clone(),
            Phase::Review(ReviewWatch {
                session: None,
                attempt: 1,
                retried: false,
                switchable: false,
                required: Vec::new(),
                job: review,
            }),
        ));
        slots.admit(Slot::new(run, Phase::AwaitingSlot));
        slots.abandon_jobs();
        let (_, handed) = ends.take();
        assert_eq!(handed.len(), 1);
        assert_eq!(handed[0].subject.kind, headless_job::REVIEW);
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

    /// A clock stopped at a fixed time.
    struct At(std::time::SystemTime);

    impl crate::application::Clock for At {
        fn system_time(&self) -> std::time::SystemTime {
            self.0
        }

        fn monotonic(&self) -> std::time::Instant {
            std::time::Instant::now()
        }
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
