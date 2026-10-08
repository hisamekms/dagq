//! Non-interactive worker recovery: turn stalls and idle processes.
//! Verdicts are checked again before application; failures and low confidence
//! escalate to an ask. Historical alert kinds remain readable in the domain.

use super::*;
use crate::application::prompt::{BinaryFacts, FittedPrompt, binary_facts, recovery_binary_file};
use crate::domain::ActorContext;
use crate::domain::EventKind;
use crate::domain::actor_model::{ActorLaunch, JobStartRoute, ModelRole, UnnamedWithoutClaude};
use crate::domain::headless_job::JobSession;
use crate::domain::idle_process::{
    CpuWatch, IdleProcess, PROGRESS_CPU_PER_MILLE, without_session_helpers,
};
use crate::domain::provider_switch::SwitchReason;
use crate::domain::recovery::{
    MAX_RECHECK_SECS, MAX_RECOVERY_ATTEMPTS, ProcessInfo, RecoveryAction, attempts, failed_live,
    run_processes,
};

/// The actions a recovery job may choose for an `idle_process` alert;
/// `send_instruction` holds only while the session is idle at its prompt.
pub(super) const IDLE_PROCESS_ACTIONS: [&str; 3] = ["stop_processes", "send_instruction", "wait"];

/// The setting the `idle_process` alert is judged by.
pub(super) const IDLE_PROCESS_THRESHOLD: &str = "idle_process_secs";

/// How long a stopped process gets between SIGTERM and SIGKILL.
pub(super) const STOP_GRACE: Duration = Duration::from_secs(3);

/// What a recovery job printed: its verdict, or why there is none.
type JobOutcome = std::result::Result<RecoveryVerdict, String>;

/// The recovery job in progress for one alert of a live session.
pub(super) struct RecoveryJob {
    pub(super) alert: RecoveryAlert,
    /// Why the alert was raised, for an alert raised for more than one
    /// reason (`stalled`: `turn_without_receipt` or `permission_denied`).
    pub(super) reason: Option<&'static str>,
    pub(super) attempt: usize,
    marker: Option<SystemTime>,
    /// Whether `[roles.recovery]` names its provider, so that a provider
    /// that cannot be used is held for the next jobs (ADR-t1063-1
    /// decision 4).
    switchable: bool,

    job: HeadlessJob,
}

#[derive(Default)]
pub(super) struct RecoveryWatch {
    job: Option<Box<RecoveryJob>>,
    /// The `wait` of each alert (and reason) whose job answered it, until
    /// when.
    held: Vec<(RecoveryAlert, Option<&'static str>, SystemTime)>,
    /// The CPU time of the run's processes (`idle_process`), the time of the
    /// process sample it last took and the idle processes it last handed
    /// to a job.
    cpu: CpuWatch,
    cpu_sampled: Option<SystemTime>,
    idle: Vec<IdleProcess>,
    /// When the watch first looked for idle processes: the first sample
    /// waits one sample interval, so a short session is never sampled.
    idle_watched: Option<Instant>,
}

/// What the watch of a live session offers the recovery job of an alert.
pub(super) struct Live<'a> {
    pub(super) workspace: &'a str,
    pub(super) run_dir: &'a Path,
    /// The actions that apply to the alert here.
    pub(super) allowed: &'static [&'static str],
    /// The session is idle at its prompt, for `send_instruction`.
    pub(super) at_prompt: bool,
    /// The run is running in its first session, which `resume` parks as
    /// `needs_session` for a session of its own (task 442).
    pub(super) park: bool,
}

/// What a `repair` applied to a live session changed.
#[derive(Default)]
pub(super) struct Applied {
    pub(super) names: Vec<&'static str>,
    /// The instruction typed, when and how it went.
    pub(super) sent: Option<(String, SystemTime, Submission)>,
    /// `resume` was chosen, with its instruction: the watch parks the run
    /// as `needs_session` and asks its session to exit.
    pub(super) resume: Option<String>,
}

pub(super) enum LiveStep {
    /// Nothing to do now: its job runs, another alert's does, or its
    /// `wait` holds.
    Pending,
    /// The job's repair was applied.
    Repaired(Applied),
    /// A person is asked: the job's number and why. A failed job is one
    /// too: the alert's own ask opens (ADR-t609-1).
    Escalate(usize, Escalation),
}

/// A person's note for the ask an escalation opens.
pub(super) struct Note {
    /// Why the recovery job did not repair it, as a sentence.
    pub(super) why: String,
    /// The lines for the ask's question: why a person, the diagnosis, the
    /// recommended actions, the job's question and its material.
    pub(super) text: String,
    /// The job's options, to add to the kind's own.
    pub(super) options: Vec<String>,
    pub(super) category: AskReason,
}

fn millis(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn at_millis(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap_or(0))
}

/// Why a person is asked instead of the verdict being applied.
pub(super) enum Escalation {
    /// The job failed or printed no verdict.
    JobFailed(String),
    /// The job answered `escalate`, or `repair` with low confidence.
    Verdict(RecoveryVerdict),
    /// A `repair` whose action does not apply now.
    Refused(RecoveryVerdict, String),
    /// The alert got its [`MAX_RECOVERY_ATTEMPTS`] jobs already.
    UsedUp(usize),
}

impl Escalation {
    pub(super) fn verdict(&self) -> Option<&RecoveryVerdict> {
        match self {
            Self::Verdict(verdict) | Self::Refused(verdict, _) => Some(verdict),
            Self::JobFailed(_) | Self::UsedUp(_) => None,
        }
    }

    /// The recovery job whose verdict this escalation carries out (its
    /// `escalate`, a `repair` of low confidence, or one refused), to record
    /// as `requested_by`; `None` when no job asked for it (it failed, or
    /// the alert used its jobs up).
    pub(super) fn requester(
        &self,
        run: &TaskRun,
        alert: RecoveryAlert,
        attempt: usize,
    ) -> Option<ActorContext> {
        self.verdict()
            .map(|_| ActorContext::recovery_job(run.id(), alert.as_str(), attempt))
    }

    /// What the inbox is told (ADR-0047 decision 40): the reason category
    /// is the job's `discard` or `scope`, else `recovery_failed`.
    pub(super) fn note(&self, run: &TaskRun, alert: RecoveryAlert, attempt: usize) -> Note {
        let why = match self {
            Self::JobFailed(error) => {
                format!("the recovery job failed ({error})")
            }
            Self::Verdict(verdict) if verdict.verdict == RecoveryDecision::Escalate => {
                "the recovery job could not repair it".to_owned()
            }
            Self::Verdict(_) => {
                "the recovery job was not sure of its repair (confidence low), so it was not applied"
                    .to_owned()
            }
            Self::Refused(_, why) => {
                format!("the runtime did not apply the recovery job's repair: {why}")
            }
            Self::UsedUp(attempts) => format!(
                "the recovery job ran {attempts} times for this alert already (at most {MAX_RECOVERY_ATTEMPTS})"
            ),
        };
        let verdict = self.verdict();
        let category = match verdict.and_then(|v| v.reason_category) {
            Some(category @ (AskReason::Discard | AskReason::Scope)) => category,
            _ => AskReason::RecoveryFailed,
        };
        let mut text = format!("Why a person: {}", category.as_str());
        if let Some(verdict) = verdict {
            text.push_str(&format!("\nDiagnosis: {}", verdict.diagnosis));
            if !verdict.actions.is_empty() {
                text.push_str(&format!(
                    "\nRecommended: {}",
                    serde_json::to_string(&verdict.actions).unwrap_or_default()
                ));
            }
            if !verdict.question.trim().is_empty() {
                text.push_str(&format!("\nQuestion: {}", verdict.question.trim()));
            }
        }
        if !matches!(self, Self::UsedUp(_))
            && let Some(run_dir) = run.run_dir()
        {
            text.push_str(&format!(
                "\nRecovery material: {run_dir}/{}",
                job_file(alert, attempt, "prompt.txt")
            ));
        }
        let mut options = Vec::new();
        for option in verdict.map(|v| v.options.as_slice()).unwrap_or_default() {
            let option = option.trim();
            if !option.is_empty() && !options.iter().any(|o| o == option) {
                options.push(option.to_owned());
            }
        }
        Note {
            why,
            text,
            options,
            category,
        }
    }

