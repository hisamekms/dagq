//! The throughput review job (ADR-t996-1): the supervisor starts
//! `throughput-review` on its timer for the last whole hour, yesterday and
//! the ISO week before this one. For an hour the runtime counts the
//! landings and judges them by rules
//! ([`crate::domain::throughput_review::judge_hourly`]); every hour is
//! reviewed whatever the rules found (ADR-t1172-1), and the rules it met
//! (`reasons`) go into the inputs and the events, head its review, and
//! make its report a notice to the inbox. A headless agent (`DAGQ_ROLE=throughput-review-job`, which the
//! queue lets read only, without MCP), on the provider of its launch (Claude
//! by default, Codex when `[roles.throughput_review]` names it, task 1220),
//! reads the landings, `kpi`, `stats`,
//! the claim deferrals, the asks and the timelines of the longest runs, and
//! follows the weekly review of the dagq skill's `reference/kpi.md` for its
//! cadence. The command, not the agent, saves the review under
//! `<queue dir>/reports/reviews/`, records the weekly next move as a
//! finding marked for a proposal, and records `throughput_review_reported`,
//! the inbox's notice. A failed job leaves its log and a failed
//! `throughput_review_finished`, itself a notice to the inbox (task 1099),
//! but for a Codex job that found Codex unusable: its finish says so
//! (`provider_unusable`), is no notice, and the supervisor reviews its
//! period again on the other provider.
//! The prompt carries a summary of the inputs within [`PROMPT_LIMIT`]
//! ([`prompt_input`]); the whole is the job directory's `input.json`.
//! The reads the composition root assembles come through
//! [`ThroughputReviewSources`], the files and the agent's process through
//! [`ThroughputReviewHost`]; [`crate::compose::throughput_review`] opens
//! the queue and runs [`review`].
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};

use super::prompt::PromptBytes;
use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::{
    application::{
        AgentProvider, AgentSignals, AskQuery, EventReads, Generators, Queue,
        observer::HeadlessAgent,
    },
    domain::{
        ActorContext, EventFilter, EventId, EventKind, FindingTarget, NewFinding, Provider,
        RunEvent, RunId,
        actor_model::ActorLaunch,
        event_kind::{ASK_OPENED, RUN_INTEGRATED},
        headless_job::JobAccess,
        kpi::{DAY_MS, KpiQuery, Period},
        language::Language,
        stats::{Cursor, StatsQuery, timestamp_millis},
        throughput_review::{
            HOUR_MS, HourlyJudgment, LOOKBACK_HOURS, NEXT_MOVE_FENCE, ReviewMode, ReviewOutput,
            Window, bucket_counts, judge_hourly, parse_output, window,
        },
        transcript::millis_text,
    },
};

/// How long one review may take before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// What the job may do: run the queue CLI only (ADR-t1063-1 decision 2).
pub const ACCESS: JobAccess = JobAccess::QueueCli;
/// The runs landed in the period whose timelines the input carries, the
/// longest first.
pub const TIMELINES: usize = 3;
/// The shortest gap the timelines report, in seconds.
const TIMELINE_GAP_SECS: i64 = 300;
/// The landing events the input lists, the newest kept.
const LANDING_EVENTS: usize = 200;
/// The finding kind of the weekly next move.
pub const FINDING_KIND: &str = "throughput";
/// The most the prompt may hold, in bytes (task 1099). The job took the
/// prompt as an argument then, which with the environment may not pass
/// the host's `ARG_MAX` (1 MiB on macOS); it reads it from stdin now (task
/// 1560), and the limit keeps the agent's context small, as the whole
/// inputs of a day or a week are MBs.
pub const PROMPT_LIMIT: usize = 128 * 1024;
/// The most the inputs in the prompt ([`prompt_input`], pretty JSON) may
/// hold, in bytes: the rest of [`PROMPT_LIMIT`] holds the instructions,
/// the procedure and the language line.
pub const PROMPT_INPUT_LIMIT: usize = 96 * 1024;
/// The parts of `stats` the prompt carries: the whole of the period, its
/// serial landing slot, the waits and holds, and the failures. The rest
/// (the runs, the builds, the goals, …) is for the job to read with
/// `stats --full`.
const PROMPT_STATS: &[&str] = &[
    "overall",
    "landing_utilization",
    "waiting",
    "claim_deferrals",
    "claim_holds",
    "landing_holds",
    "escalations",
    "backend_failures",
    "verification_failures",
    "provider_switches",
];
/// What [`prompt_input`] leaves out, in this order, while the inputs pass
/// [`PROMPT_INPUT_LIMIT`]: the largest and the easiest to read again
/// first.
const DROP_ORDER: &[&[&str]] = &[
    &["stats"],
    &["kpi", "latest"],
    &["timelines"],
    &["kpi", "targets"],
    &["kpi", "periods"],
    &["asks"],
    &["health"],
    &["claim_deferred"],
    &["landings"],
    // A `kpi` of a shape not known passes whole: it goes last.
    &["kpi"],
];

/// The dagq skill's KPI reference, whose weekly review the job follows: the
/// prompt carries the procedure from there, not a copy of its own.
const KPI_REFERENCE: &str = include_str!("../../plugins/claude-dagq/skills/dagq/reference/kpi.md");
/// The heading of the weekly review in [`KPI_REFERENCE`].
const PROCEDURE_HEADING: &str = "## Raising throughput: the weekly review";

#[derive(Debug, Clone)]
pub struct ReviewOptions {
    pub mode: ReviewMode,
    /// The unix second the period is the latest finished one at; now
    /// without (the supervisor passes the second it found the review due).
    pub at: Option<i64>,
    /// Build and return the prompt without starting the agent.
    pub dry_run: bool,
    pub timeout: Duration,
    /// The `dagq` binary the agent calls; its directory goes first on PATH.
    pub dagq: PathBuf,
    /// The user's `config.toml` the language comes from under the bound
    /// checkout's `dagq.toml` (ADR-t616-2); `None` reads none.
    pub user_config: Option<PathBuf>,
    /// The time zone the hours, days and weeks begin in, seconds east of
    /// UTC; the host's without (the supervisor passes the one it found the
    /// review due in).
    pub utc_offset: Option<i64>,
    /// What the job starts with: the provider the supervisor routed it to
    /// (ADR-t1063-1 decisions 1 and 4, task 1220), the provider of
    /// `provider`; `None` reads `[roles.throughput_review]` of the bound
    /// checkout's `dagq.toml` ([`ThroughputReviewHost::launch`]).
    pub launch: Option<ActorLaunch>,
    /// Whether `[roles.throughput_review]` names its provider, so that a
    /// Codex job that stopped at a wall (a login, the usage limit, a launch
    /// that failed) records `provider_unusable` and is started again on the
    /// other provider (ADR-t1063-1 decision 4).
    pub switchable: bool,
    /// `[provider_fallback] jobs` (ADR-t1857-1): with it off (`false`) a
    /// Claude job of a role that names its provider that stopped where
    /// Claude cannot be used (its start, a login, the usage limit) records
    /// `provider_unusable` too, and the supervisor holds Claude and starts
    /// the period again on Claude once Claude's hold ends.
    pub fallback: bool,
    /// Why no provider can run the job (`--no-claude` with Codex not
    /// usable, ADR-t1204-1): a period that needs a review records its
    /// failure with this reason instead of starting the agent.
    pub unavailable: Option<String>,
}

/// `<queue dir>/reports/reviews`: one directory per review.
pub fn reviews_dir(db: &Path) -> PathBuf {
    db.parent()
        .unwrap_or(Path::new("."))
        .join("reports")
        .join("reviews")
}

