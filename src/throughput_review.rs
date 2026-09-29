//! The throughput review job (ADR-t996-1): the supervisor starts
//! `throughput-review` on its timer for the last whole hour, yesterday and
//! the ISO week before this one. For an hour the runtime counts the
//! landings and judges them by rules
//! ([`crate::domain::throughput_review::judge_hourly`]); an hour no rule
//! hit starts no agent and records a skipped `throughput_review_finished`.
//! Otherwise a headless agent (`DAGQ_ROLE=throughput-review-job`, which the
//! queue lets read only, without MCP) reads the landings, `kpi`, `stats`,
//! the claim deferrals, the asks and the timelines of the longest runs, and
//! follows the weekly review of the dagq skill's `reference/kpi.md` for its
//! cadence. The command, not the agent, saves the review under
//! `<queue dir>/reports/reviews/`, records the weekly next move as a
//! finding marked for a proposal, and records `throughput_review_reported`,
//! the inbox's notice. A failed job leaves its log and a failed
//! `throughput_review_finished`, and nothing else.
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::{
    application::AgentProvider,
    domain::{
        ActorContext, EventFilter, EventId, EventKind, FindingTarget, NewFinding, RunEvent, RunId,
        actor_model::{ActorLaunch, ModelRole},
        event_kind::{ASK_OPENED, RUN_INTEGRATED},
        kpi::{DAY_MS, KpiQuery, Period},
        stats::{Cursor, StatsQuery, timestamp_millis},
        throughput_review::{
            HOUR_MS, HourlyJudgment, LOOKBACK_HOURS, NEXT_MOVE_FENCE, ReviewMode, ReviewOutput,
            Window, bucket_counts, judge_hourly, parse_output, window,
        },
        transcript::millis_text,
    },
    infrastructure::{adapters::shell_join, asks::AskQuery, sqlite::SqliteQueue},
    observer::{HeadlessAgent, run_agent},
};

/// How long one review may take before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The tools the job may use: the queue CLI only.
pub const ALLOWED_TOOLS: &[&str] = &["Bash(dagq:*)"];
/// The runs landed in the period whose timelines the input carries, the
/// longest first.
pub const TIMELINES: usize = 3;
/// The shortest gap the timelines report, in seconds.
const TIMELINE_GAP_SECS: i64 = 300;
/// The landing events the input lists, the newest kept.
const LANDING_EVENTS: usize = 200;
/// The finding kind of the weekly next move.
pub const FINDING_KIND: &str = "throughput";

/// The dagq skill's KPI reference, whose weekly review the job follows: the
/// prompt carries the procedure from there, not a copy of its own.
const KPI_REFERENCE: &str = include_str!("../plugins/claude-dagq/skills/dagq/reference/kpi.md");
/// The heading of the weekly review in [`KPI_REFERENCE`].
const PROCEDURE_HEADING: &str = "## Raising throughput: the weekly review";