    /// `recovery_finished` of an escalation: `ask_id` names the ask it
    /// opened, `None` when an open ask of the run already has a person
    /// looking (`outcome: already_asked`); `extra` adds to it. A failed job
    /// escalated to its ask adds `outcome: job_failed` and the `error`.
    pub(super) fn finished(
        &self,
        alert: RecoveryAlert,
        attempt: usize,
        note: &Note,
        ask_id: Option<AskId>,
        extra: Value,
    ) -> Value {
        let verdict = self.verdict();
        let mut payload = json!({
            "alert": alert,
            "attempt": attempt,
            "verdict": verdict.map(|v| v.verdict),
            "confidence": verdict.map(|v| v.confidence),
            "diagnosis": verdict.map(|v| v.diagnosis.clone()),
            "applied": [],
            "escalated": ask_id.is_some(),
            "why": note.why,
            "reason_category": note.category,
            "ask_id": ask_id,
        });
        if ask_id.is_none() {
            payload["outcome"] = json!("already_asked");
        }
        if let Self::JobFailed(error) = self {
            payload["outcome"] = json!("job_failed");
            payload["error"] = json!(error);
        }
        if let (Value::Object(payload), Value::Object(extra)) = (&mut payload, extra) {
            payload.extend(extra);
        }
        payload
    }

    /// Record [`Self::finished`] for a live session's alert, with the end
    /// of the job that led to it ([`JobEnd`]) when one ran.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record(
        &self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        alert: RecoveryAlert,
        attempt: usize,
        note: &Note,
        ask_id: Option<AskId>,
        extra: Value,
    ) -> Result<()> {
        let mut payload = self.finished(alert, attempt, note, ask_id, extra);
        if let Some(at) = sv
            .triage
            .live_job_ends
            .iter()
            .position(|(id, ended, number, _)| {
                id == run.id() && *ended == alert && *number == attempt
            })
        {
            let (_, _, _, end) = sv.triage.live_job_ends.remove(at);
            // An alert past its jobs ran none: an end its last job left
            // (its escalation failed before this record) is dropped.
            if !matches!(self, Self::UsedUp(_)) {
                end.record(&mut payload);
            }
        }
        sv.queue
            .record_runtime_event(run.id(), EventKind::RecoveryFinished, payload)?;
        Ok(())
    }
}

impl Supervisor<'_> {
    /// Carry out `escalation` of the live `alert`'s job `attempt`: the
    /// `recovery_finished` and the ask written meanwhile record the job as
    /// `requested_by` when its verdict asked for the escalation
    /// ([`Escalation::requester`], task 782); a failed job's are the
    /// supervisor's own.
    pub(super) fn for_escalation<T>(
        &mut self,
        run: &TaskRun,
        alert: RecoveryAlert,
        attempt: usize,
        escalation: &Escalation,
        apply: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        match escalation.requester(run, alert, attempt) {
            Some(job) => self.for_job(&job, apply),
            None => apply(self),
        }
    }
}

/// The options of the ask an escalation opens: the ask kind's own
/// `base`, then the job's options (`note`, none for an ask without a job)
/// that are not among them, in order.
pub(super) fn ask_options(base: &[&str], note: Option<&Note>) -> Vec<String> {
    let mut options: Vec<String> = base.iter().map(|o| (*o).to_owned()).collect();
    for option in note.map(|note| note.options.as_slice()).unwrap_or_default() {
        if !options.contains(option) {
            options.push(option.clone());
        }
    }
    options
}

/// The `recovery_finished` of the job `attempt` of `alert` (raised for
/// `reason` at `marker`) stopped as `outcome`, with the Execution its agent
/// was so far: `session`, what its output named, or not measured.
fn stopped_end(
    (alert, reason): (RecoveryAlert, Option<&str>),
    attempt: usize,
    marker: Option<SystemTime>,
    outcome: &str,
    session: Option<&JobSession>,
) -> Value {
    let mut finished = json!({
        "alert": alert,
        "reason": reason,
        "attempt": attempt,
        "outcome": outcome,
        "marker_at_ms": marker.map(millis),
    });
    JobSession::record_execution(session, &mut finished);
    finished
}

/// The name of a recovery job's file in the run directory: its prompt
/// (`prompt.txt`), stdout (`out`) and stderr (`err`).
pub(super) fn job_file(alert: RecoveryAlert, attempt: usize, what: &str) -> String {
    format!("recovery-{}-{attempt}.{what}", alert.as_str())
}

/// What the end of a recovery job records beyond its verdict
/// (ADR-t1063-1 decisions 4 and 6): the session its provider names itself
/// (Codex's thread, and its model or why none was read; none on Claude,
/// whose session id its start records), the job's tokens (ADR-t1486-1),
/// and the provider found unusable when the job failed for that.
#[derive(Debug, Clone, Default)]
pub(super) struct JobEnd {
    pub(super) session: Option<JobSession>,
    pub(super) unusable: Option<(Provider, SwitchReason)>,
}

impl JobEnd {
    /// Put the end into the payload of an event that ends the job.
    pub(super) fn record(&self, payload: &mut Value) {
        if let Some(session) = &self.session {
            session.record(payload);
        }
        if let Some((provider, reason)) = self.unusable {
            payload["provider_unusable"] = json!({"provider": provider, "reason": reason});
        }
    }
}

impl Supervisor<'_> {
    /// Where the next recovery job (of a run that ended or of a live one)
    /// starts (ADR-t1063-1 decisions 1, 4 and 5, ADR-t1204-1), or `None`
    /// while it waits, from `[roles.recovery]` as it reads now
    /// ([`Self::start_route`]). Under `--no-claude` a role that names no
    /// provider goes to a person told why. A role that names its provider,
    /// Claude or Codex, is among the jobs `[provider_fallback] jobs` turns
    /// off (ADR-t1857-1): off, it waits for its own provider instead of
    /// moving to the other one, before a start and after a job found its
    /// provider unusable (and held it) alike.
    pub(super) fn recovery_route(&self) -> Option<JobStartRoute> {
        self.start_route(ModelRole::Recovery, UnnamedWithoutClaude::Unavailable)
    }

    /// The end of the recovery `job` once it ended: the session its
    /// provider's output names (read by `agent`, the provider that ran
    /// it), and, for a job that failed (`error`), the provider held when
    /// it could not be used and the role names its provider
    /// (`switchable`), as a goal review's (ADR-t1063-1 decisions 4 and 5).
    /// A Claude job stopped at a login or the usage limit joins the
    /// queue's hold ask (task 438).
    pub(super) fn recovery_job_end(
        &mut self,
        run: &RunId,
        job: &HeadlessJob,
        agent: &dyn AgentProvider,
        switchable: bool,
        error: Option<&str>,
    ) -> JobEnd {
        let stdout = self.files.read_to_string(&job.stdout).unwrap_or_default();
        let session = agent.job_session(&stdout, job.started_at);
        let unusable = error.and_then(|error| {
            let failure = self.job_failure(job);
            // The provider's own words (Codex's `turn.failed` is on its
            // stdout) may say when a usage limit resets.
            let said = format!("{error}\n{stdout}");
            self.job_provider_failed(
                job.provider,
                failure,
                (error, &said),
                &HoldJob::Recovery(run.clone()),
                switchable,
            )
        });
        JobEnd { session, unusable }
    }

    /// A recovery job whose provider's process did not start (`failed`,
    /// told as `error`): the provider is held when the role names its
    /// provider (`switchable`) and it cannot be used for that, so that the
    /// next jobs start on the other one, or, with `[provider_fallback] jobs`
    /// off, on it once its hold ends (ADR-t1063-1 decisions 4 and 5,
    /// ADR-t1857-1).
    pub(super) fn recovery_start_failed(
        &mut self,
        run: &RunId,
        launch: &ActorLaunch,
        failed: &anyhow::Error,
        error: &str,
        switchable: bool,
    ) -> Option<(Provider, SwitchReason)> {
        self.job_provider_failed(
            launch.provider,
            crate::application::job_start_failure(failed),
            (error, error),
            &HoldJob::Recovery(run.clone()),
            switchable,
        )
    }
}

impl RecoveryWatch {
    /// Rebuild the watch of an adopted run from its events, so a marker
    /// already handed to a job is not handed again.
    pub(super) fn adopt(queue: &dyn Queue, run: &TaskRun) -> Result<Self> {
        let events = queue.run_events(run.id())?;
        let mut watch = Self::default();
        // The `wait` a `stalled` job answered holds for the adopter too, so
        // it starts no job before the wait is over.
        for reason in IDLE_REASONS {
            let finished = events.iter().rfind(|e| {
                e.kind == "recovery_finished"
                    && e.payload["alert"] == RecoveryAlert::Stalled.as_str()
                    && e.payload["reason"] == reason
            });
            if let Some(at) = finished
                .and_then(|e| e.payload["recheck_at_ms"].as_i64())
                .map(at_millis)
            {
                watch.held.push((RecoveryAlert::Stalled, Some(reason), at));
            }
        }
        Ok(watch)
    }

    /// Stop a job still running: the session ended or the run moved on.
    pub(super) fn stop(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) {
        self.stop_for(sv, run, None, "session_ended");
    }