/// The weekly review of the dagq skill (`reference/kpi.md`), up to the next
/// section.
pub fn procedure() -> &'static str {
    let Some(start) = KPI_REFERENCE.find(PROCEDURE_HEADING) else {
        return "";
    };
    let rest = &KPI_REFERENCE[start..];
    let end = rest[PROCEDURE_HEADING.len()..]
        .find("\n## ")
        .map_or(rest.len(), |end| end + PROCEDURE_HEADING.len());
    rest[..end].trim()
}

/// The reads of the queue the review's input takes that the composition
/// root assembles (`stats`, the KPIs and the bound checkout), and the
/// finding the weekly next move records.
pub trait ThroughputReviewSources<Q: ?Sized> {
    fn stats(&self, queue: &Q, db: &Path, query: &StatsQuery) -> Result<Value>;
    fn kpi(&self, queue: &Q, db: &Path, query: &KpiQuery) -> Result<Value>;
    /// The checkout the queue is bound to, if any.
    fn checkout(&self, queue: &Q) -> Result<Option<PathBuf>>;
    /// Record `finding`: its id.
    fn record_finding(&self, queue: &mut Q, finding: NewFinding) -> Result<i64>;
}

/// The host the review runs on: its time zone and process, its files
/// under `<queue dir>/reports/reviews`, the configuration its agent starts
/// with and the agent's process.
pub trait ThroughputReviewHost {
    /// The host's time zone at `now` (unix seconds), seconds east of UTC.
    fn utc_offset(&self, now: i64) -> i64;
    /// This process's id and its parent's: who to reap.
    fn pids(&self) -> (u32, u32);
    /// A new directory for the review of `period` ([`reviews_dir`]).
    fn review_dir(&self, db: &Path, period: &Window) -> Result<PathBuf>;
    fn write(&self, path: &Path, contents: &str) -> Result<()>;
    fn read(&self, path: &Path) -> Result<String>;
    /// What the job's agent starts with (ADR-0079 decision 7).
    fn launch(&self, checkout: Option<&Path>) -> ActorLaunch;
    /// The language of the prompt (ADR-t616-2).
    fn language(&self, checkout: Option<&Path>, user_config: Option<&Path>) -> Option<Language>;
    /// Start the agent in `dir` and wait for it up to its timeout: the
    /// exit code, or `None` when a signal ended it.
    fn run(
        &self,
        provider: &dyn AgentProvider,
        db: &Path,
        dir: &Path,
        prompt: &str,
        agent: &HeadlessAgent<'_>,
    ) -> Result<Option<i32>>;
}

/// What [`review`] reaches outside the queue through.
pub struct ThroughputReviewEnvironment<'a, Q: ?Sized> {
    pub sources: &'a dyn ThroughputReviewSources<Q>,
    pub host: &'a dyn ThroughputReviewHost,
    pub generators: &'a Generators,
    /// Claude Code's reading of a job's output, for the walls of a Claude
    /// job (task 438); `None` reads none.
    pub signals: Option<&'a dyn AgentSignals>,
}

/// Run one review: judge the hour (hourly), gather the inputs, start the
/// agent headless on `provider` (the provider of the launch), save what it
/// printed and tell the inbox. `db` is the queue's canonical path, which
/// names its directory.
pub fn review<Q: Queue + EventReads>(
    queue: &mut Q,
    db: &Path,
    provider: &dyn AgentProvider,
    options: &ReviewOptions,
    environment: &ThroughputReviewEnvironment<'_, Q>,
) -> Result<Value> {
    let host = environment.host;
    let now = options
        .at
        .unwrap_or_else(|| environment.generators.clock.now());
    let offset_ms = options.utc_offset.unwrap_or_else(|| host.utc_offset(now)) * 1000;
    let period = window(options.mode, now * 1000, offset_ms);
    let (pid, parent_pid) = host.pids();
    let mut failure = json!({
        "mode": options.mode.as_str(),
        "period": period.label,
        "outcome": "error",
        "exit_code": null,
        "dir": null,
        "pid": pid,
        "parent_pid": parent_pid,
    });
    let result = review_period(
        queue,
        db,
        provider,
        options,
        environment,
        &period,
        &mut failure,
    );
    if options.dry_run {
        return result;
    }
    let payload = result.unwrap_or_else(|error| {
        failure["error"] = json!(format!("{error:#}"));
        failure
    });
    // The only finish write: preparation errors and agent outcomes share
    // this path. A failed finish write propagates without trying it again.
    queue.record_queue_event(EventKind::ThroughputReviewFinished, payload.clone())?;
    tracing::info!(
        mode = options.mode.as_str(),
        period = period.label,
        outcome = payload["outcome"].as_str(),
        "throughput review ({}) finished: {}",
        options.mode.as_str(),
        payload["outcome"]
    );
    Ok(payload)
}

