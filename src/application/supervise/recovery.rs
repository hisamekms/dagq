//! The recovery job of a live session's alert (ADR-0047 decisions 39 and
//! 40): background work past `[stall].background_alert_secs`
//! (`long_background`), processes of the run that stay alive without
//! using CPU time (`idle_process`, task 469), a session that holds the
//! supervisor's `/exit` back after the runtime's own repairs
//! (`stuck_exit`), and a dialog the runtime does not answer by itself
//! (`prompt_waiting`); the `stalled` alert's session side is in
//! `stall_recovery.rs`. The supervisor records
//! `recovery_requested` and starts a headless job with the screen, the
//! run's processes and the worktree's state; the job only reads and prints
//! a verdict. The runtime checks every action's preconditions again, and
//! applies a `repair` of high confidence only when all of them hold,
//! recording `auto_repaired` (`layer: recovery`) for each and
//! `recovery_finished`. A verdict it does not apply, one of low confidence,
//! an escalation, a failed job and an alert past [`MAX_RECOVERY_ATTEMPTS`]
//! become the alert's ask to the inbox (`stalled`, `stuck_exit`,
//! `answer_prompt`), with the job's diagnosis, its actions as the
//! recommendation, its options added and its reason category. A run that
//! ended (`failed`, `interrupted`, `resume_exhausted`) is recovered in
//! `triage.rs`, with the same verdict.

use super::*;
use crate::domain::ActorContext;
use crate::domain::actor_model::{ActorLaunch, ModelRole};
use crate::domain::idle_process::{
    CpuWatch, IdleProcess, PROGRESS_CPU_PER_MILLE, without_session_helpers,
};
use crate::domain::language::with_instruction;
use crate::domain::recovery::{
    MAX_RECHECK_SECS, MAX_RECOVERY_ATTEMPTS, PROMPT_WAITING_ACTIONS, ProcessInfo, RecoveryAction,
    SEND_UNCONFIRMED, attempts, failed_live, run_processes,
};

/// The actions a recovery job may choose for a running session's
/// `long_background` alert.
pub(super) const LONG_BACKGROUND_ACTIONS: [&str; 3] =
    ["stop_processes", "send_instruction", "wait"];

/// The actions a recovery job may choose for an `idle_process` alert;
/// `send_instruction` holds only while the session is idle at its prompt.
pub(super) const IDLE_PROCESS_ACTIONS: [&str; 3] = ["stop_processes", "send_instruction", "wait"];

/// The setting the `idle_process` alert is judged by.
pub(super) const IDLE_PROCESS_THRESHOLD: &str = "idle_process_secs";

/// The actions for a `stuck_exit` of a run that does not land after its
/// exit: [`STUCK_EXIT_ACTIONS`] without `close_and_proceed`.
pub(super) const STUCK_EXIT_HELD_ACTIONS: [&str; 3] =
    ["answer_known_dialog", "stop_processes", "wait"];

/// How long a stopped process gets between SIGTERM and SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// What a recovery job printed: its verdict, or why there is none.
type JobOutcome = std::result::Result<RecoveryVerdict, String>;

/// The recovery job in progress for one alert of a live session.
pub(super) struct RecoveryJob {
    pub(super) alert: RecoveryAlert,
    /// Why the alert was raised, for an alert raised for more than one
    /// reason (`stalled`: [`IDLE_WITHOUT_RECEIPT`] or [`SEND_UNCONFIRMED`]).
    pub(super) reason: Option<&'static str>,
    pub(super) attempt: usize,
    /// The idle marker a `long_background` job was started for.
    marker: Option<SystemTime>,
    idle_secs: i64,
    job: HeadlessJob,
}