#[derive(Debug, Clone)]
pub struct ReviewOptions {
    pub mode: ReviewMode,
    /// The unix second the period is the latest finished one at; now
    /// without (the supervisor passes the second it found the review due).
    pub at: Option<i64>,
    /// Build and return the prompt without starting the agent, whatever
    /// the rules made of the hour.
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

/// Run one review: judge the hour (hourly), gather the inputs, start the
/// agent headless, save what it printed and tell the inbox.
pub fn review(db: &Path, provider: &dyn AgentProvider, options: &ReviewOptions) -> Result<Value> {
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let mut queue = SqliteQueue::open(&db)?;
    let now = options.at.unwrap_or_else(|| queue.generators().clock.now());
    let offset_ms = options
        .utc_offset
        .unwrap_or_else(|| crate::infrastructure::clock::local_utc_offset(now))
        * 1000;
    let period = window(options.mode, now * 1000, offset_ms);
    let landings = landings(&queue, &period)?;
    let judgment = match options.mode {
        ReviewMode::Hourly => Some(judge_hourly(&bucket_counts(
            &landings.iter().map(|(at, _)| *at).collect::<Vec<_>>(),
            period.end_ms,
            HOUR_MS,
            LOOKBACK_HOURS + 1,
        ))?),
        _ => None,
    };
    if !options.dry_run
        && let Some(judged) = judgment.as_ref().filter(|judged| !judged.triggered)
    {
        let payload = json!({
            "mode": options.mode.as_str(),
            "period": period.label,
            "outcome": "skipped",
            "reason": "the hour met no rule",
            "hourly": judged,
            "pid": std::process::id(),
            "parent_pid": std::os::unix::process::parent_id(),
        });
        queue.record_queue_event(EventKind::ThroughputReviewFinished, payload.clone())?;
        tracing::info!(
            period = period.label,
            "throughput review (hourly) skipped: no rule met"
        );
        return Ok(payload);
    }
    let input = gather(&queue, &db, &period, &landings, judgment.as_ref())?;
    let command = shell_join(&[
        "dagq".into(),
        "--db".into(),
        db.to_string_lossy().into_owned(),
    ]);
    let checkout = crate::compose::bound_checkout(&queue)?;
    let language = crate::infrastructure::language::language_for_prompt(
        checkout.as_deref(),
        options.user_config.as_deref(),
    );
    let prompt = crate::domain::language::with_instruction(
        review_prompt(&period, &command, &input)?,
        language.as_ref(),
    );
    if options.dry_run {
        return Ok(json!({
            "dry_run": true,
            "mode": options.mode.as_str(),
            "period": period.label,
            "hourly": judgment,
            "prompt": prompt,
        }));
    }
    let dir = review_dir(&db, &period)?;
    fs::write(dir.join("prompt.md"), &prompt)?;
    fs::write(
        dir.join("input.json"),
        serde_json::to_string_pretty(&input)?,
    )?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let launch = review_launch(checkout.as_deref());
    let reasons = judgment.as_ref().map(|judged| judged.reasons.clone());
    let started = queue.record_queue_event(
        EventKind::ThroughputReviewStarted,
        json!({
            "mode": options.mode.as_str(),
            "period": period.label,
            "reasons": reasons,
            "dir": dir,
            "session_id": session_id,
            "launch": launch.to_value(),
        }),
    )?;
    tracing::info!(
        mode = options.mode.as_str(),
        period = period.label,
        "throughput review ({}) started",
        options.mode.as_str()
    );
    let clock = Instant::now();
    let ran = run_agent(
        provider,
        &db,
        &dir,
        &prompt,
        &HeadlessAgent {
            actor: ActorContext::throughput_review_job(options.mode.as_str(), &period.label),
            session_id: &session_id,
            launch: &launch,
            dagq: &options.dagq,
            timeout: options.timeout,
            what: "the throughput review",
            allowed_tools: ALLOWED_TOOLS,
        },
    );
    let (outcome, exit_code, error) = match ran {
        Ok(Some(0)) => ("succeeded", Some(0), None),
        Ok(code) => ("failed", code, None),
        Err(error) => ("error", None, Some(format!("{error:#}"))),
    };
    let mut payload = json!({
        "mode": options.mode.as_str(),
        "period": period.label,
        "outcome": outcome,
        "exit_code": exit_code,
        "error": error,
        "reasons": reasons,
        "dir": dir,
        "duration_secs": clock.elapsed().as_secs(),
        // Who to reap: a supervisor that exec'd while this ran is still its
        // parent and waits on it by these.
        "pid": std::process::id(),
        "parent_pid": std::os::unix::process::parent_id(),
    });
    if outcome == "succeeded" {
        // A review that could not be saved or reported is a failed one:
        // the event says why, and the loop goes on.
        match report(&mut queue, &dir, &period, reasons.as_deref(), started) {
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
fn report(
    queue: &mut SqliteQueue,
    dir: &Path,
    period: &Window,
    reasons: Option<&[crate::domain::throughput_review::HourlyReason]>,
    started: EventId,
) -> Result<Reported> {
    let output = fs::read_to_string(dir.join("output.log")).context("read the review's output")?;
    let ReviewOutput {
        conclusion,
        text,
        next_move,
        next_move_error,
    } = parse_output(&output);
    let path = dir.join("review.md");
    fs::write(&path, format!("{text}\n"))?;
    // Only the weekly review proposes a change (ADR-t996-1 decision 4).
    let next_move = next_move.filter(|_| period.mode == ReviewMode::Weekly);
    let finding_id = match &next_move {
        Some(next) => {
            let detail = match &next.detail {
                Some(detail) => format!("{detail}\n\nWhy: {}", next.why),
                None => format!("Why: {}", next.why),
            };
            let recorded = queue.record_finding(NewFinding {
                kind: FINDING_KIND.to_owned(),
                target: FindingTarget::Queue,
                subject: format!("weekly/{}", period.label),
                summary: next.summary.clone(),
                detail: Some(detail),
                impact: None,
                evidence: vec![started],
                propose: Some(next.why.clone()),
                by: crate::domain::ActorRole::Supervisor.as_str().to_owned(),
            })?;
            Some(recorded.finding.id.as_i64())
        }
        None => None,
    };
    fs::write(
        dir.join("review.json"),
        serde_json::to_string_pretty(&json!({
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
fn landings(queue: &SqliteQueue, period: &Window) -> Result<Vec<(i64, RunEvent)>> {
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

/// The review's input: the landings, the rules' judgment, `kpi`, `stats`,
/// the claim deferrals, the asks and the timelines of the longest runs
/// that landed. A part that does not read is its `{"error": ...}`.
fn gather(
    queue: &SqliteQueue,
    db: &Path,
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
    // The hourly review reads the stats of the last 6 hours: the moving
    // average the procedure's hourly look reads.
    let stats_from = match period.mode {
        ReviewMode::Hourly => period.end_ms - 6 * HOUR_MS,
        _ => period.start_ms,
    };
    let one_shot = crate::compose::OneShot::new(queue.generators().clone());
    let or_error =
        |value: Result<Value>| value.unwrap_or_else(|error| json!({"error": format!("{error:#}")}));
    let stats = or_error(one_shot.stats_of(
        queue,
        db,
        &StatsQuery {
            since: Some(Cursor::Time(stats_from)),
            until: Some(Cursor::Time(period.end_ms)),
            ..StatsQuery::default()
        },
        None,
    ));
    let (kpi_period, last) = match period.mode {
        ReviewMode::Hourly => (Period::Day, 2),
        ReviewMode::Daily => (Period::Day, 8),
        ReviewMode::Weekly => (Period::Week, 5),
    };
    let kpi = or_error(one_shot.kpi_of(
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
        "kpi": kpi,
        "stats": stats,
        "claim_deferred": or_error(counted(queue, &[crate::domain::claim_defer::CLAIM_DEFERRED], stats_from, period.end_ms, "reason")),
        "asks": or_error(asks(queue, stats_from, period.end_ms)),
        "timelines": or_error(timelines(queue, &in_period)),
    }))
}

/// The events of `kinds` in `[from, to)` counted by their payload's `key`.
fn counted(queue: &SqliteQueue, kinds: &[&str], from: i64, to: i64, key: &str) -> Result<Value> {
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
fn asks(queue: &SqliteQueue, from: i64, to: i64) -> Result<Value> {
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
fn timelines(queue: &SqliteQueue, landed: &[&(i64, RunEvent)]) -> Result<Value> {
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
                "timeline": crate::watch::timeline_in(queue, &run, TIMELINE_GAP_SECS, false)?,
            }))
        })
        .collect::<Result<Vec<_>>>()
        .map(Value::Array)
}

/// `<queue dir>/reports/reviews/<mode>-<period>/`, suffixed when one
/// already exists (a review run again by hand).
fn review_dir(db: &Path, period: &Window) -> Result<PathBuf> {
    let root = reviews_dir(db);
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    for n in 0.. {
        let name = if n == 0 {
            format!("{}-{}", period.mode.as_str(), period.label)
        } else {
            format!("{}-{}-{n}", period.mode.as_str(), period.label)
        };
        let dir = root.join(name);
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("create {}", dir.display())),
        }
    }
    unreachable!("the suffixes do not run out")
}

/// What the job's agent starts with (ADR-0079 decision 7):
/// `[roles.throughput_review]` of the bound checkout's `dagq.toml`; none,
/// no checkout, or a file that cannot be read starts it with the
/// provider's default.
fn review_launch(checkout: Option<&Path>) -> ActorLaunch {
    let Some(checkout) = checkout else {
        return ActorLaunch::default_of(ModelRole::ThroughputReview);
    };
    match crate::infrastructure::run_env::load_role_models(checkout) {
        Ok(models) => models.launch(ModelRole::ThroughputReview),
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "[roles.throughput_review] could not be read; starting it with the default: {error:#}");
            ActorLaunch::default_of(ModelRole::ThroughputReview)
        }
    }
}

/// The job's instructions: its cadence's part of the weekly review, the
/// form of its output, the procedure itself and the inputs.
pub fn review_prompt(period: &Window, dagq: &str, input: &Value) -> Result<String> {
    let cadence = match period.mode {
        ReviewMode::Hourly => format!(
            "This is the hourly review of the hour {label}. The runtime's rules found it worth a look (`hourly.reasons` in the inputs: \
             `deviation` — its landings are far off the 6 hours before, `sustained_drop` — the 3-hour average stayed well below the 24-hour one for 3 hours, \
             `no_landing` — nothing landed). Step 6 says an hour alone is noise: say whether landings have stopped or slowed on the 3–6 hour moving average, \
             and why the hour rose or fell (which runs were long, what waited: a person, the landing slot, the claims deferred, the load).",
            label = period.label
        ),
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
    Ok(format!(
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
         Inputs (JSON: the period, its landings (`by_hour` or `by_day`, oldest first, and the period before's total), the hourly rules' judgment, `kpi`, `stats`, \
         the claims deferred and the asks by reason and kind, and the timelines of the longest runs that landed):\n\
         ```json\n{input}\n```\n",
        procedure = procedure(),
        input = serde_json::to_string_pretty(input)?,
    ))
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
        let hourly = review_prompt(&at(ReviewMode::Hourly), "dagq", &input).unwrap();
        assert!(
            hourly.contains("hourly review of the hour 2026-09-29T12"),
            "{hourly}"
        );
        assert!(hourly.contains("`sustained_drop`"));
        assert!(!hourly.contains("```next_move"));
        let daily = review_prompt(&at(ReviewMode::Daily), "dagq", &input).unwrap();
        assert!(daily.contains("daily review of 2026-09-28"));
        let weekly = review_prompt(&at(ReviewMode::Weekly), "dagq", &input).unwrap();
        assert!(weekly.contains("steps 1 to 5"));
        assert!(weekly.contains("```next_move"));
        for prompt in [&hourly, &daily, &weekly] {
            assert!(prompt.contains("## Conclusion"));
            assert!(prompt.contains("Raising throughput: the weekly review"));
            assert!(prompt.contains("\"total\": 3"));
            assert!(prompt.contains("`dagq timeline RUN`"));
            // The strata of the procedure's step 5 (ADR-t980-1), not the
            // kind that is going away.
            assert!(prompt.contains("[--area A] [--change C]"));
            assert!(!prompt.contains("--kind runtime"));
        }
    }
}