/// Prepare and execute a known period; retain context for preparation errors.
fn review_period<Q: Queue + EventReads>(
    queue: &mut Q,
    db: &Path,
    provider: &dyn AgentProvider,
    options: &ReviewOptions,
    environment: &ThroughputReviewEnvironment<'_, Q>,
    period: &Window,
    failure: &mut Value,
) -> Result<Value> {
    let ThroughputReviewEnvironment {
        sources,
        host,
        generators,
        signals,
    } = *environment;
    let landings = landings(queue, period)?;
    let judgment = match options.mode {
        ReviewMode::Hourly => Some(judge_hourly(&bucket_counts(
            &landings.iter().map(|(at, _)| *at).collect::<Vec<_>>(),
            period.end_ms,
            HOUR_MS,
            LOOKBACK_HOURS + 1,
        ))?),
        _ => None,
    };
    failure["reasons"] = json!(judgment.as_ref().map(|judged| &judged.reasons));
    // No provider can run it (`--no-claude`, Codex not usable): the period
    // fails with why, for a person (ADR-t1204-1).
    if !options.dry_run
        && let Some(why) = &options.unavailable
    {
        anyhow::bail!("the throughput review could not start: {why}");
    }
    let input = gather(
        queue,
        db,
        sources,
        generators,
        period,
        &landings,
        judgment.as_ref(),
    )?;
    // The job's `dagq` goes to the queue service in client mode: the
    // prompt names no queue path (goal 82's stage (3)).
    let command = "dagq";
    let checkout = sources.checkout(queue)?;
    let language = host.language(checkout.as_deref(), options.user_config.as_deref());
    // A dry run makes no directory: its prompt names where one would be.
    let dir = if options.dry_run {
        reviews_dir(db).join(format!("{}-{}", period.mode.as_str(), period.label))
    } else {
        host.review_dir(db, period)?
    };
    failure["dir"] = json!(dir);
    let ReviewPrompt {
        text: prompt,
        record: prompt_record,
    } = job_prompt(
        period,
        command,
        &input,
        &dir.join("input.json"),
        language.as_ref(),
    )?;
    // What the prompt took (ADR-t1566-1 decision 6), on the start and on
    // the finish, whichever way the review ends.
    merge(failure, &prompt_record);
    if options.dry_run {
        let mut payload = json!({
            "dry_run": true,
            "mode": options.mode.as_str(),
            "period": period.label,
            "hourly": judgment,
            "prompt": prompt,
        });
        merge(&mut payload, &prompt_record);
        return Ok(payload);
    }
    host.write(&dir.join("prompt.md"), &prompt)?;
    host.write(
        &dir.join("input.json"),
        &serde_json::to_string_pretty(&input)?,
    )?;
    let launch = options
        .launch
        .clone()
        .unwrap_or_else(|| host.launch(checkout.as_deref()));
    // The job's Claude session id (ADR-0048 decision 4); Codex names its
    // thread itself, which the finish records (ADR-t1063-1 decision 6).
    let session_id = (launch.provider == Provider::Claude).then(|| generators.ids.uuid());
    failure["session_id"] = json!(session_id);
    let reasons = judgment.as_ref().map(|judged| judged.reasons.clone());
    let mut started = json!({
        "mode": options.mode.as_str(),
        "period": period.label,
        "reasons": reasons,
        "dir": dir,
        "session_id": session_id,
        "launch": launch.to_value(),
    });
    merge(&mut started, &prompt_record);
    let started = queue.record_queue_event(EventKind::ThroughputReviewStarted, started)?;
    tracing::info!(
        mode = options.mode.as_str(),
        period = period.label,
        "throughput review ({}) started with a prompt of {} bytes",
        options.mode.as_str(),
        prompt_record["prompt_bytes"]["total"]
    );
    let clock = Instant::now();
    let started_ms = generators
        .clock
        .system_time()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok());
    let ran = host.run(
        provider,
        db,
        &dir,
        &prompt,
        &HeadlessAgent {
            actor: ActorContext::throughput_review_job(options.mode.as_str(), &period.label),
            session_id: session_id.as_deref(),
            launch: &launch,
            dagq: &options.dagq,
            timeout: options.timeout,
            what: "the throughput review",
            access: ACCESS,
        },
    );
    // An agent that did not start at all (its executable gone, say) left
    // no output to read why: its start's error says.
    let mut start_failure = None;
    let (outcome, exit_code, error) = match ran {
        Ok(Some(0)) => ("succeeded", Some(0), None),
        Ok(code) => ("failed", code, None),
        Err(error) => {
            if crate::application::observer::not_started(&error, "the throughput review") {
                start_failure = Some(crate::application::job_start_failure(&error));
            }
            ("error", None, Some(format!("{error:#}")))
        }
    };
    let (pid, parent_pid) = host.pids();
    let mut payload = json!({
        "mode": options.mode.as_str(),
        "period": period.label,
        "outcome": outcome,
        "exit_code": exit_code,
        "error": error,
        "reasons": reasons,
        "dir": dir,
        // The span this review's start opened closes by it.
        "session_id": session_id,
        "duration_secs": clock.elapsed().as_secs(),
        // Who to reap: a supervisor that exec'd while this ran is still its
        // parent and waits on it by these.
        "pid": pid,
        "parent_pid": parent_pid,
    });
    merge(&mut payload, &prompt_record);
    let stdout = host.read(&dir.join("output.out")).unwrap_or_default();
    // Codex's thread and the model of its rollout (ADR-t1063-1 decision 6):
    // the id that closes its span and the model `stats` reads.
    if let Some(session) = provider.job_session(&stdout, started_ms) {
        session.record(&mut payload);
    }
    // A Codex job that stopped where Codex cannot be used starts again on
    // the other provider (ADR-t1063-1 decision 4): the supervisor holds
    // Codex and starts the period again. Under `--no-claude` it finds no
    // provider and records why, so Claude is never started. With
    // `[provider_fallback] jobs` off it starts again on the same provider
    // once its hold ends, a Claude one too (ADR-t1857-1).
    if outcome != "succeeded" {
        let stderr = host.read(&dir.join("output.err")).unwrap_or_default();
        let (_, unusable) = crate::application::observer::read_failure(
            false,
            launch.provider,
            (options.switchable, options.fallback),
            start_failure,
            || start_failure.unwrap_or_else(|| provider.job_failure(&stdout, &stderr)),
            || signals.map(|signals| signals.job_failure(&format!("{stdout}\n{stderr}"))),
        );
        if let Some(reason) = unusable {
            payload["provider_unusable"] =
                json!({"provider": launch.provider.as_str(), "reason": reason.as_str()});
        }
    }
    if outcome == "succeeded" {
        // A review that could not be saved or reported is a failed one:
        // the event says why, and the loop goes on.
        match report(
            queue,
            provider,
            environment,
            &dir,
            period,
            reasons.as_deref(),
            started,
        ) {
            Ok(reported) => {
                payload["reported_event_id"] = json!(reported.event_id);
                payload["finding_id"] = json!(reported.finding_id);
                payload["path"] = json!(reported.path);
            }
            Err(failure) => {
                payload["outcome"] = json!("error");
                payload["error"] = json!(format!("{failure:#}"));
            }
        }
    }
    Ok(payload)
}

/// What [`report`] recorded.
struct Reported {
    event_id: EventId,
    finding_id: Option<i64>,
    path: PathBuf,
}

/// Save the review the agent printed (`review.md`, the whole text, and
/// `review.json`, the conclusion and the next move), record the weekly
/// next move as a finding marked for a proposal, and record
/// `throughput_review_reported` for the inbox.
fn report<Q: Queue>(
    queue: &mut Q,
    provider: &dyn AgentProvider,
    environment: &ThroughputReviewEnvironment<'_, Q>,
    dir: &Path,
    period: &Window,
    reasons: Option<&[crate::domain::throughput_review::HourlyReason]>,
    started: EventId,
) -> Result<Reported> {
    let host = environment.host;
    let output = host
        .read(&dir.join("output.out"))
        .context("read the review's output")?;
    // The reply its provider reads out of the output (ADR-t1063-1 decision
    // 2), whatever the provider.
    let output = provider.job_reply(&output);
    let ReviewOutput {
        conclusion,
        text,
        next_move,
        next_move_error,
    } = parse_output(&output);
    let path = dir.join("review.md");
    host.write(&path, &format!("{text}\n"))?;
    // Only the weekly review proposes a change (ADR-t996-1 decision 4).
    let next_move = next_move.filter(|_| period.mode == ReviewMode::Weekly);
    let finding_id = match &next_move {
        Some(next) => {
            let detail = match &next.detail {
                Some(detail) => format!("{detail}\n\nWhy: {}", next.why),
                None => format!("Why: {}", next.why),
            };
            Some(environment.sources.record_finding(
                queue,
                NewFinding {
                    kind: FINDING_KIND.to_owned(),
                    target: FindingTarget::Queue,
                    subject: format!("weekly/{}", period.label),
                    summary: next.summary.clone(),
                    detail: Some(detail),
                    impact: None,
                    evidence: vec![started],
                    propose: Some(next.why.clone()),
                    by: crate::domain::ActorRole::Supervisor.as_str().to_owned(),
                },
            )?)
        }
        None => None,
    };
    host.write(
        &dir.join("review.json"),
        &serde_json::to_string_pretty(&json!({
            "mode": period.mode.as_str(),
            "period": period.label,
            "reasons": reasons,
            "conclusion": conclusion,
            "next_move": next_move,
            "next_move_error": next_move_error,
            "finding_id": finding_id,
        }))?,
    )?;
    let event_id = queue.record_queue_event(
        EventKind::ThroughputReviewReported,
        json!({
            "mode": period.mode.as_str(),
            "period": period.label,
            "reasons": reasons,
            "conclusion": conclusion,
            "path": path,
            "dir": dir,
            "finding_id": finding_id,
            "next_move_error": next_move_error,
        }),
    )?;
    Ok(Reported {
        event_id,
        finding_id,
        path,
    })
}