/// The recovery of one live session: the job in progress (one at a time;
/// another alert waits for it), which idle marker the last
/// `long_background` job was started for, so one marker starts one job (or
/// one more after a `wait`), and the `wait` of a `stuck_exit` or
/// `prompt_waiting` job.
#[derive(Default)]
pub(super) struct RecoveryWatch {
    job: Option<Box<RecoveryJob>>,
    seen: Option<SystemTime>,
    recheck: Option<SystemTime>,
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
    /// The supervisor typed the session's `/exit`: the condition of the
    /// "Background work is running" dialog (ADR-0047 decision 29).
    pub(super) exit_typed: bool,
    /// The session is idle at its prompt, for `send_instruction`.
    pub(super) at_prompt: bool,
    /// The run lands once its session exits, for `close_and_proceed`.
    pub(super) lands: bool,
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
    /// A known dialog was answered or processes were stopped: the session
    /// gets the exit timeout again.
    pub(super) exit_again: bool,
    /// The workspace was closed: the run goes on as after the session's
    /// exit.
    pub(super) closed: bool,
    /// `resume` was chosen, with its instruction: the watch parks the run
    /// as `needs_session` and asks its session to exit.
    pub(super) resume: Option<String>,
}

/// Where a `stuck_exit` or `prompt_waiting` alert stands after one look.
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

fn secs_between(from: SystemTime, to: SystemTime) -> i64 {
    to.duration_since(from)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// When the longest running of the marker's `tasks` was first listed in
/// the hook's `idle.log` next to the marker (the streak that ends with the
/// marker written `at`); the marker's own time when the log shows none
/// earlier or cannot be read.
fn background_since(
    sv: &Supervisor<'_>,
    marker_path: &Path,
    at: SystemTime,
    tasks: &[BackgroundTask],
) -> SystemTime {
    let at_ms = millis(at);
    let log = marker_path.with_file_name(crate::domain::stall::IDLE_LOG);
    let Ok(Some((_, bytes))) = sv.files.read_stamped(&log) else {
        return at;
    };
    let history = String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let (secs, marker) = line.split_once('\t')?;
            let secs: i64 = secs.trim().parse().ok()?;
            Some((
                secs.saturating_mul(1000),
                sv.signals.idle_hook(marker.as_bytes()).background_tasks,
            ))
        })
        .chain([(at_ms, tasks.to_vec())])
        .collect::<Vec<_>>();
    let seen = crate::domain::stall::background_first_seen(history);
    tasks
        .iter()
        .filter_map(|task| seen.get(&task.id).copied())
        .min()
        .filter(|&since| since < at_ms)
        .map_or(at, at_millis)
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

    /// Record [`Self::finished`] for a live session's alert.
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
        let payload = self.finished(alert, attempt, note, ask_id, extra);
        sv.queue
            .record_runtime_event(run.id(), event_kind::RECOVERY_FINISHED, payload)?;
        Ok(())
    }
}

/// The name of a recovery job's file in the run directory: its prompt
/// (`prompt.txt`), stdout (`out`) and stderr (`err`).
pub(super) fn job_file(alert: RecoveryAlert, attempt: usize, what: &str) -> String {
    format!("recovery-{}-{attempt}.{what}", alert.as_str())
}

/// The event an alert of a live session is raised from, the evidence of
/// its `recovery_requested`.
fn evidence_kind(alert: RecoveryAlert) -> Option<&'static str> {
    match alert {
        RecoveryAlert::StuckExit => Some(event_kind::EXIT_REQUEST_TIMED_OUT),
        RecoveryAlert::PromptWaiting => Some(event_kind::PROMPT_WAITING),
        _ => None,
    }
}