    /// Stop the job of `alert` (any alert's when `None`), recording
    /// `outcome` as its `recovery_finished`.
    pub(super) fn stop_for(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        alert: Option<RecoveryAlert>,
        outcome: &str,
    ) {
        if let Some(alert) = alert {
            self.held.retain(|(held, _, _)| *held != alert);
        }
        if self
            .job
            .as_ref()
            .is_none_or(|job| alert.is_some_and(|alert| job.alert != alert))
        {
            return;
        }
        self.stop_running(sv, run, outcome);
    }

    /// Stop the job of `alert` raised for `reason`, if one runs, recording
    /// `outcome`; its `wait` is forgotten too.
    pub(super) fn stop_reason(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        alert: RecoveryAlert,
        reason: &'static str,
        outcome: &str,
    ) {
        self.held
            .retain(|(held, why, _)| !(*held == alert && *why == Some(reason)));
        if self.running_for(alert, Some(reason)).is_some() {
            self.stop_running(sv, run, outcome);
        }
    }

    /// The attempt of the job running for `alert` and `reason`, if one runs.
    pub(super) fn running_for(
        &self,
        alert: RecoveryAlert,
        reason: Option<&'static str>,
    ) -> Option<usize> {
        self.job
            .as_ref()
            .filter(|job| job.alert == alert && job.reason == reason)
            .map(|job| job.attempt)
    }

    /// Stop the job that runs, recording `outcome` as its
    /// `recovery_finished` with the Execution its agent was so far
    /// (ADR-t1486-1): the tokens its output gives, or not measured.
    fn stop_running(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun, outcome: &str) {
        let mut job = self.job.take().expect("checked above");
        job.job.stop();
        let agent = sv.job_agent(job.job.provider).unwrap_or(sv.reviewer);
        let stdout = sv.files.read_to_string(&job.job.stdout).unwrap_or_default();
        let session = agent.job_session(&stdout, job.job.started_at);
        let recorded = sv.queue.record_runtime_event(
            run.id(),
            EventKind::RecoveryFinished,
            stopped_end(
                (job.alert, job.reason),
                job.attempt,
                job.marker,
                outcome,
                session.as_ref(),
            ),
        );
        if let Err(error) = recorded {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the stopped recovery job could not be recorded: {error:#}", run.id());
        }
    }

    /// Whether a recovery job is running for the session: a run then waits
    /// for the job, not for a person (ADR-0062 decision 6).
    pub(super) fn running(&self) -> bool {
        self.job.is_some()
    }

    /// Stop the job that runs with no `recovery_finished`: the slot is no
    /// longer watched. Its Execution is still recorded
    /// ([`HeadlessJob::abandon`]).
    pub(super) fn stop_job(&mut self) {
        if let Some(job) = &mut self.job {
            job.job.abandon();
        }
    }

    /// Record `recovery_requested` for `alert` with `facts` and start its
    /// job; `Some` when a person is asked at once instead: the alert got
    /// its jobs already (nothing is recorded), or the job could not start.
    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        live: &Live<'_>,
        alert: RecoveryAlert,
        reason: Option<&'static str>,
        facts: Value,
        marker: Option<(SystemTime, i64)>,
    ) -> Result<Option<(usize, Escalation)>> {
        // A login or usage limit that holds the queue starts no recovery
        // job on Claude (task 437): the alert is followed again once it is
        // fixed. One whose role names its provider may start on Codex
        // meanwhile (ADR-t1063-1 decision 5).
        let Some(route) = sv.recovery_route() else {
            return Ok(None);
        };
        let events = sv.queue.run_events(run.id())?;
        let done = attempts(&events, alert);
        if done >= MAX_RECOVERY_ATTEMPTS {
            return Ok(Some((done, Escalation::UsedUp(done))));
        }
        let attempt = done + 1;
        let evidence: Vec<EventId> = Vec::new();
        let mut payload = json!({
            "alert": alert,
            "attempt": attempt,
            "evidence": evidence,
            "workspace_id": live.workspace,
        });
        if let Some(reason) = reason {
            payload["reason"] = json!(reason);
        }
        // The facts may name the evidence themselves.
        if let (Value::Object(payload), Value::Object(facts)) = (&mut payload, facts) {
            payload.extend(facts);
        }
        // What the job is started with (ADR-0079 decision 7): recorded,
        // not shown to the job among the facts. A job no provider can run
        // under `--no-claude` is recorded on the provider its role names,
        // so that the person sees why.
        let (launch, switchable, unavailable) = match route {
            JobStartRoute::Start(launch, switchable) => (launch, switchable, None),
            JobStartRoute::Unavailable(launch, why) => (launch, false, Some(why)),
        };
        let mut recorded = payload.clone();
        recorded["launch"] = launch.to_value();
        sv.queue
            .record_runtime_event(run.id(), EventKind::RecoveryRequested, recorded)?;
        if let Some(why) = unavailable {
            return Ok(Some((
                attempt,
                Escalation::JobFailed(format!("the recovery job could not start: {why}")),
            )));
        }
        info!(run_id = %run.id(), "run {}: alert {}; recovery job {attempt} starts on {}", run.id(), alert.as_str(), launch.provider.as_str());
        // The outer error is the job's own preparation, the inner one the
        // start of its provider's process: only the latter says whether
        // the provider can be used.
        let started = spawn_live(sv, run, live, alert, attempt, &payload, &launch)
            .map_err(|error| (error, false))
            .and_then(|started| started.map_err(|error| (error, true)));
        match started {
            Ok(job) => {
                self.job = Some(Box::new(RecoveryJob {
                    alert,
                    reason,
                    attempt,
                    marker: marker.map(|(at, _)| at),
                    switchable,

                    job,
                }));
                Ok(None)
            }
            Err((failed, spawned)) => {
                let error = format!("the recovery job could not start: {failed:#}");
                let unusable = spawned
                    .then(|| {
                        sv.recovery_start_failed(run.id(), &launch, &failed, &error, switchable)
                    })
                    .flatten();
                // A provider that cannot be used is held, and the alert's
                // next job starts where the route says then (ADR-t1063-1
                // decision 4, ADR-t1857-1).
                if let Some(unusable) = unusable {
                    let end = JobEnd {
                        session: None,
                        unusable: Some(unusable),
                    };
                    Self::restarted(
                        sv,
                        run,
                        (alert, reason),
                        attempt,
                        marker.map(|(at, _)| at),
                        &error,
                        &end,
                    )?;
                    return Ok(None);
                }
                Ok(Some((attempt, Escalation::JobFailed(error))))
            }
        }
    }

    /// The job of `alert` that ended, with its verdict or why there is
    /// none.
    fn ended(
        &mut self,
        sv: &Supervisor<'_>,
        alert: RecoveryAlert,
        reason: Option<&'static str>,
    ) -> Result<Option<(Box<RecoveryJob>, JobOutcome)>> {
        let Some(job) = self
            .job
            .as_mut()
            .filter(|job| job.alert == alert && job.reason == reason)
        else {
            return Ok(None);
        };
        // The job's reply is read by the provider it ran on (ADR-t1063-1
        // decision 2).
        let agent = sv.job_agent(job.job.provider).unwrap_or(sv.reviewer);
        let Some(output) = job.job.poll(&*sv.files, agent)? else {
            return Ok(None);
        };
        let job = self.job.take().expect("polled above");
        Ok(Some((
            job,
            output.and_then(|stdout| RecoveryVerdict::parse(&stdout)),
        )))
    }

    pub(super) fn follow(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        live: &Live<'_>,
        alert: RecoveryAlert,
        facts: impl FnOnce() -> Value,
    ) -> Result<LiveStep> {
        self.follow_for(sv, run, live, alert, None, facts)
    }