/// The landings (`run_integrated`) from the start of what the review of
/// `period` compares with to its end: the hours the rules read, or the
/// period before as long as this one. Each with its unix milliseconds.
fn landings(queue: &(impl Queue + EventReads), period: &Window) -> Result<Vec<(i64, RunEvent)>> {
    let from = match period.mode {
        ReviewMode::Hourly => {
            period.end_ms - HOUR_MS * i64::try_from(LOOKBACK_HOURS + 1).unwrap_or(i64::MAX)
        }
        _ => period.start_ms - (period.end_ms - period.start_ms),
    };
    let events = queue.events_between(
        EventId::new(0),
        queue.latest_event_id()?,
        &EventFilter {
            kinds: Some(vec![RUN_INTEGRATED.to_owned()]),
            since: Some(millis_text(from)),
            until: Some(millis_text(period.end_ms)),
            ..EventFilter::default()
        },
        usize::MAX >> 1,
    )?;
    Ok(events
        .into_iter()
        .filter_map(|event| timestamp_millis(&event.created_at).map(|at| (at, event)))
        .collect())
}

/// The review's input: the landings, the rules' judgment, the workers'
/// health and the disk's free space, `kpi`, `stats`, the claim deferrals,
/// the asks and the timelines of the longest runs that landed. A part that does not read is its `{"error": ...}`.
fn gather<Q: Queue + EventReads>(
    queue: &Q,
    db: &Path,
    sources: &dyn ThroughputReviewSources<Q>,
    generators: &Generators,
    period: &Window,
    landings: &[(i64, RunEvent)],
    judgment: Option<&HourlyJudgment>,
) -> Result<Value> {
    let at: Vec<i64> = landings.iter().map(|(at, _)| *at).collect();
    let in_period: Vec<&(i64, RunEvent)> = landings
        .iter()
        .filter(|(at, _)| *at >= period.start_ms)
        .collect();
    let length = period.end_ms - period.start_ms;
    let (buckets, bucket_ms, name) = match period.mode {
        ReviewMode::Hourly => (LOOKBACK_HOURS + 1, HOUR_MS, "by_hour"),
        ReviewMode::Daily => (24, HOUR_MS, "by_hour"),
        ReviewMode::Weekly => (7, DAY_MS, "by_day"),
    };
    let stats_from = stats_from(period);
    let or_error =
        |value: Result<Value>| value.unwrap_or_else(|error| json!({"error": format!("{error:#}")}));
    let stats = or_error(sources.stats(
        queue,
        db,
        &StatsQuery {
            since: Some(Cursor::Time(stats_from)),
            until: Some(Cursor::Time(period.end_ms)),
            ..StatsQuery::default()
        },
    ));
    let (kpi_period, last) = match period.mode {
        ReviewMode::Hourly => (Period::Day, 2),
        ReviewMode::Daily => (Period::Day, 8),
        ReviewMode::Weekly => (Period::Week, 5),
    };
    let kpi = or_error(sources.kpi(
        queue,
        db,
        &KpiQuery {
            period: kpi_period,
            last,
            at: Some(Cursor::Time(period.end_ms - 1)),
            ..KpiQuery::default()
        },
    ));
    let events: Vec<Value> = in_period
        .iter()
        .rev()
        .take(LANDING_EVENTS)
        .rev()
        .map(|(_, event)| {
            json!({"id": event.id, "run_id": event.run_id, "task_id": event.task_id, "created_at": event.created_at})
        })
        .collect();
    Ok(json!({
        "period": {
            "mode": period.mode.as_str(),
            "label": period.label,
            "start": millis_text(period.start_ms),
            "end": millis_text(period.end_ms),
        },
        "landings": {
            "total": in_period.len(),
            "previous_period_total": landings.iter().filter(|(at, _)| *at < period.start_ms && *at >= period.start_ms - length).count(),
            name: bucket_counts(&at, period.end_ms, bucket_ms, buckets),
            "events": events,
        },
        "hourly": judgment,
        "health": health(&kpi),
        "kpi": kpi,
        "stats": stats,
        "claim_deferred": or_error(counted(queue, &[crate::domain::claim_defer::CLAIM_DEFERRED], stats_from, period.end_ms, "reason")),
        "asks": or_error(asks(queue, stats_from, period.end_ms)),
        "timelines": or_error(timelines(queue, generators, &in_period)),
    }))
}

/// The health of the workers per route and the disk's least free space
/// (task 1371): the `health` of `kpi`'s last period, the one the review's
/// period ends in (the reviewed day for the daily review, the week for the
/// weekly, the day so far for the hourly), with its label. `kpi`'s error
/// passes as it is.
fn health(kpi: &Value) -> Value {
    if kpi.get("error").is_some() {
        return kpi.clone();
    }
    let latest = kpi
        .get("periods")
        .and_then(Value::as_array)
        .and_then(|periods| periods.last());
    latest.map_or_else(
        || json!({"error": "kpi listed no period"}),
        |period| {
            let mut health = period["health"].clone();
            if let Value::Object(health) = &mut health {
                health.insert("period".to_owned(), period["label"].clone());
            }
            health
        },
    )
}

/// Where the inputs' `stats`, claims deferred and asks begin: the hourly
/// review reads the last 6 hours, the moving average the procedure's
/// hourly look reads; the others their period.
fn stats_from(period: &Window) -> i64 {
    match period.mode {
        ReviewMode::Hourly => period.end_ms - 6 * HOUR_MS,
        _ => period.start_ms,
    }
}

/// The events of `kinds` in `[from, to)` counted by their payload's `key`.
fn counted(
    queue: &(impl Queue + EventReads),
    kinds: &[&str],
    from: i64,
    to: i64,
    key: &str,
) -> Result<Value> {
    let events = queue.events_between(
        EventId::new(0),
        queue.latest_event_id()?,
        &EventFilter {
            kinds: Some(kinds.iter().map(|kind| (*kind).to_owned()).collect()),
            since: Some(millis_text(from)),
            until: Some(millis_text(to)),
            ..EventFilter::default()
        },
        usize::MAX >> 1,
    )?;
    let mut by = serde_json::Map::new();
    for event in &events {
        let value = event.payload[key].as_str().unwrap_or("unknown").to_owned();
        let count = by.entry(value).or_insert(json!(0));
        *count = json!(count.as_u64().unwrap_or(0) + 1);
    }
    Ok(json!({"count": events.len(), "by": by}))
}

/// The asks opened in `[from, to)` by kind, and the open ones by kind.
fn asks(queue: &(impl Queue + EventReads), from: i64, to: i64) -> Result<Value> {
    let opened = counted(queue, &[ASK_OPENED], from, to, "kind")?;
    let mut open = serde_json::Map::new();
    let asks = queue.asks(AskQuery {
        open: true,
        ..AskQuery::default()
    })?;
    for ask in &asks {
        let count = open.entry(ask.kind.as_str().to_owned()).or_insert(json!(0));
        *count = json!(count.as_u64().unwrap_or(0) + 1);
    }
    Ok(json!({"opened": opened, "open": {"count": asks.len(), "by": open}}))
}

/// The timelines of the [`TIMELINES`] runs that landed in the period and
/// took the longest from their first event to their landing.
fn timelines(
    queue: &(impl Queue + EventReads),
    generators: &Generators,
    landed: &[&(i64, RunEvent)],
) -> Result<Value> {
    let mut spans = Vec::new();
    for (at, event) in landed {
        let Some(run) = &event.run_id else { continue };
        let first = queue
            .run_events(run)?
            .first()
            .and_then(|first| timestamp_millis(&first.created_at))
            .unwrap_or(*at);
        spans.push((at - first, run.clone()));
    }
    spans.sort_by_key(|(span, _)| std::cmp::Reverse(*span));
    spans
        .into_iter()
        .take(TIMELINES)
        .map(|(span, run): (i64, RunId)| {
            Ok(json!({
                "run_id": run,
                "secs": span / 1000,
                "timeline": crate::application::watch::timeline_in(
                    queue,
                    &run,
                    TIMELINE_GAP_SECS,
                    false,
                    generators.clock.as_ref(),
                )?,
            }))
        })
        .collect::<Result<Vec<_>>>()
        .map(Value::Array)
}