impl RecoveryWatch {
    /// Rebuild the watch of an adopted run from its events, so a marker
    /// already handed to a job is not handed again.
    pub(super) fn adopt(queue: &dyn Queue, run: &TaskRun) -> Result<Self> {
        let events = queue.run_events(run.id())?;
        let alert = |e: &&crate::domain::RunEvent| {
            e.payload["alert"] == RecoveryAlert::LongBackground.as_str()
        };
        let mut watch = Self::default();
        if let Some(event) = events
            .iter()
            .filter(alert)
            // A job the previous supervisor left running is gone: its
            // marker gets a new one (counted as another attempt).
            .rfind(|e| e.kind == event_kind::RECOVERY_FINISHED)
        {
            watch.seen = event.payload["marker_at_ms"].as_i64().map(at_millis);
            watch.recheck = event.payload["recheck_at_ms"].as_i64().map(at_millis);
        }
        // The `wait` a `stalled` job answered holds for the adopter too, so
        // it starts no job before the wait is over.
        for reason in IDLE_REASONS.into_iter().chain([SEND_UNCONFIRMED]) {
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
    /// `recovery_finished`.
    fn stop_running(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun, outcome: &str) {
        let mut job = self.job.take().expect("checked above");
        job.job.stop();
        let recorded = sv.queue.record_runtime_event(
            run.id(),
            event_kind::RECOVERY_FINISHED,
            json!({
                "alert": job.alert,
                "reason": job.reason,
                "attempt": job.attempt,
                "outcome": outcome,
                "marker_at_ms": job.marker.map(millis),
            }),
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

    pub(super) fn stop_job(&mut self) {
        if let Some(job) = &mut self.job {
            job.job.stop();
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
        // job (task 437): the alert is followed again once it is fixed.
        if sv.queue_hold.is_some() {
            return Ok(None);
        }
        let events = sv.queue.run_events(run.id())?;
        let done = attempts(&events, alert);
        if done >= MAX_RECOVERY_ATTEMPTS {
            return Ok(Some((done, Escalation::UsedUp(done))));
        }
        let attempt = done + 1;
        let evidence: Vec<EventId> = evidence_kind(alert)
            .and_then(|kind| events.iter().rev().find(|e| e.kind == kind))
            .map(|e| e.id)
            .into_iter()
            .collect();
        let mut payload = json!({
            "alert": alert,
            "attempt": attempt,
            "evidence": evidence,
            "workspace_id": live.workspace,
        });
        if let Some(reason) = reason {
            payload["reason"] = json!(reason);
        }
        // The facts may name the evidence themselves (the send a
        // `send_unconfirmed` alert was raised for).
        if let (Value::Object(payload), Value::Object(facts)) = (&mut payload, facts) {
            payload.extend(facts);
        }
        // What the job is started with (ADR-0079 decision 7): recorded,
        // not shown to the job among the facts.
        let launch = sv.actor_launch(ModelRole::Recovery);
        let mut recorded = payload.clone();
        recorded["launch"] = launch.to_value();
        sv.queue
            .record_runtime_event(run.id(), event_kind::RECOVERY_REQUESTED, recorded)?;
        info!(run_id = %run.id(), "run {}: alert {}; recovery job {attempt} starts", run.id(), alert.as_str());
        match spawn_live(sv, run, live, alert, attempt, &payload, &launch) {
            Ok(job) => {
                self.job = Some(Box::new(RecoveryJob {
                    alert,
                    reason,
                    attempt,
                    marker: marker.map(|(at, _)| at),
                    idle_secs: marker.map_or(0, |(_, secs)| secs),
                    job,
                }));
                Ok(None)
            }
            Err(error) => Ok(Some((
                attempt,
                Escalation::JobFailed(format!("the recovery job could not start: {error:#}")),
            ))),
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
        let Some(output) = job.job.poll(&*sv.files)? else {
            return Ok(None);
        };
        let job = self.job.take().expect("polled above");
        Ok(Some((
            job,
            output.and_then(|stdout| RecoveryVerdict::parse(&stdout)),
        )))
    }

    /// Follow a `stuck_exit` or `prompt_waiting` alert that holds now: act
    /// on its job once it ended, or start one when no job runs, no `wait`
    /// holds the alert and no failed job of it recorded by an earlier
    /// runtime waits for a person ([`failed_live`]). A job that failed is
    /// returned as [`LiveStep::Escalate`] with [`Escalation::JobFailed`]:
    /// the alert's ask opens (ADR-t609-1).
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
            let verdict = match verdict {
                Ok(verdict) => verdict,
                Err(error) => {
                    return Ok(LiveStep::Escalate(
                        job.attempt,
                        Escalation::JobFailed(error),
                    ));
                }
            };
            return Ok(match apply_live(sv, run, live, &job, verdict)? {
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
            return Ok(self.after_idle(step));
        }
        if self.job.is_some() {
            return Ok(LiveStep::Pending);
        }
        let threshold = sv.stall.idle_process_secs;
        let interval = sample_interval(threshold);
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
        let idle = self.cpu.idle(&own, at_ms, threshold);
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
/// tenth of its threshold, between one second and a minute.
fn sample_interval(threshold_secs: i64) -> Duration {
    Duration::from_secs(u64::try_from(threshold_secs / 10).unwrap_or(0).clamp(1, 60))
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
        sv.process_sample = Some((Instant::now(), sample));
    }
    sv.process_sample
        .as_ref()
        .and_then(|(_, sample)| sample.as_ref())
        .filter(|(at, _)| Some(*at) != last)
        .cloned()
}

/// An `idle_process` escalation of a phase that asks the inbox by itself
/// when it runs out (the wait on background work after the receipt ends at
/// the resume timeout, a held `/exit` becomes the `stuck_exit` alert):
/// the job's diagnosis is recorded (`outcome: left_to_phase`) for that
/// ask's recovery job to read, and no ask of its own is opened.
pub(super) fn leave_idle_to_phase(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    attempt: usize,
    escalation: &Escalation,
    phase: &str,
) -> Result<()> {
    let alert = RecoveryAlert::IdleProcess;
    let note = escalation.note(run, alert, attempt);
    warn!(run_id = %run.id(), "run {}: idle processes: {}; left to the {phase} phase's own timeout", run.id(), note.why);
    escalation.record(
        sv,
        run,
        alert,
        attempt,
        &note,
        None,
        json!({"outcome": "left_to_phase", "phase": phase}),
    )
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
/// the run directory, allowed to read only.
fn spawn_live(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    live: &Live<'_>,
    alert: RecoveryAlert,
    attempt: usize,
    facts: &Value,
    launch: &ActorLaunch,
) -> Result<HeadlessJob> {
    let task = sv.queue.show(run.task_id())?.task;
    // A headless session has no screen: its last turns stand in for it.
    let screen = if headless(run) {
        turns_excerpt(sv, run)
    } else {
        match sv.cmux.capture(live.workspace) {
            Ok(screen) => sv.signals.screen_excerpt(&screen),
            Err(error) => format!("(the screen could not be read: {error:#})"),
        }
    };
    let listed = own_processes(sv, run).map_err(|error| format!("{error:#}"));
    let (status, head, receipt) = git_facts(sv, run)?;
    let history = repair_history(sv, run)?;
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
    };
    let prompt = with_instruction(
        recovery_prompt(&task, run, attempt, &material)?,
        sv.verifier.language().as_ref(),
    );
    start_job(
        sv,
        run.id(),
        live.run_dir,
        alert,
        attempt,
        &prompt,
        None,
        launch,
    )
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
/// recorded one (ADR-0048 decision 4); `launch` the model and effort it
/// starts with (ADR-0079 decision 7).
#[allow(clippy::too_many_arguments)]
pub(super) fn start_job(
    sv: &mut Supervisor<'_>,
    run: &RunId,
    dir: &Path,
    alert: RecoveryAlert,
    attempt: usize,
    prompt: &str,
    session_id: Option<&str>,
    launch: &ActorLaunch,
) -> Result<HeadlessJob> {
    sv.files
        .create_dir_all(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    sv.files.write(
        &dir.join(job_file(alert, attempt, "prompt.txt")),
        prompt.as_bytes(),
    )?;
    let stdout = dir.join(job_file(alert, attempt, "out"));
    let stderr = dir.join(job_file(alert, attempt, "err"));
    let child = sv
        .actors()
        .spawn(ActorExecutionSpec::new(
            ActorContext::recovery_job(run, alert.as_str(), attempt),
            WorkspaceAccess::Scratch(dir.to_path_buf()),
            ActorProgram::Headless {
                program: HeadlessProgram::Job {
                    cwd: dir,
                    prompt,
                    allowed_tools: TRIAGE_TOOLS,
                },
                session_id,
                launch: Some(launch),
                without_mcp: false,
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
        if headless(run) && HEADLESS_NEVER.contains(&action.name()) {
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
            RecoveryAction::AnswerKnownDialog { dialog } => {
                known_dialog_ready(sv, run, live.workspace, live.exit_typed, dialog)
                    .map_err(|why| format!("answer_known_dialog: {why}"))?;
            }
            RecoveryAction::CloseAndProceed => {
                if !live.lands {
                    return Err(
                        "close_and_proceed: the run does not land after its session's exit"
                            .to_owned(),
                    );
                }
                if let Some(why) = landable_without_exit(sv, run) {
                    return Err(format!("close_and_proceed: {why}"));
                }
            }
            RecoveryAction::Resume { .. } => {
                if !live.park {
                    return Err(
                        "resume: only a run in its first session is parked for a session of its own"
                            .to_owned(),
                    );
                }
                if resumes_exhausted(&*sv.queue, run.id(), sv.resume_config) {
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
) -> Result<std::result::Result<Applied, Escalation>> {
    let actor = ActorContext::recovery_job(run.id(), job.alert.as_str(), job.attempt);
    sv.for_job(&actor, |sv| apply_live_verdict(sv, run, live, job, verdict))
}

/// [`apply_live`] with the job recorded as the requester.
fn apply_live_verdict(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    live: &Live<'_>,
    job: &RecoveryJob,
    verdict: RecoveryVerdict,
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
            .record_runtime_event(run.id(), event_kind::AUTO_REPAIRED, payload)
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
                applied.exit_again = true;
                info!(run_id = %run.id(), "run {}: recovery job {} stopped processes {pids:?} of its worktree", run.id(), job.attempt);
            }
            RecoveryAction::SendInstruction { instruction } => {
                let text = recovery_instruction(run, job.alert.as_str(), instruction);
                let sent_at = sv.files.now();
                let submission = match submit(
                    sv,
                    run,
                    live.workspace,
                    Input::Text(&text),
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
                applied.sent = Some((text, sent_at, submission));
            }
            RecoveryAction::AnswerKnownDialog { .. } => {
                let screen = match sv.cmux.capture(live.workspace) {
                    Ok(screen) => screen,
                    Err(error) => {
                        return Ok(Err(Escalation::Refused(
                            verdict.clone(),
                            format!("the screen could not be read: {error:#}"),
                        )));
                    }
                };
                if !answer_known_dialog(
                    sv,
                    run,
                    live.workspace,
                    &screen,
                    live.exit_typed,
                    Some((job.alert, job.attempt)),
                )? {
                    return Ok(Err(Escalation::Refused(
                        verdict.clone(),
                        "the known dialog's keys could not be sent".to_owned(),
                    )));
                }
                applied.exit_again = true;
            }
            RecoveryAction::CloseAndProceed => {
                if let Err(error) = sv.cmux.close(live.workspace) {
                    return Ok(Err(Escalation::Refused(
                        verdict.clone(),
                        format!("its workspace could not be closed: {error:#}"),
                    )));
                }
                repaired(
                    sv,
                    action,
                    json!({
                        "workspace_id": live.workspace,
                        "conditions": {"review": "pass", "clean": true, "receipt": "head", "reviewed": true},
                    }),
                )?;
                applied.closed = true;
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
    sv.queue.record_runtime_event(
        run.id(),
        event_kind::RECOVERY_FINISHED,
        json!({
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
        }),
    )?;
    info!(run_id = %run.id(), "run {}: recovery job {} of {} repaired it ({}): {}", run.id(), job.attempt, job.alert.as_str(), applied.names.join(", "), verdict.diagnosis);
    Ok(Ok(applied))
}

impl SessionWatch {
    /// One look at the session's background work (ADR-0047 decision 39):
    /// follow its recovery job in progress, or start one when the idle
    /// marker says background work has run past the threshold, no other
    /// alert's job runs and no `stalled` ask is open for the run.
    pub(super) fn watch_background(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<()> {
        // After the receipt the session's own wait on background work
        // takes over (up to the resume timeout): no job acts on it.
        if self.receipt_seen {
            self.recovery.stop_for(
                sv,
                run,
                Some(RecoveryAlert::LongBackground),
                "session_ended",
            );
            return Ok(());
        }
        if let Some((job, verdict)) =
            self.recovery
                .ended(sv, RecoveryAlert::LongBackground, None)?
        {
            return self.act_on_background(sv, run, &job, verdict);
        }
        if self.recovery.running()
            || sv.queue.has_unclosed_ask(run.id(), AskKind::Stalled)?
            || failed_live(
                &sv.queue.run_events(run.id())?,
                Some(RecoveryAlert::LongBackground),
            )
            .is_some()
        {
            return Ok(());
        }
        let Some(idle) = IdleMarker::read(&*sv.files, sv.signals, &self.idle_marker)? else {
            return Ok(());
        };
        if !idle.background_running() {
            return Ok(());
        }
        let now = sv.files.now();
        let marker = idle.modified();
        // Timed from when the running tasks were first listed, as `stats`
        // does (task 331), so turns the session keeps taking do not reset it.
        let since = background_since(sv, &self.idle_marker, marker, idle.background_tasks());
        let idle_secs = secs_between(since, now);
        if idle_secs <= sv.stall.background_alert_secs
            || self.recovery.recheck.is_some_and(|at| now < at)
            || (self.recovery.recheck.is_none()
                && self.recovery.seen.is_some_and(|seen| marker <= seen))
        {
            return Ok(());
        }
        // A login or usage limit that holds the queue starts no job (task
        // 437): the alert holds on and is raised once it is fixed.
        if sv.queue_hold.is_some() {
            return Ok(());
        }
        self.recovery.recheck = None;
        self.recovery.seen = Some(marker);
        let facts = json!({
            "idle_secs": idle_secs,
            "background_since_ms": millis(since),
            "threshold": BACKGROUND_THRESHOLD,
            "threshold_secs": sv.stall.background_alert_secs,
            "background_tasks": idle.background_tasks(),
            "marker_at_ms": millis(marker),
        });
        info!(run_id = %run.id(), "run {}: background work has run {idle_secs}s (over {}s)", run.id(), sv.stall.background_alert_secs);
        let live = Live {
            workspace: &self.workspace,
            run_dir: &self.run_dir,
            allowed: &LONG_BACKGROUND_ACTIONS,
            exit_typed: false,
            at_prompt: self.at_prompt(sv),
            lands: false,
            park: false,
        };
        let started = self.recovery.start(
            sv,
            run,
            &live,
            RecoveryAlert::LongBackground,
            None,
            facts,
            Some((marker, idle_secs)),
        )?;
        match started {
            None => Ok(()),
            Some((attempt, escalation)) => {
                self.escalate_background(sv, run, attempt, marker, idle_secs, escalation)
            }
        }
    }

    /// Act on the `long_background` job's verdict: apply a `repair` of high
    /// confidence whose every action holds now, and ask the inbox
    /// otherwise.
    fn act_on_background(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        job: &RecoveryJob,
        verdict: std::result::Result<RecoveryVerdict, String>,
    ) -> Result<()> {
        let marker = job.marker.unwrap_or(UNIX_EPOCH);
        let verdict = match verdict {
            Ok(verdict) => verdict,
            Err(error) => {
                return self.escalate_background(
                    sv,
                    run,
                    job.attempt,
                    marker,
                    job.idle_secs,
                    Escalation::JobFailed(error),
                );
            }
        };
        let (workspace, run_dir) = (self.workspace.clone(), self.run_dir.clone());
        let live = Live {
            workspace: &workspace,
            run_dir: &run_dir,
            allowed: &LONG_BACKGROUND_ACTIONS,
            exit_typed: false,
            at_prompt: self.at_prompt(sv),
            lands: false,
            park: false,
        };
        match apply_live(sv, run, &live, job, verdict)? {
            Ok(applied) => {
                if let Some((text, sent_at, submission)) = applied.sent {
                    self.stall.input_sent(sent_at, Some(&text));
                    self.answer_start = Some(StartCheck::new(
                        "recovery instruction",
                        &text,
                        sent_at,
                        &submission,
                    ));
                }
                self.recovery.recheck = applied_recheck(sv, job, run)?;
                Ok(())
            }
            Err(escalation) => {
                self.escalate_background(sv, run, job.attempt, marker, job.idle_secs, escalation)
            }
        }
    }

    /// Raise the `long_background` alert to the inbox as a `stalled` ask
    /// with the job's diagnosis, its recommended actions and why a person
    /// is needed, and hand the ask to the [`StallWatch`].
    fn escalate_background(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        attempt: usize,
        marker: SystemTime,
        idle_secs: i64,
        escalation: Escalation,
    ) -> Result<()> {
        let alert = RecoveryAlert::LongBackground;
        let note = escalation.note(run, alert, attempt);
        let extra = json!({"marker_at_ms": millis(marker)});
        // A `stalled` ask opened meanwhile (the idle detection's) already
        // has a person looking: the diagnosis is recorded, not merged into
        // it.
        if sv.queue.has_unclosed_ask(run.id(), AskKind::Stalled)? {
            escalation.record(sv, run, alert, attempt, &note, None, extra)?;
            warn!(run_id = %run.id(), "run {}: {}; a stalled ask is already open, so no other is asked", run.id(), note.why);
            return Ok(());
        }
        let question = format!(
            "The session of run {run_id} (task {task_id}) in workspace {workspace} has had background work running for {idle_secs}s (alert: long_background, over {threshold}s), and {why}.\n{text}\nAnswer `wait` to leave the session alone, or `intervene` to step in yourself (read the screen, stop its background work, type an instruction; see the dagq-recover skill). This ask closes itself once the session moves on.",
            run_id = run.id(),
            task_id = run.task_id(),
            workspace = self.workspace,
            threshold = sv.stall.background_alert_secs,
            why = note.why,
            text = note.text,
        );
        let mut options: Vec<String> = STALLED_OPTIONS.iter().map(|o| (*o).to_owned()).collect();
        options.extend(
            note.options
                .iter()
                .filter(|o| !options.contains(o))
                .cloned()
                .collect::<Vec<_>>(),
        );
        let outcome = ask::ask(
            &mut *sv.queue,
            &sv.layout.main_checkout,
            NewAsk {
                kind: AskKind::Stalled,
                task_id: Some(run.task_id()),
                run_id: Some(run.id().clone()),
                question,
                options,
                asked_by: SessionRole::Supervisor.as_str().into(),
                reason_category: note.category,
                finding_id: None,
            },
            sv.cmux,
        )?;
        let id = AskId::new(outcome["id"].as_i64().context("ask returned no id")?);
        let now = sv.files.now();
        self.stall
            .escalated(id, now, idle_secs, BACKGROUND_THRESHOLD);
        escalation.record(sv, run, alert, attempt, &note, Some(id), extra)?;
        warn!(ask_id = %id, run_id = %run.id(), "run {}: {}; stalled ask {id} (notified: {})", run.id(), note.why, outcome["notified"]);
        Ok(())
    }

    /// One look at the session's processes before its `/exit` (task 469):
    /// the `idle_process` alert, before and after the receipt. Before the
    /// receipt an escalation is the `stalled` ask, as `long_background`'s;
    /// after it the wait on background work ends at the resume timeout by
    /// itself, so it is left to that ([`leave_idle_to_phase`]). No job
    /// starts while a `stalled` ask of the run is open.
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
            exit_typed: false,
            at_prompt: self.at_prompt(sv),
            lands: false,
            park: false,
        };
        match self.recovery.watch_idle(sv, run, &live, phase)? {
            LiveStep::Pending => Ok(()),
            LiveStep::Repaired(applied) => {
                if let Some((text, sent_at, submission)) = applied.sent {
                    self.stall.input_sent(sent_at, Some(&text));
                    self.answer_start = Some(StartCheck::new(
                        "recovery instruction",
                        &text,
                        sent_at,
                        &submission,
                    ));
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
        let question = format!(
            "The session of run {run_id} (task {task_id}) in workspace {workspace} has processes that have used almost no CPU time for over {threshold}s (alert: idle_process): {listed}. And {why}.\n{text}\nAnswer `wait` to leave the session alone, or `intervene` to step in yourself (read the screen, stop the processes, type an instruction; see the dagq-recover skill). This ask closes itself once the session moves on.",
            run_id = run.id(),
            task_id = run.task_id(),
            workspace = self.workspace,
            threshold = sv.stall.idle_process_secs,
            why = note.why,
            text = note.text,
        );
        let mut options: Vec<String> = STALLED_OPTIONS.iter().map(|o| (*o).to_owned()).collect();
        for option in &note.options {
            if !options.contains(option) {
                options.push(option.clone());
            }
        }
        let outcome = ask::ask(
            &mut *sv.queue,
            &sv.layout.main_checkout,
            NewAsk {
                kind: AskKind::Stalled,
                task_id: Some(run.task_id()),
                run_id: Some(run.id().clone()),
                question,
                options,
                asked_by: SessionRole::Supervisor.as_str().into(),
                reason_category: note.category,
                finding_id: None,
            },
            sv.cmux,
        )?;
        let id = AskId::new(outcome["id"].as_i64().context("ask returned no id")?);
        let now = sv.files.now();
        self.stall
            .escalated(id, now, idle_secs, IDLE_PROCESS_THRESHOLD);
        escalation.record(sv, run, alert, attempt, &note, Some(id), json!({}))?;
        warn!(ask_id = %id, run_id = %run.id(), "run {}: {}; stalled ask {id} (notified: {})", run.id(), note.why, outcome["notified"]);
        Ok(())
    }

    /// The session holds its `/exit` back past the exit timeout: its
    /// recovery job (`stuck_exit`), and the `stuck_exit` ask once it
    /// escalates. A repair that answered a dialog or stopped processes
    /// gives the session the exit timeout again.
    pub(super) fn recover_stuck_exit(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<()> {
        // A `long_background` or `idle_process` job is only followed
        // before the `/exit`: left running, it would hold this alert's job
        // back for good.
        for alert in [RecoveryAlert::LongBackground, RecoveryAlert::IdleProcess] {
            self.recovery
                .stop_for(sv, run, Some(alert), "exit_requested");
        }
        let live = Live {
            workspace: &self.workspace,
            run_dir: &self.run_dir,
            allowed: &STUCK_EXIT_HELD_ACTIONS,
            exit_typed: self.exit_requested.is_some(),
            at_prompt: false,
            lands: false,
            park: false,
        };
        let timeout = sv.cmux.exit_timeout().as_secs();
        let step = self.recovery.follow(sv, run, &live, RecoveryAlert::StuckExit, || {
            json!({"timeout_secs": timeout, "exit_typed": true, "silent": self.exit_for_silence})
        })?;
        match step {
            LiveStep::Pending => {}
            LiveStep::Repaired(applied) => {
                if applied.exit_again {
                    self.exit_requested = Some(Instant::now());
                    self.exit_timed_out = false;
                }
            }
            LiveStep::Escalate(attempt, escalation) => {
                let note = escalation.note(run, RecoveryAlert::StuckExit, attempt);
                let after = stuck_exit_after(
                    self.exit_for_silence,
                    "The run stays running, and goes on to validating once the session exits",
                );
                let workspace = self.workspace.clone();
                let id = ask_stuck_exit(sv, run, &workspace, &after, Some(&note))?;
                escalation.record(
                    sv,
                    run,
                    RecoveryAlert::StuckExit,
                    attempt,
                    &note,
                    Some(id),
                    json!({}),
                )?;
                self.exit_asked = true;
            }
        }
        Ok(())
    }

    /// The session waits at a dialog the runtime does not answer: its
    /// recovery job (`prompt_waiting`), and the `answer_prompt` ask once it
    /// escalates. `kind` and `excerpt` are the dialog on `screen` now.
    pub(super) fn recover_prompt(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        kind: &str,
        excerpt: &str,
    ) -> Result<()> {
        if sv.queue.has_unclosed_ask(run.id(), AskKind::AnswerPrompt)? {
            return Ok(());
        }
        let live = Live {
            workspace: &self.workspace,
            run_dir: &self.run_dir,
            allowed: &PROMPT_WAITING_ACTIONS,
            exit_typed: self.exit_requested.is_some(),
            at_prompt: false,
            lands: false,
            park: false,
        };
        let hash = self.prompt_hash.clone();
        let step = self.recovery.follow(
            sv,
            run,
            &live,
            RecoveryAlert::PromptWaiting,
            || json!({"prompt": kind, "screen_hash": hash, "excerpt": excerpt}),
        )?;
        match step {
            LiveStep::Pending | LiveStep::Repaired(_) => {}
            LiveStep::Escalate(attempt, escalation) => {
                let note = escalation.note(run, RecoveryAlert::PromptWaiting, attempt);
                let workspace = self.workspace.clone();
                let id = ask_answer_prompt(sv, run, &workspace, kind, excerpt, Some(&note))?;
                escalation.record(
                    sv,
                    run,
                    RecoveryAlert::PromptWaiting,
                    attempt,
                    &note,
                    Some(id),
                    json!({}),
                )?;
            }
        }
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
