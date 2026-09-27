//! The headless jobs of a run: the review (ADR-0027) and the recovery job
//! (ADR-0047 decisions 39 and 40), each a process waited for with a
//! timeout. Every headless job's process is recorded in `headless_jobs`
//! (task 443): a supervisor that takes over from one that died stops the
//! jobs it left before it starts its own.

use super::*;
use crate::{
    application::{HeadlessJobRecord, NewHeadlessJob},
    domain::headless_job::{Takeover, takeover},
};
use std::sync::Mutex;

/// How long the processes of a gone supervisor's job get after SIGTERM
/// before SIGKILL.
const TAKEOVER_GRACE: Duration = Duration::from_secs(5);

/// The ends of this process's headless jobs not written to the queue yet
/// (`headless_jobs` row, outcome): a job ends where no queue is at hand,
/// and the supervisor writes them at the top of its next pass.
#[derive(Clone, Default)]
pub(super) struct JobEnds(Arc<Mutex<Vec<(i64, &'static str)>>>);

impl JobEnds {
    fn push(&self, id: i64, outcome: &'static str) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((id, outcome));
    }
    fn take(&self) -> Vec<(i64, &'static str)> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// What a headless job is about, for its `headless_jobs` row.
pub(super) struct JobSubject {
    pub(super) kind: &'static str,
    pub(super) label: Option<String>,
    pub(super) run_id: Option<RunId>,
    pub(super) proposal_id: Option<crate::domain::ProposalId>,
    pub(super) goal_id: Option<crate::domain::GoalId>,
    pub(super) attempt: usize,
}

impl JobSubject {
    pub(super) fn run(kind: &'static str, run: &RunId, attempt: usize) -> Self {
        Self {
            kind,
            label: None,
            run_id: Some(run.clone()),
            proposal_id: None,
            goal_id: None,
            attempt,
        }
    }
}

/// A headless job's process (a review or a recovery job) whose stdout and stderr
/// go to files, waited for at most `timeout`.
pub(super) struct HeadlessJob {
    /// What the job is, for its failure messages: `review`, `recovery job`.
    pub(super) what: &'static str,
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
}

impl HeadlessJob {
    /// `Some` once the job ended: its stdout, or why it failed (a non-zero
    /// exit, or the timeout, after which the process is killed).
    pub(super) fn poll(
        &mut self,
        files: &dyn RunFiles,
    ) -> Result<Option<std::result::Result<String, String>>> {
        let status = match self.child.try_wait()? {
            Some(status) => status,
            None if self.started.elapsed() < self.timeout => return Ok(None),
            None => {
                self.stop();
                return Ok(Some(Err(format!(
                    "the headless {} did not finish within {} seconds",
                    self.what,
                    self.timeout.as_secs()
                ))));
            }
        };
        self.ended(headless_job::ENDED);
        if !status.success {
            let stderr = files.read_to_string(&self.stderr).unwrap_or_default();
            return Ok(Some(Err(format!(
                "the headless {} exited with {status}: {}",
                self.what,
                or_none(tail(stderr.trim(), 500))
            ))));
        }
        Ok(Some(Ok(files
            .read_to_string(&self.stdout)
            .unwrap_or_default())))
    }

    /// Kill the job's process and the processes it started (a `claude -p`'s
    /// Bash and what that runs), so none outlives the job.
    pub(super) fn stop(&mut self) {
        let descendants = self.processes.descendants(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
        for pid in descendants {
            let _ = self.processes.kill(pid);
        }
        self.ended(headless_job::STOPPED);
    }

    fn ended(&mut self, outcome: &'static str) {
        if let Some((ends, id)) = self.record.take() {
            ends.push(id, outcome);
        }
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
/// failed) is stopped, so it neither runs on unwatched nor leaves its row
/// open while this process lives.
impl Drop for HeadlessJob {
    fn drop(&mut self) {
        if self.record.is_some() {
            self.stop();
        }
    }
}

impl Supervisor<'_> {
    /// The job whose process `child` just started, recorded in
    /// `headless_jobs` with its pid and the start of its process. A record
    /// that fails is only noted: the job runs either way.
    pub(super) fn headless_job(
        &mut self,
        what: &'static str,
        child: Box<dyn Spawned>,
        stdout: PathBuf,
        stderr: PathBuf,
        subject: JobSubject,
    ) -> HeadlessJob {
        let pid = child.id();
        let new = NewHeadlessJob {
            kind: subject.kind,
            label: subject.label,
            run_id: subject.run_id,
            proposal_id: subject.proposal_id,
            goal_id: subject.goal_id,
            attempt: subject.attempt,
            pid,
            process_start: self.processes.start_identity(pid),
            supervisor_token: self.token.clone(),
        };
        let record = match self.queue.record_headless_job(&new) {
            Ok(id) => Some((self.job_ends.clone(), id)),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the start of the headless {what} (pid {pid}) could not be recorded: {error:#}");
                None
            }
        };
        HeadlessJob {
            what,
            child,
            started: Instant::now(),
            timeout: self.reviewer.review_timeout(),
            stdout,
            stderr,
            processes: self.processes.clone(),
            record,
        }
    }

    /// The wall only a person moves (a login that ran out, the usage
    /// limit) that the output of a headless job that failed shows it
    /// stopped at (ADR-0047 decision 42, task 438).
    pub(super) fn job_wall(&self, job: &HeadlessJob) -> Option<Wall> {
        self.signals.job_wall(&job.output(&*self.files))
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
    pub(super) fn raise_job_wall(&mut self, wall: Wall, job: &HoldJob, error: &str) -> bool {
        match self.hold_job(wall, job, error) {
            Ok(()) => true,
            Err(hold_error) => {
                warn!(error = %format_args!("{hold_error:#}"), "the headless {} stopped at the {} wall, and its hold ask could not be written: {hold_error:#}", job.entry(), wall.as_str());
                false
            }
        }
    }

    fn hold_job(&mut self, wall: Wall, job: &HoldJob, error: &str) -> Result<()> {
        let run = job.run_id().cloned();
        let (outcome, value) = ask::hold(
            &mut *self.queue,
            &self.layout.main_checkout,
            NewHold::wall(wall, run.clone(), Some(job.clone())),
            self.cmux,
        )?;
        if outcome.joined {
            let payload = json!({
                "job": job.kind(),
                "entry": job.entry(),
                "error": tail(error, 500),
                "ask_id": outcome.ask.id,
            });
            match &run {
                Some(run) => self
                    .queue
                    .record_runtime_event(run, wall.event_kind(), payload)?,
                None => {
                    self.queue.record_queue_event(wall.event_kind(), payload)?;
                }
            }
        }
        if self.queue_hold.is_none() {
            self.queue_hold = crate::domain::queue_hold::hold_of(&outcome.ask);
        }
        warn!(ask_id = %outcome.ask.id, "the headless {} stopped at the {} wall: ask {} holds {} run(s) and job(s) (notified: {})", job.entry(), wall.as_str(), outcome.ask.id, outcome.ask.affected.len(), value["notified"]);
        Ok(())
    }

    /// Write the ends of this process's jobs, then stop the jobs gone
    /// supervisors left running (task 443): on every pass before any job
    /// starts, and the first time also those of this process's token,
    /// which an exec'd process knows nothing of.
    pub(super) fn tend_headless_jobs(&mut self) {
        self.write_job_ends();
        let own = !self.jobs_swept;
        let orphans = match self.queue.orphaned_headless_jobs(&self.token, own) {
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
        for (id, outcome) in self.job_ends.take() {
            if let Err(error) = self.queue.end_headless_job(id, outcome) {
                warn!(error = %format_args!("{error:#}"), "the end of headless job {id} could not be recorded: {error:#}");
            }
        }
    }

    /// Stop the job of a gone supervisor when its pid still runs the job's
    /// process (the same start), with its descendants: SIGTERM, then
    /// SIGKILL after [`TAKEOVER_GRACE`]; record `headless_job_stopped`. A
    /// pid that runs another process now, or whose start cannot be told, is
    /// never signalled.
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
        let outcome = match takeover(alive, job.process_start.as_deref(), start.as_deref()) {
            Takeover::Gone => headless_job::GONE,
            Takeover::NotTheJob => {
                info!(
                    "headless {} job {} of supervisor {}: pid {} runs another process now (started {}, the job's {}); it is left alone",
                    job.kind,
                    job.id,
                    job.supervisor_token,
                    job.pid,
                    start.as_deref().unwrap_or("unknown"),
                    job.process_start.as_deref().unwrap_or("unknown")
                );
                headless_job::NOT_THE_JOB
            }
            Takeover::Stop => {
                // Closed first, so of two supervisors taking over at once
                // only one signals the job.
                if !self
                    .queue
                    .end_headless_job(job.id, headless_job::TAKEN_OVER)?
                {
                    return Ok(());
                }
                let (descendants, killed) = self.stop_tree(job.pid, job.process_start.as_deref());
                let payload = json!({
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
                info!(
                    "stopped the headless {} job {} (pid {}, attempt {}) that supervisor {} left running",
                    job.kind, job.id, job.pid, job.attempt, job.supervisor_token
                );
                let recorded = match &job.run_id {
                    Some(run) => self.queue.record_runtime_event(
                        run,
                        event_kind::HEADLESS_JOB_STOPPED,
                        payload.clone(),
                    ),
                    None => Err(anyhow!("no run")),
                };
                if recorded.is_err()
                    && let Err(error) = self
                        .queue
                        .record_queue_event(event_kind::HEADLESS_JOB_STOPPED, payload)
                {
                    warn!(error = %format_args!("{error:#}"), "the stop of headless job {} could not be recorded: {error:#}", job.id);
                }
                return Ok(());
            }
        };
        self.queue.end_headless_job(job.id, outcome).map(|_| ())
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
    /// readable verdict: another unreadable one is not retried again.
    pub(super) retried: bool,
    pub(super) job: HeadlessJob,
}

/// How a headless review ended.
pub(super) enum ReviewEnd {
    Verdict(ReviewVerdict),
    /// The job ended well but its stdout held no readable verdict JSON
    /// (task 328): worth one more review with the same input.
    Unreadable(String),
    /// The job itself failed: it exited non-zero or timed out.
    Failed(String),
}

impl ReviewWatch {
    /// `Some` once the review ended: its verdict, or why there is none.
    pub(super) fn poll(&mut self, files: &dyn RunFiles) -> Result<Option<ReviewEnd>> {
        Ok(self.job.poll(files)?.map(|output| match output {
            Ok(stdout) => match ReviewVerdict::parse(&stdout) {
                Ok(verdict) => ReviewEnd::Verdict(verdict),
                Err(error) => ReviewEnd::Unreadable(error),
            },
            Err(error) => ReviewEnd::Failed(error),
        }))
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
    pub(super) job: HeadlessJob,
}

impl EndedRecovery {
    pub(super) fn poll(
        &mut self,
        files: &dyn RunFiles,
    ) -> Result<Option<std::result::Result<RecoveryVerdict, String>>> {
        Ok(self
            .job
            .poll(files)?
            .map(|output| output.and_then(|stdout| RecoveryVerdict::parse(&stdout))))
    }
}