/// The prompt the job is given and what it took.
#[derive(Debug, Clone)]
pub struct ReviewPrompt {
    pub text: String,
    /// What the prompt took (ADR-t1566-1 decision 6), recorded on
    /// `throughput_review_started`, `throughput_review_finished` and the
    /// dry run's output:
    /// - `prompt_bytes`, a [`PromptBytes`]: `total` is the whole prompt with
    ///   its language line (what `prompt.md` holds and stdin gets) and
    ///   `limit` is [`PROMPT_LIMIT`]. Its sections sum to `total`:
    ///   `instructions` (the procedure and the inputs' explanation, up to
    ///   the opening JSON fence), `input` (the summary's pretty JSON and the
    ///   closing fence, less the trailing whitespace the language line
    ///   trims) and `language` (the language line and the blank line before
    ///   it, 0 without one). `omitted` counts each name of `omitted_to_fit`
    ///   once, not the items of a section as the other jobs do, and
    ///   `over_limit` says why the prompt is still over [`PROMPT_LIMIT`]
    ///   after [`DROP_ORDER`] (null when within it).
    /// - `input_bytes`, the summary of the inputs as pretty JSON, and
    ///   `input_limit`, [`PROMPT_INPUT_LIMIT`].
    /// - `omitted_to_fit`, the parts [`DROP_ORDER`] left out (empty when
    ///   none).
    pub record: Value,
}

/// The job's prompt ([`review_prompt`] with the language line) and what it
/// took.
pub fn job_prompt(
    period: &Window,
    dagq: &str,
    input: &Value,
    input_path: &Path,
    language: Option<&crate::domain::language::Language>,
) -> Result<ReviewPrompt> {
    let summary = prompt_input(input);
    let (instructions, mut input_section) =
        render_prompt_parts(period, dagq, &summary, input_path)?;
    if language.is_some() {
        input_section.truncate(input_section.trim_end().len());
    }
    let body = format!("{instructions}{input_section}");
    let body_bytes = body.len();
    let text = crate::domain::language::with_instruction(body, language);
    let mut omitted = BTreeMap::new();
    for path in DROP_ORDER {
        let name = path.join(".");
        if summary["omitted_to_fit"]
            .as_array()
            .is_some_and(|names| names.iter().any(|item| item.as_str() == Some(&name)))
        {
            // DROP_ORDER names are static and each dropped part counts once.
            let key = match *path {
                ["kpi", "latest"] => "kpi.latest",
                ["kpi", "targets"] => "kpi.targets",
                ["kpi", "periods"] => "kpi.periods",
                [name] => name,
                _ => unreachable!("DROP_ORDER has only one- or two-part paths"),
            };
            omitted.insert(key, 1);
        }
    }
    let bytes = PromptBytes {
        total: text.len(),
        limit: PROMPT_LIMIT,
        sections: BTreeMap::from([
            ("instructions", instructions.len()),
            ("input", input_section.len()),
            ("language", text.len() - body_bytes),
        ]),
        omitted,
        over_limit: (text.len() > PROMPT_LIMIT)
            .then(|| "prompt still exceeds PROMPT_LIMIT after DROP_ORDER".to_owned()),
    };
    let record = json!({
        "prompt_bytes": bytes,
        "input_bytes": pretty_len(&summary),
        "input_limit": PROMPT_INPUT_LIMIT,
        "omitted_to_fit": summary.get("omitted_to_fit").cloned().unwrap_or_else(|| json!([])),
    });
    Ok(ReviewPrompt { text, record })
}

/// Copy the keys of the object `from` into the object `into`.
fn merge(into: &mut Value, from: &Value) {
    if let (Value::Object(into), Value::Object(from)) = (into, from) {
        into.extend(from.iter().map(|(key, value)| (key.clone(), value.clone())));
    }
}

/// The job's instructions: its cadence's part of the weekly review, the
/// form of its output, the procedure itself and the inputs.
pub fn review_prompt(
    period: &Window,
    dagq: &str,
    input: &Value,
    input_path: &Path,
) -> Result<String> {
    render_prompt(period, dagq, &prompt_input(input), input_path)
}

/// [`review_prompt`] with the summary of the inputs ([`prompt_input`])
/// made already.
fn render_prompt(
    period: &Window,
    dagq: &str,
    summary: &Value,
    input_path: &Path,
) -> Result<String> {
    let (instructions, input) = render_prompt_parts(period, dagq, summary, input_path)?;
    Ok(format!("{instructions}{input}"))
}

fn render_prompt_parts(
    period: &Window,
    dagq: &str,
    summary: &Value,
    input_path: &Path,
) -> Result<(String, String)> {
    let cadence = match period.mode {
        ReviewMode::Hourly => hourly_cadence(&period.label, &summary["hourly"]["reasons"]),
        ReviewMode::Daily => format!(
            "This is the daily review of {label}: step 6's daily part, the outliers (step 4) and the targets in `breach` \
             (`kpi` of the last days in the inputs, with the day under review last).",
            label = period.label
        ),
        ReviewMode::Weekly => format!(
            "This is the weekly review of {label}: steps 1 to 5 of the procedure below, in order.",
            label = period.label
        ),
    };
    let next_move = if period.mode == ReviewMode::Weekly {
        format!(
            "- When step 5 finds one change worth making, end your output with that one change as JSON in a block fenced as `{NEXT_MOVE_FENCE}`:\n\
             ```{NEXT_MOVE_FENCE}\n{{\"summary\": \"<the change, one line>\", \"detail\": \"<what to change and how to measure it>\", \"why\": \"<the constraint and the numbers it rests on>\"}}\n```\n\
             The supervisor records it as a finding for a planner, who proposes the work. At most one; leave the block out when no change is worth it.\n"
        )
    } else {
        String::new()
    };
    let instructions = format!(
        "You are the throughput review job of the dagq queue, started headless by the supervisor.\n\
         You read and explain; you change nothing. The queue refuses every command that changes state from your environment \
         (no notes, marks, findings, asks, tasks or goals); the supervisor saves what you print and passes your conclusion to the inbox.\n\
         {cadence}\n\
         \n\
         Do:\n\
         - Follow the procedure below for this cadence, reading the inputs first and more only when needed: \
           `{dagq} kpi [--period day|week] [--last N] [--area A] [--change C]`, `{dagq} stats [--since TIME] [--full]`, `{dagq} timeline RUN`, \
           `{dagq} events --full --kind KIND --since TIME --until TIME` (UTC, YYYY-MM-DDTHH:MM:SSZ; add `--all` for every kind), `{dagq} asks`, `{dagq} marks`, `{dagq} findings`, `{dagq} show ID`.\n\
         - Never compute a number the inputs and the commands do not give, and keep the time runs waited for a person apart from their own time.\n\
         - Print your review in Markdown: first a `## Conclusion` heading with at most 5 short lines a person reads first (what happened, the one constraint or cause, what to do or that nothing needs doing), \
           then `## Details` with the numbers and the runs they rest on.\n\
         {next_move}\
         \n\
         The procedure (the dagq skill's `reference/kpi.md`):\n\
         \n\
         {procedure}\n\
         \n\
         Inputs, a summary (JSON: the period, its landings (`by_hour` or `by_day`, oldest first, and the period before's total), the hourly rules' judgment, \
         `kpi` (`periods`: each period's runs; `latest`: the period under review, each KPI's `all` stratum with its `comparison` to the period before and the 7 days before; \
         `targets`: each target's state and its latest value), the parts of `stats` in `stats.parts` (`stats.omitted` names the others), \
         the claims deferred and the asks by reason and kind, and the longest runs that landed with their seconds). \
         The whole inputs are saved in `{input_path}` in the review's directory for a person; you read the details through the commands above instead: \
         `kpi` for the other periods, strata and the host (`--period`, `--last`, `--area`, `--change`), `stats --since {start} --until {end} --full` for the runs, \
         `timeline RUN` for each of the longest runs, and `events --full` for the landings (`--kind run_integrated`) and anything else. \
         `omitted_to_fit`, when present, names what was left out to keep this prompt small; read it with the commands too:\n\
         ```json\n",
        procedure = procedure(),
        input_path = input_path.display(),
        start = millis_text(stats_from(period)),
        end = millis_text(period.end_ms),
    );
    let input = format!("{}\n```\n", serde_json::to_string_pretty(summary)?);
    Ok((instructions, input))
}