    /// [`Self::follow`] for an alert raised for `reason`: its job, its
    /// `wait` and its verdict are that reason's (the `stalled` alert of an
    /// idle and of a send not taken share the attempts and the one job at
    /// a time).
    pub(super) fn follow_for(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        live: &Live<'_>,
        alert: RecoveryAlert,
        reason: Option<&'static str>,
        facts: impl FnOnce() -> Value,
    ) -> Result<LiveStep> {
        if let Some((job, verdict)) = self.ended(sv, alert, reason)? {
            let agent = sv.job_agent(job.job.provider).unwrap_or(sv.reviewer);
            let verdict = match verdict {
                Ok(verdict) => verdict,
                // Stopped at a wall only a person moves: it joins the hold
                // ask and no failure is recorded; the next job of the alert
                // starts once the hold ends (task 438). Only Claude's walls
                // hold the queue.
                Err(error)
                    if let Some(wall) = sv.job_wall(&job.job)
                        && sv.raise_job_wall(
                            wall,
                            &HoldJob::Recovery(run.id().clone()),
                            &error,
                        ) =>
                {
                    return Ok(LiveStep::Pending);
                }
                // A provider that could not be used (its login, its usage
                // limit, an agent that did not start) of a role that names
                // its provider is held, and the alert's next job starts on
                // the other one (ADR-t1063-1 decision 4), or, with
                // `[provider_fallback] jobs` off, on the same one once its
                // hold ends (ADR-t1857-1), or, under
                // `--no-claude` with none left, goes to its ask told why.
                // Any other failure (a non-zero exit, the time limit, a
                // verdict that does not parse) is the alert's own ask
                // (ADR-t609-1), whatever the provider: a Codex job that
                // failed never moves to Claude.
                Err(error) => {
                    let end = sv.recovery_job_end(
                        run.id(),
                        &job.job,
                        agent,
                        job.switchable,
                        Some(&error),
                    );
                    if end.unusable.is_some() {
                        Self::restarted(
                            sv,
                            run,
                            (alert, reason),
                            job.attempt,
                            job.marker,
                            &error,
                            &end,
                        )?;
                        return Ok(LiveStep::Pending);
                    }
                    sv.triage
                        .live_job_ends
                        .push((run.id().clone(), alert, job.attempt, end));
                    return Ok(LiveStep::Escalate(
                        job.attempt,
                        Escalation::JobFailed(error),
                    ));
                }
            };
            let end = sv.recovery_job_end(run.id(), &job.job, agent, job.switchable, None);
            return Ok(match apply_live(sv, run, live, &job, verdict, end)? {
                Ok(applied) => {
                    if let Some(at) = applied_recheck(sv, &job, run)? {
                        self.held
                            .retain(|(held, why, _)| !(*held == alert && *why == reason));
                        self.held.push((alert, reason, at));
                    }
                    LiveStep::Repaired(applied)
                }
                Err(escalation) => LiveStep::Escalate(job.attempt, escalation),
            });
        }
        if self.job.is_some()
            || self
                .held
                .iter()
                .any(|&(held, why, at)| held == alert && why == reason && sv.files.now() < at)
        {
            return Ok(LiveStep::Pending);
        }
        if failed_live(&sv.queue.run_events(run.id())?, Some(alert)).is_some() {
            return Ok(LiveStep::Pending);
        }
        self.held
            .retain(|(held, why, _)| !(*held == alert && *why == reason));
        Ok(
            match self.start(sv, run, live, alert, reason, facts(), None)? {
                Some((attempt, escalation)) => LiveStep::Escalate(attempt, escalation),
                None => LiveStep::Pending,
            },
        )
    }
    /// Record the end of the live `alert`'s job `attempt` whose provider could
    /// not be used (`end`'s `provider_unusable`, told as `error`): its
    /// `recovery_finished` (`outcome: job_failed`, no ask), after which the
    /// alert's next job starts on the provider the route gives then
    /// (ADR-t1063-1 decision 4, task 1225).
    fn restarted(
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        (alert, reason): (RecoveryAlert, Option<&'static str>),
        attempt: usize,
        marker: Option<SystemTime>,
        error: &str,
        end: &JobEnd,
    ) -> Result<()> {
        let mut finished = json!({
            "alert": alert,
            "reason": reason,
            "attempt": attempt,
            "outcome": "job_failed",
            "applied": [],
            "escalated": false,
            "error": error,
            "marker_at_ms": marker.map(millis),
        });
        end.record(&mut finished);
        sv.queue
            .record_runtime_event(run.id(), EventKind::RecoveryFinished, finished)?;
        let next = end.unusable.map_or_else(String::new, |(provider, _)| {
            again_on(sv.provider.fallback.jobs, provider)
        });
        warn!(run_id = %run.id(), "run {}: recovery job {attempt} of {} failed: {error}; its provider cannot be used, and the next job starts {next}", run.id(), alert.as_str());
        Ok(())
    }

    /// One look at the run's processes for the `idle_process` alert (task
    /// 469): follow its job in progress, or, at each new process sample,
    /// start one when a process of the run and its descendants have used
    /// almost no CPU time for `[stall].idle_process_secs`
    /// ([`CpuWatch::idle`]) and no other alert's job runs. Processes handed
    /// to a job are not an alert again until they make progress, unless
    /// its verdict was `wait`. `phase` names where the session is, for the
    /// facts and for what an escalation does.
    pub(super) fn watch_idle(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        live: &Live<'_>,
        phase: &str,
    ) -> Result<LiveStep> {
        let alert = RecoveryAlert::IdleProcess;
        if self.job.as_ref().is_some_and(|job| job.alert == alert) {
            let step = self.follow(sv, run, live, alert, || json!({}))?;
            // A job that ended with nothing applied nor asked (a wall, a
            // provider that could not be used) leaves its processes to
            // raise the alert again.
            if matches!(step, LiveStep::Pending) && self.job.is_none() {
                self.cpu = std::mem::take(&mut self.cpu).released();
            }
            return Ok(self.after_idle(step));
        }
        if self.job.is_some() {
            return Ok(LiveStep::Pending);
        }
        let threshold = sv.stall.idle_process_secs;
        let interval = sample_interval(sv.stall.idle_process());
        if self.idle_watched.get_or_insert_with(Instant::now).elapsed() < interval {
            return Ok(LiveStep::Pending);
        }
        let Some((at, all)) = process_sample(sv, interval, self.cpu_sampled) else {
            return Ok(LiveStep::Pending);
        };
        self.cpu_sampled = Some(at);
        // Without the session's wrapper and agent registered there are no
        // processes of the run to judge.
        let Ok(own) = own_of(sv, run, &all) else {
            return Ok(LiveStep::Pending);
        };
        let agent = sv
            .queue
            .processes(run.id())?
            .iter()
            .find(|p| p.role == "agent")
            .and_then(|agent| all.iter().find(|p| p.pid == agent.pid))
            .cloned();
        let own = without_session_helpers(own, agent.as_ref());
        let at_ms = millis(at);
        self.cpu.observe(&own, at_ms);
        let idle = self.cpu.idle(&own, at_ms, sv.stall.idle_process());
        if idle.is_empty() {
            return Ok(LiveStep::Pending);
        }
        let facts = json!({
            "idle_processes": idle,
            "threshold": IDLE_PROCESS_THRESHOLD,
            "threshold_secs": threshold,
            "progress_cpu_per_mille": PROGRESS_CPU_PER_MILLE,
            "phase": phase,
        });
        let step = self.follow(sv, run, live, alert, || facts)?;
        // Handed to a job (or straight to a person): not again until the
        // processes make progress. A `wait` that holds the alert leaves
        // them for the next look.
        if self.job.is_some() || matches!(step, LiveStep::Escalate(..)) {
            info!(run_id = %run.id(), "run {}: processes {:?} have used almost no CPU time for {threshold}s", run.id(), idle.iter().map(|p| p.pid).collect::<Vec<_>>());
            self.cpu.hand(&idle);
            self.idle = idle;
        }
        Ok(self.after_idle(step))
    }

    /// A `wait` applied to an `idle_process` alert lets the same processes
    /// raise it again once the wait is over.
    fn after_idle(&mut self, step: LiveStep) -> LiveStep {
        if let LiveStep::Repaired(applied) = &step
            && applied.names.contains(&"wait")
        {
            self.cpu = std::mem::take(&mut self.cpu).released();
        }
        step
    }

    /// The idle processes last handed to a job, for its ask.
    pub(super) fn idle_processes(&self) -> &[IdleProcess] {
        &self.idle
    }
}

/// How often the processes are sampled for the `idle_process` alert: a
/// tenth of its threshold in whole seconds, between one second and a
/// minute, and no longer than the threshold (one a test set below a
/// second, task 1045).
fn sample_interval(threshold: Duration) -> Duration {
    Duration::from_secs((threshold.as_secs() / 10).clamp(1, 60)).min(threshold)
}

/// The user's processes as the supervisor listed them at most `interval`
/// ago (one listing serves every run), with the time of the listing;
/// `None` when that listing is the one taken `last`, or when they could
/// not be listed (tried again after `interval`).
fn process_sample(
    sv: &mut Supervisor<'_>,
    interval: Duration,
    last: Option<SystemTime>,
) -> Option<(SystemTime, Vec<ProcessInfo>)> {
    if sv
        .triage
        .process_sample
        .as_ref()
        .is_none_or(|(at, _)| at.elapsed() >= interval)
    {
        let sample = match sv.processes.list() {
            Ok(all) => Some((sv.files.now(), all)),
            Err(error) => {
                tracing::debug!(error = %format_args!("{error:#}"), "the processes could not be listed for the idle_process alert: {error:#}");
                None
            }
        };
        sv.triage.process_sample = Some((Instant::now(), sample));
    }
    sv.triage
        .process_sample
        .as_ref()
        .and_then(|(_, sample)| sample.as_ref())
        .filter(|(at, _)| Some(*at) != last)
        .cloned()
}

pub(super) fn leave_idle_to_phase(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    attempt: usize,
    escalation: &Escalation,
    phase: &str,
) -> Result<()> {
    leave_to_phase(
        sv,
        run,
        RecoveryAlert::IdleProcess,
        attempt,
        escalation,
        phase,
        json!({}),
    )
}

pub(super) fn leave_to_phase(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    alert: RecoveryAlert,
    attempt: usize,
    escalation: &Escalation,
    phase: &str,
    mut extra: Value,
) -> Result<()> {
    let note = escalation.note(run, alert, attempt);
    warn!(run_id = %run.id(), "run {}: {}: {}; left to the {phase} phase's own timeout", run.id(), alert.as_str(), note.why);
    extra["outcome"] = json!("left_to_phase");
    extra["phase"] = json!(phase);
    sv.for_escalation(run, alert, attempt, escalation, |sv| {
        escalation.record(sv, run, alert, attempt, &note, None, extra)
    })
}

/// When the `wait` of the job's applied verdict ends, from its
/// `recovery_finished`.
fn applied_recheck(
    sv: &Supervisor<'_>,
    job: &RecoveryJob,
    run: &TaskRun,
) -> Result<Option<SystemTime>> {
    Ok(sv
        .queue
        .run_events(run.id())?
        .iter()
        .rev()
        .find(|e| {
            e.kind == event_kind::RECOVERY_FINISHED
                && e.payload["alert"] == job.alert.as_str()
                && e.payload["attempt"] == job.attempt
        })
        .and_then(|e| e.payload["recheck_at_ms"].as_i64())
        .map(at_millis))
}

/// The run's processes that `stop_processes` may stop: those of its
/// worktree or under its session, never the session's wrapper or agent.
fn own_processes(sv: &Supervisor<'_>, run: &TaskRun) -> Result<Vec<ProcessInfo>> {
    let all = sv.processes.list()?;
    own_of(sv, run, &all)
}

/// The run's processes among `all`, as [`own_processes`] picks them.
fn own_of(sv: &Supervisor<'_>, run: &TaskRun, all: &[ProcessInfo]) -> Result<Vec<ProcessInfo>> {
    let worktree = run.worktree_path().context("the run has no worktree")?;
    let processes = sv.queue.processes(run.id())?;
    let pid = |role: &str| {
        processes
            .iter()
            .find(|p| p.role == role)
            .map(|p| p.pid)
            .with_context(|| format!("the session's {role} is not registered"))
    };
    Ok(run_processes(
        all,
        Path::new(worktree),
        Some(pid("wrapper")?),
        Some(pid("agent")?),
        std::process::id(),
    )
    .into_iter()
    .cloned()
    .collect())
}

/// Write the job's prompt with what the runtime reads now and start it in
/// the run directory, allowed to read only. The outer error is the job's
/// own preparation, the inner one the start of its provider's process
/// ([`start_job`]).
fn spawn_live(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    live: &Live<'_>,
    alert: RecoveryAlert,
    attempt: usize,
    facts: &Value,
    launch: &ActorLaunch,
) -> Result<Result<HeadlessJob>> {
    let detail = sv.queue.show(run.task_id())?;
    let binary = binary_facts_of(sv, run, &detail)?;
    let task = detail.task;
    // A headless session has no screen: its last turns stand in for it.
    let screen = { turns_excerpt(sv, run) };
    let listed = own_processes(sv, run).map_err(|error| format!("{error:#}"));
    let (status, head, receipt) = git_facts(sv, run)?;
    let mut history = repair_history(sv, run)?;
    history.extend(
        detail
            .events
            .iter()
            .filter(|event| event.kind == event_kind::TASK_EDITED)
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?,
    );
    let material = RecoveryMaterial {
        alert,
        ended: None,
        facts,
        workspace: live.workspace,
        screen: &screen,
        processes: listed,
        git_status: &status,
        head: &head,
        receipt_commit: receipt.as_deref(),
        history: &history,
        allowed: live.allowed,
        binary: &binary,
    };
    let prompt = recovery_prompt(&task, run, attempt, &material)?
        .with_language(sv.verifier.language().as_ref());
    start_job(
        sv,
        run.id(),
        live.run_dir,
        (alert, attempt),
        (&prompt, &binary),
        None,
        launch,
    )
}

/// The queue's `update_*` events the recovery job's [`BinaryFacts`] reads
/// at most, newest first: a few days of landings.
const BINARY_UPDATE_EVENTS: usize = 200;

/// What the recovery job of `run` reads of the binary the supervisor runs
/// (task 1633): this process's build identifier, the replacements since
/// the claim, and whether the build holds the landing commit of each of
/// the task's dependencies (`detail`).
pub(super) fn binary_facts_of(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    detail: &crate::domain::TaskDetail,
) -> Result<BinaryFacts> {
    let updates = sv.queue.update_events(BINARY_UPDATE_EVENTS)?;
    let mut landings = Vec::new();
    for dependency in &detail.dependencies {
        let events = sv.queue.show(*dependency)?.events;
        let landed = crate::domain::areas::landed_commits(&events)
            .pop()
            .map(|(_, commit)| commit);
        landings.push((*dependency, landed));
    }
    let repository = sv.repository.clone();
    Ok(binary_facts(
        &sv.layout.version,
        run,
        &detail.events,
        &updates,
        &landings,
        &|commit, build| repository.is_ancestor(commit, build),
    ))
}

/// The worktree's `git status`, HEAD and the receipt's `commit`, for the
/// recovery job; what cannot be read says so.
pub(super) fn git_facts(
    sv: &Supervisor<'_>,
    run: &TaskRun,
) -> Result<(String, String, Option<String>)> {
    let worktree = run.worktree_path().map(Path::new);
    let status = match worktree {
        Some(worktree) if sv.files.is_dir(worktree) => sv
            .repository
            .status(worktree)
            .unwrap_or_else(|error| format!("(unreadable: {error:#})")),
        _ => "(no worktree)".to_owned(),
    };
    let head = match worktree {
        Some(worktree) if sv.files.is_dir(worktree) => sv.repository.head(worktree).map_or_else(
            |error| format!("(unreadable: {error:#})"),
            |h| h.to_string(),
        ),
        _ => "(no worktree)".to_owned(),
    };
    let receipt = run
        .receipt_path()
        .and_then(|path| sv.files.read(Path::new(path)).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|receipt| receipt["commit"].as_str().map(str::to_owned));
    Ok((status, head, receipt))
}

/// The run's earlier recovery verdicts and automatic repairs.
pub(super) fn repair_history(sv: &Supervisor<'_>, run: &TaskRun) -> Result<Vec<Value>> {
    Ok(sv
        .queue
        .run_events(run.id())?
        .iter()
        .filter(|e| {
            matches!(
                e.kind.as_str(),
                event_kind::RECOVERY_FINISHED | event_kind::AUTO_REPAIRED
            )
        })
        .map(super::super::health::compact_event)
        .collect())
}

/// Write `prompt` next to the run and start the headless job in `dir`,
/// allowed to read only; the job's environment and CLI are the review's.
/// `session_id` is the Claude session id the job runs as, when its start
/// recorded one (ADR-0048 decision 4); `launch` the provider, model and
/// effort it starts with (ADR-0079 decision 7, ADR-t1063-1): Claude
/// Code's `claude -p`, or Codex's `codex exec --json` in its read-only
/// sandbox, as [`TRIAGE_ACCESS`] says. The outer error is the job's own
/// preparation (its prompt and files), the inner one the start of its
/// provider's process: only the latter says whether the provider can be
/// used.
#[allow(clippy::too_many_arguments)]
pub(super) fn start_job(
    sv: &mut Supervisor<'_>,
    run: &RunId,
    dir: &Path,
    (alert, attempt): (RecoveryAlert, usize),
    (prompt, binary): (&FittedPrompt, &BinaryFacts),
    session_id: Option<&str>,
    launch: &ActorLaunch,
) -> Result<Result<HeadlessJob>> {
    sv.files
        .create_dir_all(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    sv.files.write(
        &dir.join(job_file(alert, attempt, "prompt.txt")),
        prompt.text.as_bytes(),
    )?;
    // All of what the prompt's binary sections hold to their limits, for
    // the job to read (task 1633).
    sv.files.write(
        &dir.join(recovery_binary_file(alert, attempt)),
        &serde_json::to_vec_pretty(binary)?,
    )?;
    // What the prompt takes (task 1571, ADR-t1566-1 decision 6).
    sv.queue.record_runtime_event(
        run,
        EventKind::RecoveryPromptWritten,
        json!({"alert": alert, "attempt": attempt, "prompt_bytes": prompt.bytes}),
    )?;
    let prompt = prompt.text.as_str();
    let stdout = dir.join(job_file(alert, attempt, "out"));
    let stderr = dir.join(job_file(alert, attempt, "err"));
    Ok(spawn_job(
        sv,
        run,
        dir,
        (alert, attempt),
        prompt,
        session_id,
        launch,
        (stdout, stderr),
    ))
}

/// Start the provider's process of a recovery job ([`start_job`]).
#[allow(clippy::too_many_arguments)]
fn spawn_job(
    sv: &mut Supervisor<'_>,
    run: &RunId,
    dir: &Path,
    (alert, attempt): (RecoveryAlert, usize),
    prompt: &str,
    session_id: Option<&str>,
    launch: &ActorLaunch,
    (stdout, stderr): (PathBuf, PathBuf),
) -> Result<HeadlessJob> {
    let agent = sv
        .job_agent(launch.provider)
        .with_context(|| format!("no {} runs on this supervisor", launch.provider.as_str()))?;
    let child = sv
        .actors_on(agent)
        .spawn(ActorExecutionSpec::new(
            ActorContext::recovery_job(run, alert.as_str(), attempt),
            WorkspaceAccess::Scratch(dir.to_path_buf()),
            ActorProgram::Headless {
                program: HeadlessProgram::Job {
                    cwd: dir,
                    prompt,
                    access: TRIAGE_ACCESS,
                },
                session_id,
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
        .context("start the recovery job")?
        .process()?;
    Ok(sv.headless_job(
        "recovery job",
        child,
        stdout,
        stderr,
        JobSubject {
            label: Some(alert.as_str().to_owned()),
            provider: launch.provider,
            ..JobSubject::run(headless_job::RECOVERY, run, attempt)
        },
    ))
}

/// Check each action's preconditions now (ADR-0047 decision 40): only the
/// actions allowed for the alert here, only the run's own processes, an
/// instruction only into a session at its prompt, a known dialog only
/// under its rule, and a close only of a run that lands and whose receipt
/// still holds. The first that fails refuses the whole verdict.
fn check_live(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    live: &Live<'_>,
    actions: &[RecoveryAction],
) -> std::result::Result<(), String> {
    for action in actions {
        // A headless session has no dialog and no /exit, whatever the
        // alert allows (ADR-t813-1 decision 9).
        if HEADLESS_NEVER.contains(&action.name()) {
            return Err(format!(
                "{} does not apply to a headless session",
                action.name()
            ));
        }
        if !live.allowed.contains(&action.name()) {
            return Err(format!(
                "{} does not apply to this alert of a live session (allowed: {})",
                action.name(),
                live.allowed.join(", ")
            ));
        }
        match action {
            RecoveryAction::StopProcesses { pids } => {
                if pids.is_empty() {
                    return Err("stop_processes names no pid".to_owned());
                }
                let own = own_processes(sv, run).map_err(|error| {
                    format!("the run's processes could not be listed: {error:#}")
                })?;
                if let Some(pid) = pids.iter().find(|pid| !own.iter().any(|p| p.pid == **pid)) {
                    return Err(format!(
                        "pid {pid} is not one of the run's own processes (working directory in its worktree, or under its session; never the session itself)"
                    ));
                }
            }
            RecoveryAction::SendInstruction { instruction } => {
                if instruction.trim().is_empty() {
                    return Err("send_instruction has no instruction".to_owned());
                }
                if !live.at_prompt {
                    return Err(
                        "the session is not idle at its prompt for an instruction".to_owned()
                    );
                }
            }

            RecoveryAction::Resume { .. } => {
                if !live.park {
                    return Err(
                        "resume: only a run in its first session is parked for a session of its own"
                            .to_owned(),
                    );
                }
                if resumes_exhausted(&*sv.queue, run.id(), sv.resume.config) {
                    return Err("resume: the run's resumes are used up".to_owned());
                }
            }
            RecoveryAction::Wait { .. } => (),
            other => {
                return Err(format!("{} does not apply to a live session", other.name()));
            }
        }
    }
    Ok(())
}

/// Apply a verdict to a live session: a `repair` of high confidence whose
/// every action holds now, each recorded as `auto_repaired`, and
/// `recovery_finished`; otherwise the escalation.
pub(super) fn apply_live(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    live: &Live<'_>,
    job: &RecoveryJob,
    verdict: RecoveryVerdict,
    end: JobEnd,
) -> Result<std::result::Result<Applied, Escalation>> {
    let actor = ActorContext::recovery_job(run.id(), job.alert.as_str(), job.attempt);
    let applied = sv.for_job(&actor, |sv| {
        apply_live_verdict(sv, run, live, job, verdict, &end)
    })?;
    // An escalation records the job's end with its `recovery_finished`.
    if applied.is_err() {
        sv.triage
            .live_job_ends
            .push((run.id().clone(), job.alert, job.attempt, end));
    }
    Ok(applied)
}

/// [`apply_live`] with the job recorded as the requester.
fn apply_live_verdict(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    live: &Live<'_>,
    job: &RecoveryJob,
    verdict: RecoveryVerdict,
    end: &JobEnd,
) -> Result<std::result::Result<Applied, Escalation>> {
    let duration_secs = job.job.started.elapsed().as_secs();
    if !verdict.applies() {
        return Ok(Err(Escalation::Verdict(verdict)));
    }
    if let Err(why) = check_live(sv, run, live, &verdict.actions) {
        warn!(run_id = %run.id(), "run {}: recovery job {} of {} answered repair, but {why}; asking the inbox", run.id(), job.attempt, job.alert.as_str());
        return Ok(Err(Escalation::Refused(verdict, why)));
    }
    let mut applied = Applied::default();
    let mut recheck_at = None;
    let repaired = |sv: &mut Supervisor<'_>, action: &RecoveryAction, detail: Value| {
        let mut payload = json!({
            "layer": "recovery",
            "repair": action.name(),
            "alert": job.alert,
            "attempt": job.attempt,
        });
        if let (Value::Object(payload), Value::Object(detail)) = (&mut payload, detail) {
            payload.extend(detail);
        }
        sv.queue
            .record_runtime_event(run.id(), EventKind::AutoRepaired, payload)
    };
    for action in &verdict.actions {
        match action {
            RecoveryAction::StopProcesses { pids } => {
                let stopped = match stop_processes(sv, run, pids) {
                    Ok(stopped) => stopped,
                    Err(error) => {
                        return Ok(Err(Escalation::Refused(
                            verdict.clone(),
                            format!("stopping the processes failed: {error:#}"),
                        )));
                    }
                };
                repaired(sv, action, json!({"processes": stopped}))?;
                info!(run_id = %run.id(), "run {}: recovery job {} stopped processes {pids:?} of its worktree", run.id(), job.attempt);
            }
            RecoveryAction::SendInstruction { instruction } => {
                let text = recovery_instruction(run, job.alert.as_str(), instruction);
                let sent_at = sv.files.now();
                let submission = match submit(
                    sv,
                    run,
                    live.workspace,
                    Input::from(&text),
                    "recovery instruction",
                ) {
                    Ok(submission) => submission,
                    Err(error) => {
                        return Ok(Err(Escalation::Refused(
                            verdict.clone(),
                            format!("the instruction could not be typed: {error:#}"),
                        )));
                    }
                };
                repaired(
                    sv,
                    action,
                    json!({"instruction": instruction, "workspace_id": live.workspace}),
                )?;
                applied.sent = Some((text.text, sent_at, submission));
            }

            RecoveryAction::Resume { instruction } => {
                let instruction = if instruction.trim().is_empty() {
                    verdict.diagnosis.trim().to_owned()
                } else {
                    instruction.trim().to_owned()
                };
                repaired(sv, action, json!({"instruction": instruction}))?;
                applied.resume = Some(instruction);
            }
            RecoveryAction::Wait { recheck_after_secs } => {
                let at = sv.files.now()
                    + Duration::from_secs((*recheck_after_secs).min(MAX_RECHECK_SECS));
                recheck_at = Some(millis(at));
            }
            _ => unreachable!("checked above"),
        }
        applied.names.push(action.name());
    }
    let mut finished = json!({
        "alert": job.alert,
        "reason": job.reason,
        "attempt": job.attempt,
        "verdict": verdict.verdict,
        "confidence": verdict.confidence,
        "diagnosis": verdict.diagnosis,
        "applied": applied.names,
        "escalated": false,
        "marker_at_ms": job.marker.map(millis),
        "recheck_at_ms": recheck_at,
        "duration_secs": duration_secs,
    });
    end.record(&mut finished);
    sv.queue
        .record_runtime_event(run.id(), EventKind::RecoveryFinished, finished)?;
    info!(run_id = %run.id(), "run {}: recovery job {} of {} repaired it ({}): {}", run.id(), job.attempt, job.alert.as_str(), applied.names.join(", "), verdict.diagnosis);
    Ok(Ok(applied))
}

impl SessionWatch {
    pub(super) fn watch_idle_processes(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<()> {
        if !self.recovery.running() && sv.queue.has_unclosed_ask(run.id(), AskKind::Stalled)? {
            return Ok(());
        }
        let phase = if self.receipt_seen {
            "after_receipt"
        } else {
            "session"
        };
        let live = Live {
            workspace: &self.workspace,
            run_dir: &self.run_dir,
            allowed: &IDLE_PROCESS_ACTIONS,

            at_prompt: self.at_prompt(sv, run),

            park: false,
        };
        match self.recovery.watch_idle(sv, run, &live, phase)? {
            LiveStep::Pending => Ok(()),
            LiveStep::Repaired(applied) => {
                if let Some((text, sent_at, _submission)) = applied.sent {
                    self.stall.input_sent(sent_at, Some(&text));
                    self.answer_start = Some(sent_at);
                }
                Ok(())
            }
            LiveStep::Escalate(attempt, escalation) if self.receipt_seen => {
                leave_idle_to_phase(sv, run, attempt, &escalation, phase)
            }
            LiveStep::Escalate(attempt, escalation) => {
                self.escalate_idle(sv, run, attempt, &escalation)
            }
        }
    }

    /// Raise the `idle_process` alert to the inbox as a `stalled` ask with
    /// the idle processes, the job's diagnosis and why a person is needed,
    /// and hand the ask to the [`StallWatch`].
    fn escalate_idle(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        attempt: usize,
        escalation: &Escalation,
    ) -> Result<()> {
        sv.for_escalation(run, RecoveryAlert::IdleProcess, attempt, escalation, |sv| {
            self.ask_idle(sv, run, attempt, escalation)
        })
    }

    /// [`Self::escalate_idle`] with the job recorded as the requester when
    /// its verdict asked for it.
    fn ask_idle(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        attempt: usize,
        escalation: &Escalation,
    ) -> Result<()> {
        let alert = RecoveryAlert::IdleProcess;
        let note = escalation.note(run, alert, attempt);
        if sv.queue.has_unclosed_ask(run.id(), AskKind::Stalled)? {
            escalation.record(sv, run, alert, attempt, &note, None, json!({}))?;
            warn!(run_id = %run.id(), "run {}: {}; a stalled ask is already open, so no other is asked", run.id(), note.why);
            return Ok(());
        }
        let idle = self.recovery.idle_processes();
        let listed = idle
            .iter()
            .map(|p| {
                format!(
                    "pid {} (parent {}, {}s without progress, {}ms of CPU time in it): {}",
                    p.pid,
                    p.ppid,
                    p.idle_secs,
                    p.cpu_growth_ms,
                    crate::application::tail(&p.command, 200)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let idle_secs = idle.iter().map(|p| p.idle_secs).max().unwrap_or(0);
        let question = {
            // No screen and no keys: its turns, and no `intervene` (task
            // 1179).
            let situation = format!(
                "The headless session of run {run_id} (task {task_id}) has processes that have used almost no CPU time for over {threshold}s (alert: idle_process): {listed}. And {why}.\n{text}",
                run_id = run.id(),
                task_id = run.task_id(),
                threshold = sv.stall.idle_process_secs,
                why = note.why,
                text = note.text.trim_end(),
            );
            super::stall::headless_ask_text(run.id(), &situation, "", &turns_excerpt(sv, run))
        };
        let mut options = ask_options(&STALLED_OPTIONS, Some(&note));
        // A headless session takes no keys: no `intervene` (task 1179).
        {
            options = super::stall::headless_options(options);
        }
        let outcome = ask::ask(
            &mut *sv.queue,
            NewAsk {
                recommendation: None,
                confidence: None,
                kind: AskKind::Stalled,
                task_id: Some(run.task_id()),
                run_id: Some(run.id().clone()),
                question,
                options,
                asked_by: SessionRole::Supervisor.as_str().into(),
                reason_category: note.category,
                topics: Vec::new(),
                finding_id: None,
                request_id: None,
            },
        )?;
        let id = AskId::new(outcome["id"].as_i64().context("ask returned no id")?);
        let now = sv.files.now();
        self.stall
            .escalated(id, now, idle_secs, IDLE_PROCESS_THRESHOLD);
        escalation.record(sv, run, alert, attempt, &note, Some(id), json!({}))?;
        warn!(ask_id = %id, run_id = %run.id(), "run {}: {}; stalled ask {id} (notified: {})", run.id(), note.why, outcome["notified"]);
        Ok(())
    }
}

/// Stop `pids` (SIGTERM, then SIGKILL after [`STOP_GRACE`]), checking once
/// more right before that each is the run's own. Returns what was stopped.
fn stop_processes(sv: &Supervisor<'_>, run: &TaskRun, pids: &[u32]) -> Result<Vec<Value>> {
    let own = own_processes(sv, run)?;
    // A pid no longer among the run's own ended by itself (or is another
    // process now): it is left alone and recorded as gone.
    let (targets, gone): (Vec<_>, Vec<_>) = pids
        .iter()
        .map(|pid| (pid, own.iter().find(|p| p.pid == *pid)))
        .partition(|(_, found)| found.is_some());
    let targets: Vec<&ProcessInfo> = targets.into_iter().filter_map(|(_, found)| found).collect();
    for process in &targets {
        if let Err(error) = sv.processes.terminate(process.pid)
            && sv.processes.alive(process.pid)
        {
            return Err(error.context(format!("stop pid {}", process.pid)));
        }
    }
    let started = Instant::now();
    while started.elapsed() < STOP_GRACE && targets.iter().any(|p| sv.processes.alive(p.pid)) {
        thread::sleep(Duration::from_millis(50));
    }
    let mut stopped = Vec::new();
    for process in targets {
        let killed = sv.processes.alive(process.pid);
        if killed
            && let Err(error) = sv.processes.kill(process.pid)
            && sv.processes.alive(process.pid)
        {
            return Err(error.context(format!("kill pid {}", process.pid)));
        }
        stopped.push(json!({
            "pid": process.pid,
            "ppid": process.ppid,
            "command": process.command,
            "cwd": process.cwd,
            "killed": killed,
        }));
    }
    for (pid, _) in gone {
        stopped.push(json!({"pid": pid, "gone": true}));
    }
    Ok(stopped)
}

/// A run of task 1 in `/runs/r1` with `status` and `last_error`, for the
/// unit tests of an escalation's ask here and in `triage.rs`.
#[cfg(test)]
pub(super) fn test_run(status: RunStatus, last_error: Option<&str>) -> TaskRun {
    TaskRun::restore(crate::domain::RunRecord {
        id: RunId::new("r1").unwrap(),
        task_id: TaskId::new(1),
        status,
        requested_provider: Provider::Claude,
        actual_provider: Provider::Claude,
        worker_mode: crate::domain::worker::Worker::default_mode(Provider::Claude),
        base_commit: CommitSha::parse("a".repeat(40), "commit").unwrap(),
        branch: Some("dagq/r1".to_owned()),
        worktree_path: Some("/runs/r1/worktree".to_owned()),
        workspace_id: None,
        receipt_path: Some("/runs/r1/receipt.json".to_owned()),
        log_path: None,
        result_commit: None,
        repo_path: None,
        run_dir: Some("/runs/r1".to_owned()),
        last_error: last_error.map(str::to_owned),
        workspace_closed_at: None,
        created_at: String::new(),
    })
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(json: Value) -> RecoveryVerdict {
        RecoveryVerdict::parse(&json.to_string()).unwrap()
    }

    /// What a failed job's escalation tells the inbox and records, for
    /// every alert (ADR-t609-1, moved from `runtime_job_verdicts`, task
    /// 1415): why it failed with the job's error, `recovery_failed` as the
    /// reason, no option of a job, the job's material, and nobody asking
    /// for it but the supervisor; its `recovery_finished` applies nothing,
    /// names the ask and the error, with `outcome: job_failed`.
    #[test]
    fn a_failed_job_escalates_with_recovery_failed_and_applies_nothing() {
        let run = test_run(RunStatus::Running, None);
        let error = "the recovery job printed no verdict JSON: unknown field `force`";
        for alert in [
            RecoveryAlert::LongBackground,
            RecoveryAlert::StuckExit,
            RecoveryAlert::PromptWaiting,
            RecoveryAlert::Failed,
        ] {
            let failed = Escalation::JobFailed(error.to_owned());
            assert!(failed.verdict().is_none());
            assert!(failed.requester(&run, alert, 2).is_none());
            let note = failed.note(&run, alert, 2);
            assert_eq!(note.why, format!("the recovery job failed ({error})"));
            assert_eq!(note.category, AskReason::RecoveryFailed);
            assert!(note.options.is_empty());
            assert_eq!(
                note.text,
                format!(
                    "Why a person: recovery_failed\nRecovery material: /runs/r1/recovery-{}-2.prompt.txt",
                    alert.as_str()
                )
            );
            let finished = failed.finished(
                alert,
                2,
                &note,
                Some(AskId::new(7)),
                json!({"marker_at_ms": 5}),
            );
            assert_eq!(
                finished,
                json!({
                    "alert": alert, "attempt": 2, "verdict": null, "confidence": null,
                    "diagnosis": null, "applied": [], "escalated": true, "why": note.why,
                    "reason_category": "recovery_failed", "ask_id": 7,
                    "outcome": "job_failed", "error": error, "marker_at_ms": 5,
                })
            );
        }
        // An open ask of the run already has a person looking.
        let failed = Escalation::JobFailed(error.to_owned());
        let note = failed.note(&run, RecoveryAlert::StuckExit, 1);
        let finished = failed.finished(RecoveryAlert::StuckExit, 1, &note, None, json!({}));
        assert_eq!(finished["escalated"], false);
        assert_eq!(finished["outcome"], "job_failed");
    }

    /// The other escalations: the job's `escalate`, a repair of low
    /// confidence, one the runtime refused and an alert past its jobs
    /// (moved from `runtime_triage`, task 1415). The job's verdict carries
    /// its diagnosis, recommendation, question, options (trimmed, once) and
    /// its `discard` or `scope` reason, and asks at the job's request.
    #[test]
    fn an_escalation_tells_why_and_carries_the_jobs_verdict() {
        let run = test_run(RunStatus::Failed, None);
        let alert = RecoveryAlert::Failed;
        let escalate = Escalation::Verdict(verdict(json!({
            "verdict": "escalate", "confidence": "high",
            "diagnosis": "the acceptance cannot be met",
            "question": " Is task 1 still wanted? ",
        })));
        let note = escalate.note(&run, alert, 1);
        assert_eq!(note.why, "the recovery job could not repair it");
        assert_eq!(note.category, AskReason::RecoveryFailed);
        assert_eq!(
            note.text,
            "Why a person: recovery_failed\nDiagnosis: the acceptance cannot be met\nQuestion: Is task 1 still wanted?\nRecovery material: /runs/r1/recovery-failed-1.prompt.txt"
        );
        assert_eq!(
            escalate.requester(&run, alert, 1),
            Some(ActorContext::recovery_job(run.id(), "failed", 1))
        );
        let finished = escalate.finished(alert, 1, &note, Some(AskId::new(3)), json!({}));
        assert_eq!(finished["verdict"], "escalate");
        assert_eq!(finished["applied"], json!([]));
        assert!(finished.get("outcome").is_none(), "{finished}");

        let low = Escalation::Verdict(verdict(json!({
            "verdict": "repair", "confidence": "low", "diagnosis": "maybe the machine slept",
            "actions": [{"action": "wait", "recheck_after_secs": 0}],
        })));
        assert_eq!(
            low.note(&run, alert, 1).why,
            "the recovery job was not sure of its repair (confidence low), so it was not applied"
        );

        let refused = Escalation::Refused(
            verdict(json!({
                "verdict": "repair", "confidence": "high", "diagnosis": "flaky",
                "actions": [{"action": "retry"}],
                "options": ["retry anyway", " ", "retry anyway "],
                "reason_category": "discard",
            })),
            "its branch holds commits of its own".to_owned(),
        );
        let note = refused.note(&run, alert, 1);
        assert_eq!(
            note.why,
            "the runtime did not apply the recovery job's repair: its branch holds commits of its own"
        );
        assert_eq!(note.category, AskReason::Discard);
        assert_eq!(note.options, ["retry anyway"]);
        for part in [
            "Why a person: discard",
            "Diagnosis: flaky",
            "Recommended: [{\"action\":\"retry\"}]",
        ] {
            assert!(note.text.contains(part), "{part}: {}", note.text);
        }
        // Only `discard` and `scope` are the job's to give.
        let failed_reason = Escalation::Verdict(verdict(json!({
            "verdict": "escalate", "confidence": "high", "diagnosis": "x",
            "reason_category": "authentication",
        })));
        assert_eq!(
            failed_reason.note(&run, alert, 1).category,
            AskReason::RecoveryFailed
        );

        let used_up = Escalation::UsedUp(MAX_RECOVERY_ATTEMPTS);
        let note = used_up.note(&run, alert, 3);
        assert_eq!(
            note.why,
            "the recovery job ran 3 times for this alert already (at most 3)"
        );
        assert_eq!(note.text, "Why a person: recovery_failed");
        assert!(used_up.requester(&run, alert, 3).is_none());
    }

    #[test]
    fn each_live_alerts_ask_offers_its_own_options_then_the_jobs() {
        let run = test_run(RunStatus::Running, None);
        let failed = Escalation::JobFailed("no verdict".to_owned()).note(
            &run,
            RecoveryAlert::IdleProcess,
            1,
        );
        assert_eq!(
            ask_options(&STALLED_OPTIONS, Some(&failed)),
            ["wait", "stop"]
        );
        let jobs = Escalation::Verdict(verdict(json!({"verdict":"escalate", "confidence":"high", "diagnosis":"x", "options":["wait", "kill it", "kill it"]}))).note(&run, RecoveryAlert::IdleProcess, 1);
        assert_eq!(
            ask_options(&STALLED_OPTIONS, Some(&jobs)),
            ["wait", "stop", "kill it"]
        );
        assert_eq!(ask_options(&[], Some(&jobs)), ["wait", "kill it"]);
    }

    /// The end of a recovery job records the thread and model its provider
    /// names (Codex's, or why no model was read) and the provider it found
    /// unusable; a Claude job's end adds nothing (its session id is its
    /// start's).
    #[test]
    fn a_jobs_end_records_its_thread_model_and_unusable_provider() {
        let mut payload = json!({"alert": "failed"});
        JobEnd::default().record(&mut payload);
        assert_eq!(payload, json!({"alert": "failed"}));
        let end = JobEnd {
            session: Some(JobSession {
                named: true,
                session_id: Some("codex-thread-1".into()),
                model_unknown: Some("no rollout".into()),
                ..JobSession::default()
            }),
            unusable: Some((Provider::Codex, SwitchReason::Authentication)),
        };
        end.record(&mut payload);
        assert_eq!(
            payload,
            json!({
                "alert": "failed",
                "session_id": "codex-thread-1",
                "model": null,
                "model_unknown": "no rollout",
                "provider_unusable": {"provider": "codex", "reason": "authentication"},
            })
        );
    }

    /// A stopped job's `recovery_finished` records the Execution its agent
    /// was so far: the tokens its output gave, or not measured when it
    /// gave none (ADR-t1486-1).
    #[test]
    fn a_stopped_jobs_end_records_its_execution() {
        use crate::domain::tokens::{ExecutionTokens, TokenSource, TokenUsage};
        let stopped = |session: Option<JobSession>| {
            stopped_end(
                (RecoveryAlert::Stalled, Some("turn_without_receipt")),
                2,
                None,
                "session_ended",
                session.as_ref(),
            )
        };
        let measured = stopped(Some(JobSession {
            tokens: Some(ExecutionTokens {
                tokens: Some(TokenUsage {
                    input: 12,
                    output: 3,
                    ..TokenUsage::default()
                }),
                source: Some(TokenSource::ModelUsage),
                ..ExecutionTokens::default()
            }),
            ..JobSession::default()
        }));
        assert_eq!(measured["outcome"], "session_ended");
        assert_eq!(measured["attempt"], 2);
        assert_eq!(measured["reason"], "turn_without_receipt");
        assert_eq!(measured["tokens"]["input"], 12);
        assert_eq!(measured["tokens_source"], "model_usage");
        assert!(measured["tokens_reason"].is_null(), "{measured}");
        for session in [None, Some(JobSession::default())] {
            let unmeasured = stopped(session);
            assert!(unmeasured["tokens"].is_null(), "{unmeasured}");
            assert!(unmeasured.get("tokens_source").is_some(), "{unmeasured}");
            assert_eq!(unmeasured["tokens_reason"], "tokens_not_read");
        }
    }

    /// The processes are sampled every tenth of the threshold in whole
    /// seconds, between a second and a minute, and no less often than a
    /// threshold a test set below a second (task 1045).
    #[test]
    fn the_sample_interval_follows_the_threshold() {
        let secs = Duration::from_secs;
        assert_eq!(sample_interval(secs(30 * 60)), secs(60));
        assert_eq!(sample_interval(secs(100)), secs(10));
        assert_eq!(sample_interval(secs(2)), secs(1));
        assert_eq!(sample_interval(secs(1)), secs(1));
        assert_eq!(
            sample_interval(Duration::from_millis(300)),
            Duration::from_millis(300)
        );
    }
}