/// The hourly review's part of the prompt (ADR-t1172-1 decision 2): an
/// hour that met rules (`reasons`, the judgment's) puts them at the head of
/// its conclusion and explains them; a quiet hour, which met none, is
/// written short by step 6's hourly cadence (an hour alone is noise): what
/// is steady, and any sign that would matter if it went on.
fn hourly_cadence(label: &str, reasons: &Value) -> String {
    let met: Vec<&str> = reasons
        .as_array()
        .map(|reasons| reasons.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let rules = "`deviation` — its landings are far off the 6 hours before, `sustained_drop` — the 3-hour average stayed well below the 24-hour one for 3 hours, \
                 `no_landing` — nothing landed";
    if met.is_empty() {
        format!(
            "This is the hourly review of the hour {label}. The hour met none of the runtime's rules ({rules}): it is a quiet hour. \
             Step 6 says an hour alone is noise, so keep the review short: say in the conclusion what is steady on the 3–6 hour moving average, \
             and name any sign that would matter if it went on (runs growing longer, waits on a person, the landing slot, the claims deferred, the load), or that there is none."
        )
    } else {
        format!(
            "This is the hourly review of the hour {label}. The hour met the runtime's rules {met} (`hourly.reasons` in the inputs; {rules}). \
             Start the `## Conclusion` with a line that names the rules met, `Rules met: {met}`, before anything else. \
             Step 6 says an hour alone is noise: say whether landings have stopped or slowed on the 3–6 hour moving average, \
             and why the hour rose or fell (which runs were long, what waited: a person, the landing slot, the claims deferred, the load).",
            met = met.join(", ")
        )
    }
}

/// The part of the review's `input` the prompt carries (task 1099): the
/// period, the landings without their events, the hourly judgment, the
/// workers' health, `kpi`
/// cut to each period's runs, the period under review's `all` stratum and
/// the targets' states, [`PROMPT_STATS`] of `stats`, the claims deferred,
/// the asks, and the longest runs without their timelines. While it passes
/// [`PROMPT_INPUT_LIMIT`], the parts of [`DROP_ORDER`] go, named in
/// `omitted_to_fit`.
pub fn prompt_input(input: &Value) -> Value {
    let mut summary = json!({
        "period": input["period"],
        "landings": without(&input["landings"], &["events"]),
        "hourly": input["hourly"],
        "health": input["health"],
        "kpi": kpi_summary(&input["kpi"]),
        "stats": stats_summary(&input["stats"]),
        "claim_deferred": input["claim_deferred"],
        "asks": input["asks"],
        "timelines": timelines_summary(&input["timelines"]),
    });
    let mut omitted = Vec::new();
    for path in DROP_ORDER {
        if pretty_len(&summary) <= PROMPT_INPUT_LIMIT {
            break;
        }
        let (last, parents) = path.split_last().expect("a path has a key");
        let parent = parents
            .iter()
            .try_fold(&mut summary, |value, key| value.get_mut(*key));
        if let Some(Value::Object(parent)) = parent
            && parent.remove(*last).is_some_and(|part| !part.is_null())
        {
            omitted.push(path.join("."));
        }
    }
    if !omitted.is_empty() {
        summary["omitted_to_fit"] = json!(omitted);
    }
    summary
}

fn pretty_len(value: &Value) -> usize {
    serde_json::to_string_pretty(value).map_or(usize::MAX, |text| text.len())
}

/// `value` without `keys`, when it is an object.
fn without(value: &Value, keys: &[&str]) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| !keys.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `kpi` cut for the prompt: the periods' labels and runs, the period
/// under review's KPIs in the `all` stratum with their comparison, and the
/// targets without their periods but for the latest value. An error, or a
/// shape not known, passes as it is.
fn kpi_summary(kpi: &Value) -> Value {
    let Some(periods) = kpi.get("periods").and_then(Value::as_array) else {
        return kpi.clone();
    };
    let listed: Vec<Value> = periods
        .iter()
        .map(|period| {
            json!({
                "label": period["label"],
                "partial": period["partial"],
                "runs": period["runs"],
            })
        })
        .collect();
    let latest = periods.last().map(|period| {
        let comparison = &period["comparison"];
        let kpis: serde_json::Map<String, Value> = period["kpis"]
            .as_object()
            .map(|kpis| {
                kpis.iter()
                    .map(|(name, strata)| {
                        let compared = &comparison[name]["all"];
                        let mut value = json!({"all": strata["all"]});
                        if compared.is_object() {
                            value["comparison"] = without(compared, &["judged", "delta"]);
                        }
                        (name.clone(), value)
                    })
                    .collect()
            })
            .unwrap_or_default();
        json!({
            "label": period["label"],
            "partial": period["partial"],
            "runs": period["runs"],
            "unavailable": period["unavailable"],
            "kpis": kpis,
        })
    });
    let targets: Vec<Value> = kpi["targets"]
        .as_array()
        .map(|targets| {
            targets
                .iter()
                .map(|target| {
                    let latest = target["periods"]
                        .as_array()
                        .and_then(|periods| periods.last())
                        .cloned()
                        .unwrap_or(Value::Null);
                    let mut target = without(target, &["periods"]);
                    target["latest"] = latest;
                    target
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "cores": kpi["cores"],
        "periods": listed,
        "latest": latest,
        "targets": targets,
    })
}

/// [`PROMPT_STATS`] of `stats` under `parts`, and the names of the rest
/// under `omitted`. An error passes as it is.
fn stats_summary(stats: &Value) -> Value {
    let Some(object) = stats
        .as_object()
        .filter(|stats| !stats.contains_key("error"))
    else {
        return stats.clone();
    };
    let parts: serde_json::Map<String, Value> = object
        .iter()
        .filter(|(key, _)| PROMPT_STATS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let omitted: Vec<&String> = object
        .keys()
        .filter(|key| !PROMPT_STATS.contains(&key.as_str()))
        .collect();
    json!({"parts": parts, "omitted": omitted})
}

/// The longest runs with their seconds, without the timelines.
fn timelines_summary(timelines: &Value) -> Value {
    match timelines.as_array() {
        Some(runs) => Value::Array(
            runs.iter()
                .map(|run| json!({"run_id": run["run_id"], "secs": run["secs"]}))
                .collect(),
        ),
        None => timelines.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_procedure_is_the_weekly_review_of_the_kpi_reference() {
        let text = procedure();
        assert!(text.starts_with(PROCEDURE_HEADING), "{text}");
        assert!(text.contains("**Cadence.**"), "{text}");
        assert!(!text.contains("## `dagq forecast`"), "{text}");
    }

    #[test]
    fn the_prompt_names_its_cadence_and_only_the_weekly_one_asks_for_a_next_move() {
        let input = json!({"landings": {"total": 3}});
        let at = |mode| window(mode, 1_790_655_900_000, 9 * HOUR_MS);
        let hourly = review_prompt(
            &at(ReviewMode::Hourly),
            "dagq",
            &input,
            Path::new("/q/input.json"),
        )
        .unwrap();
        assert!(
            hourly.contains("hourly review of the hour 2026-09-29T12"),
            "{hourly}"
        );
        assert!(hourly.contains("`sustained_drop`"));
        assert!(!hourly.contains("```next_move"));
        let daily = review_prompt(
            &at(ReviewMode::Daily),
            "dagq",
            &input,
            Path::new("/q/input.json"),
        )
        .unwrap();
        assert!(daily.contains("daily review of 2026-09-28"));
        let weekly = review_prompt(
            &at(ReviewMode::Weekly),
            "dagq",
            &input,
            Path::new("/q/input.json"),
        )
        .unwrap();
        assert!(weekly.contains("steps 1 to 5"));
        assert!(weekly.contains("```next_move"));
        for prompt in [&hourly, &daily, &weekly] {
            assert!(prompt.contains("## Conclusion"));
            assert!(prompt.contains("Raising throughput: the weekly review"));
            assert!(prompt.contains("\"total\": 3"));
            assert!(prompt.contains("`dagq timeline RUN`"));
            // The strata of the procedure's step 5 (ADR-t980-1), not the
            // removed `--kind`.
            assert!(prompt.contains("[--area A] [--change C]"));
            assert!(!prompt.contains("--kind runtime"));
            // The details are the job's to read with its commands; the
            // whole inputs are in the review's directory (task 1099).
            assert!(prompt.contains("`/q/input.json`"), "{prompt}");
            // The range of the inputs' `stats`: 6 hours for the hourly one.
            let since = if prompt == &hourly {
                "2026-09-28T22:00:00.000Z"
            } else {
                ""
            };
            assert!(
                prompt.contains(&format!("stats --since {since}")),
                "{prompt}"
            );
            assert!(prompt.contains("you read the details through the commands above"));
            assert!(prompt.contains("`dagq kpi [--period day|week]"));
            assert!(prompt.contains("`dagq stats [--since TIME] [--full]`"));
            assert!(prompt.contains("`dagq events --full --kind KIND"));
            assert!(prompt.contains("`timeline RUN` for each of the longest runs"));
        }
        assert!(
            daily.contains(
                "`stats --since 2026-09-27T15:00:00.000Z --until 2026-09-28T15:00:00.000Z --full`"
            ),
            "{daily}"
        );
    }

    /// An hour that met rules puts them at the head of its conclusion; a
    /// quiet hour is written short (ADR-t1172-1 decision 2).
    #[test]
    fn the_hourly_prompt_stresses_the_rules_met_and_keeps_a_quiet_hour_short() {
        let hour = window(ReviewMode::Hourly, 1_790_655_900_000, 9 * HOUR_MS);
        let prompt = |hourly: Value| {
            review_prompt(
                &hour,
                "dagq",
                &json!({"hourly": hourly}),
                Path::new("/q/input.json"),
            )
            .unwrap()
        };
        let met = prompt(json!({"reasons": ["deviation", "no_landing"], "triggered": true}));
        assert!(met.contains("Rules met: deviation, no_landing"), "{met}");
        assert!(met.contains("why the hour rose or fell"), "{met}");
        assert!(!met.contains("it is a quiet hour"), "{met}");
        for quiet in [json!({"reasons": [], "triggered": false}), Value::Null] {
            let text = prompt(quiet);
            assert!(text.contains("it is a quiet hour"), "{text}");
            assert!(text.contains("keep the review short"), "{text}");
            assert!(!text.contains("Rules met:"), "{text}");
        }
    }

    /// A KPI period of `kpis` KPIs, each with `strata` strata and a
    /// comparison, and `marks` marks with long labels: the shape `kpi`
    /// gives, as large as a production day's.
    fn kpi_period(label: &str, kpis: usize, strata: usize, marks: usize) -> Value {
        let strata = |value: Value| -> Value {
            let mut all = serde_json::Map::new();
            all.insert("all".into(), value.clone());
            for n in 0..strata {
                all.insert(format!("change=c{n}"), value.clone());
            }
            Value::Object(all)
        };
        let mut values = serde_json::Map::new();
        let mut comparison = serde_json::Map::new();
        for n in 0..kpis {
            values.insert(
                format!("kpi_{n}"),
                strata(json!({"max": 23541.0, "median": 11241.0, "min": 7168.0, "n": 3, "p90": 23541.0})),
            );
            comparison.insert(
                format!("kpi_{n}"),
                strata(json!({"baseline_7d": 526.0, "delta": 11024.0, "judged": true, "previous": 217.0,
                              "ratio": 51.802, "reason": null, "verdict": "worsened"})),
            );
        }
        let marks: Vec<Value> = (0..marks)
            .map(|n| json!({"id": n, "kind": "mark_recorded", "label": "x".repeat(400)}))
            .collect();
        json!({"label": label, "partial": false, "runs": 111, "kpis": values,
               "comparison": comparison, "marks": marks, "unavailable": {}})
    }

    /// The input's `health` is that of `kpi`'s last period, the one the
    /// review's period ends in, named by its label (task 1371).
    #[test]
    fn the_health_is_that_of_the_last_kpi_period() {
        let kpi = json!({"periods": [
            {"label": "2026-10-01", "health": {"routes": {}, "disk": null}},
            {"label": "2026-10-02", "health": {"routes": {"headless": {"turns": 7}},
                                               "disk": {"min_free_bytes": 900.0, "median_free_bytes": 9000.0}}},
        ]});
        let health = health(&kpi);
        assert_eq!(health["period"], "2026-10-02");
        assert_eq!(health["routes"]["headless"]["turns"], 7);
        assert_eq!(health["disk"]["min_free_bytes"], 900.0);
        assert_eq!(
            super::health(&json!({"error": "no"})),
            json!({"error": "no"})
        );
        assert_eq!(
            super::health(&json!({"periods": []})),
            json!({"error": "kpi listed no period"})
        );
    }

    #[test]
    fn the_prompt_carries_a_summary_of_the_inputs_within_its_limit() {
        let periods: Vec<Value> = (21..=28)
            .map(|day| kpi_period(&format!("2026-09-{day}"), 110, 6, 80))
            .collect();
        let targets: Vec<Value> = (0..5)
            .map(|n| json!({"kpi": format!("kpi_{n}"), "stratum": "all", "state": if n == 0 { "breach" } else { "ok" },
                            "streak": n, "min": null, "max": 0.6,
                            "periods": (21..=28).map(|day| json!({"period": format!("2026-09-{day}"), "value": day, "met": true})).collect::<Vec<_>>()}))
            .collect();
        let runs: Vec<Value> = (0..400)
            .map(|n| json!({"run_id": format!("run-{n}"), "work": n, "detail": "y".repeat(300)}))
            .collect();
        let events: Vec<Value> = (0..200)
            .map(|n| json!({"id": n, "run_id": format!("run-{n}"), "task_id": n, "created_at": "2026-09-28T01:00:00.000Z"}))
            .collect();
        let input = json!({
            "period": {"mode": "daily", "label": "2026-09-28"},
            "landings": {"total": 109, "previous_period_total": 158, "by_hour": vec![4; 24], "events": events},
            "hourly": null,
            "health": {"period": "2026-09-28", "disk": {"min_free_bytes": 2048.0},
                       "routes": {"headless": {"turns": 40, "stall_nudged": 3}}},
            "kpi": {"cores": 8, "periods": periods, "targets": targets},
            "stats": {"overall": {"landed": 109}, "landing_utilization": {"utilization": 0.481},
                      "runs": runs, "versions": "v".repeat(100_000)},
            "claim_deferred": {"count": 3, "by": {"load": 3}},
            "asks": {"opened": {"count": 1}},
            "timelines": (0..3).map(|n| json!({"run_id": format!("run-{n}"), "secs": 40477 - n, "timeline": {"commands": "z".repeat(50_000)}})).collect::<Vec<_>>(),
        });
        assert!(input.to_string().len() > 1024 * 1024, "the input is MBs");
        let summary = prompt_input(&input);
        assert!(
            pretty_len(&summary) <= PROMPT_INPUT_LIMIT,
            "{}",
            pretty_len(&summary)
        );
        assert_eq!(summary["landings"]["total"], 109);
        assert!(summary["landings"].get("events").is_none());
        // The workers' health and the disk pass whole (task 1371).
        assert_eq!(summary["health"], input["health"]);
        assert_eq!(summary["kpi"]["periods"].as_array().unwrap().len(), 8);
        assert_eq!(summary["kpi"]["periods"][7]["runs"], 111);
        let latest = &summary["kpi"]["latest"];
        assert_eq!(latest["label"], "2026-09-28");
        assert_eq!(latest["kpis"]["kpi_0"]["all"]["median"], 11241.0);
        assert_eq!(latest["kpis"]["kpi_0"]["comparison"]["verdict"], "worsened");
        assert!(latest["kpis"]["kpi_0"].get("change=c0").is_none());
        assert!(latest.get("marks").is_none());
        assert_eq!(summary["kpi"]["targets"][0]["state"], "breach");
        assert_eq!(summary["kpi"]["targets"][0]["latest"]["value"], 28);
        assert!(summary["kpi"]["targets"][0].get("periods").is_none());
        assert_eq!(
            summary["stats"]["parts"]["landing_utilization"]["utilization"],
            0.481
        );
        assert_eq!(summary["stats"]["omitted"], json!(["runs", "versions"]));
        assert_eq!(
            summary["timelines"],
            json!([
                {"run_id": "run-0", "secs": 40477},
                {"run_id": "run-1", "secs": 40476},
                {"run_id": "run-2", "secs": 40475},
            ])
        );
        assert!(summary.get("omitted_to_fit").is_none());
        let window = window(ReviewMode::Daily, 1_790_655_900_000, 0);
        let prompt = review_prompt(
            &window,
            "dagq --db /q/queue.db",
            &input,
            Path::new("/q/input.json"),
        )
        .unwrap();
        assert!(prompt.len() <= PROMPT_LIMIT, "{}", prompt.len());
        // An error passes as it is.
        let failed = prompt_input(
            &json!({"kpi": {"error": "no"}, "stats": {"error": "no"}, "timelines": {"error": "no"}}),
        );
        assert_eq!(failed["kpi"], json!({"error": "no"}));
        assert_eq!(failed["stats"], json!({"error": "no"}));
        assert_eq!(failed["timelines"], json!({"error": "no"}));
    }

    #[test]
    fn a_summary_past_its_limit_drops_the_parts_in_order_and_names_them() {
        let huge = "s".repeat(PROMPT_INPUT_LIMIT);
        let input = json!({
            "period": {"label": "2026-W39"},
            "landings": {"total": 5},
            "stats": {"overall": huge},
            "kpi": {"cores": 8, "periods": [{"label": "2026-W39", "kpis": {"a": {"all": {"value": huge}}}}], "targets": []},
        });
        let summary = prompt_input(&input);
        assert!(pretty_len(&summary) <= PROMPT_INPUT_LIMIT);
        assert_eq!(summary["omitted_to_fit"], json!(["stats", "kpi.latest"]));
        assert_eq!(summary["kpi"]["periods"][0]["label"], "2026-W39");
        assert_eq!(summary["landings"]["total"], 5);
        // Everything but the period goes when nothing else fits.
        let input = json!({"period": {"label": "x"}, "landings": {"total": huge}});
        let summary = prompt_input(&input);
        assert_eq!(summary["period"]["label"], "x");
        assert!(summary.get("landings").is_none());
        assert_eq!(summary["omitted_to_fit"], json!(["landings"]));
    }

    #[test]
    fn the_job_prompt_records_its_bytes_within_the_limit_after_dropping_parts() {
        let huge = "s".repeat(PROMPT_INPUT_LIMIT);
        let input = json!({
            "period": {"label": "2026-W39"},
            "landings": {"total": 5},
            "stats": {"overall": huge},
            "timelines": [{"run_id": "run-0", "secs": 9, "timeline": huge}],
            "kpi": {"cores": 8, "periods": [{"label": "2026-W39", "kpis": {"a": {"all": {"value": huge}}}}], "targets": []},
        });
        assert!(input.to_string().len() > PROMPT_LIMIT);
        let language = crate::domain::language::Language {
            tag: "ja".to_owned(),
            source: crate::domain::language::LanguageSource::Repository,
        };
        let window = window(ReviewMode::Weekly, 1_790_655_900_000, 0);
        let fitted = job_prompt(
            &window,
            "dagq",
            &input,
            Path::new("/q/input.json"),
            Some(&language),
        )
        .unwrap();
        let record = &fitted.record;
        // The whole prompt, its language line included, is counted.
        assert!(fitted.text.ends_with(&language.instruction()));
        assert_eq!(record["prompt_bytes"]["total"], fitted.text.len());
        assert!(fitted.text.len() <= PROMPT_LIMIT, "{record}");
        assert_eq!(record["prompt_bytes"]["limit"], PROMPT_LIMIT);
        let summary = prompt_input(&input);
        assert_eq!(record["input_bytes"], pretty_len(&summary));
        assert!(pretty_len(&summary) <= PROMPT_INPUT_LIMIT, "{record}");
        assert_eq!(record["input_limit"], PROMPT_INPUT_LIMIT);
        assert_eq!(record["omitted_to_fit"], json!(["stats", "kpi.latest"]));
        assert_eq!(record["omitted_to_fit"], summary["omitted_to_fit"]);
        assert_eq!(
            record["prompt_bytes"]["omitted"],
            json!({"stats": 1, "kpi.latest": 1})
        );
        assert!(record.get("prompt_limit").is_none());
        // The text is the prompt the review renders, with the language.
        assert_eq!(
            fitted.text,
            crate::domain::language::with_instruction(
                review_prompt(&window, "dagq", &input, Path::new("/q/input.json")).unwrap(),
                Some(&language)
            )
        );
        // Nothing dropped: the names are an empty list.
        let small = job_prompt(
            &window,
            "dagq",
            &json!({"landings": {"total": 3}}),
            Path::new("/q/input.json"),
            None,
        )
        .unwrap();
        assert_eq!(small.record["omitted_to_fit"], json!([]));
        assert_eq!(small.record["prompt_bytes"]["total"], small.text.len());
        assert_eq!(small.record["prompt_bytes"]["omitted"], json!({}));
        for prompt in [&fitted, &small] {
            let bytes = &prompt.record["prompt_bytes"];
            assert!(bytes["over_limit"].is_null());
            assert_eq!(
                bytes["sections"]
                    .as_object()
                    .unwrap()
                    .values()
                    .map(|value| value.as_u64().unwrap())
                    .sum::<u64>(),
                prompt.text.len() as u64
            );
        }
        let too_large = job_prompt(
            &window,
            "dagq",
            &json!({"period": "x".repeat(PROMPT_LIMIT)}),
            Path::new("/q/input.json"),
            None,
        )
        .unwrap();
        assert!(
            too_large.record["prompt_bytes"]["over_limit"]
                .as_str()
                .unwrap()
                .contains("DROP_ORDER")
        );
    }
}
