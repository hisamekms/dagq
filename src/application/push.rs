//! The push of the KPIs (ADR-0051 decisions 18, 22 and 23): after the
//! supervisor's daily reports, the breaches of the targets judged now are
//! recorded as they start and end, and the messages for the host's push
//! command are made: each breach as it starts (up to the day's limit) and
//! the summary of the latest day and week written. Running the command,
//! trying it again and recording how it went is the supervisor's
//! ([`record_attempt`]). Without `[push]` nothing is made nor recorded
//! but the breaches. The targets are judged on the KPIs of the ledger's
//! window ([`super::kpi::of_window`]).
use crate::domain::EventKind;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use super::{
    AskQuery, AskStore, LedgerStore, QueueRecords, RunLog,
    report::{self, ReportSetup, Written},
};
use crate::domain::kpi::{
    KpiQuery, Period,
    push::{self, PushConfig, PushMessage},
    read_from_ms,
    report::Report,
};

/// One run of the push command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRequest {
    /// The program and its arguments, run without a shell.
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
}

/// How one run of the push command ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushOutcome {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub stderr: String,
    /// Why it did not run or could not be waited for.
    pub error: Option<String>,
}

impl PushOutcome {
    /// A command that did not run.
    pub fn error(message: &str) -> Self {
        Self {
            error: Some(message.to_owned()),
            ..Self::default()
        }
    }
}

/// Where the messages are about: the queue's name in their titles and its
/// database (`DAGQ_QUEUE`, `queue`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    pub name: String,
    pub queue: String,
}

/// The breaches recorded by one check: started and resolved payloads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BreachCheck {
    pub started: Vec<Value>,
    pub resolved: Vec<Value>,
}

/// Judge each latest finished day and week not already checked in the queue,
/// recording completion only after its transitions. Restarting skips checked
/// periods; when both are checked no ledger window is read. Record each
/// breach that started and each that ended since the last check (decision
/// 18). A start is marked `pushed` while `push` sends breaches and the
/// local day's limit allows.
///
/// After downtime across local midnight, the next job checks the latest
/// finished day and week at that time, without backfilling older periods.
///
/// Read one ledger window with
/// [`WindowQuery::read_from_ms`](crate::domain::ledger::window::WindowQuery::read_from_ms)
/// set to the earliest [`read_from_ms`] of the unchecked day and week queries
/// (`last: 2`), including the baseline and breach streak history both need.
pub fn check_breaches(
    queue: &(impl QueueRecords + ?Sized),
    ledger: &(impl LedgerStore + ?Sized),
    setup: &ReportSetup,
    push: Option<&PushConfig>,
    now: i64,
    supervisor: &crate::domain::LeaseToken,
) -> Result<BreachCheck> {
    let checked = queue.kpi_breaches_checked()?;
    let offset_ms = setup.host.utc_offset_secs * 1000;
    let queries: Vec<_> = [Period::Day, Period::Week]
        .into_iter()
        .filter_map(|period| {
            let start = period.start(super::stats::now_ms(now), offset_ms);
            let label = period.label(period.start(start - 1, offset_ms), offset_ms);
            (!checked.contains(&(period.as_str().to_owned(), label.clone()))).then_some((
                KpiQuery {
                    period,
                    last: 2,
                    ..KpiQuery::default()
                },
                label,
            ))
        })
        .collect();
    let mut check = BreachCheck::default();
    if queries.is_empty() {
        return Ok(check);
    }
    let open = queue.kpi_breaches_open()?;
    if setup.config.targets.is_empty() && open.is_empty() {
        return Ok(check);
    }
    let day = report::local_day(now, setup.host.utc_offset_secs);
    let push_day = push
        .filter(|push| push.breach)
        .map(|push| (day, push.max_breach_per_day));
    let from = queries
        .iter()
        .filter_map(|(query, _)| {
            read_from_ms(query, &setup.config, now, setup.host.utc_offset_secs)
        })
        .min();
    let window = ledger.ledger_window(&crate::domain::ledger::window::WindowQuery {
        now_ms: super::stats::now_ms(now),
        read_from_ms: from,
        ..Default::default()
    })?;
    for (query, label) in &queries {
        let period = query.period;
        let kpi = super::kpi::of_window(
            &window,
            now,
            setup.host,
            &setup.config,
            setup.areas.as_ref(),
            query,
        )?;
        for target in &kpi.targets {
            if let Some(started) = push::breach_started(period.as_str(), target)
                && let Some(recorded) =
                    queue.record_kpi_breach(EventKind::KpiBreachStarted, started, push_day)?
            {
                check.started.push(recorded);
            }
        }
        for breach in open
            .iter()
            .filter(|open| open.get("period").and_then(Value::as_str) == Some(period.as_str()))
        {
            if let Some(resolved) = push::breach_resolved(breach, &kpi.targets)
                && let Some(recorded) =
                    queue.record_kpi_breach(EventKind::KpiBreachResolved, resolved, None)?
            {
                check.resolved.push(recorded);
            }
        }
        queue.record_kpi_breach_checked(json!({
            "period": period.as_str(),
            "label": label,
            "build": setup.build,
            "supervisor": supervisor,
        }))?;
    }
    Ok(check)
}

/// The messages after the reports `written` at `now`: the breaches just
/// recorded as pushed, then the summary of the latest day and of the
/// latest week written (older ones a backfill wrote are not pushed).
pub fn messages(
    queue: &(impl AskStore + ?Sized),
    push: &PushConfig,
    target: &PushTarget,
    breaches: &BreachCheck,
    written: &[(Written, Report)],
) -> Result<Vec<PushMessage>> {
    let mut messages = Vec::new();
    if push.breach {
        messages.extend(
            breaches
                .started
                .iter()
                .filter(|started| started.get("pushed") == Some(&Value::Bool(true)))
                .map(|started| push::breach_message(&target.name, &target.queue, started)),
        );
    }
    if !push.daily {
        return Ok(messages);
    }
    let open_asks = queue
        .asks(AskQuery {
            open: true,
            ..AskQuery::default()
        })?
        .len();
    for period in [Period::Day, Period::Week] {
        let Some((files, report)) = written
            .iter()
            .rev()
            .find(|(files, _)| files.period == period.as_str())
        else {
            continue;
        };
        let resolved: Vec<Value> = breaches
            .resolved
            .iter()
            .filter(|end| end.get("period").and_then(Value::as_str) == Some(period.as_str()))
            .cloned()
            .collect();
        messages.push(push::summary_message(
            &target.name,
            &target.queue,
            report,
            &resolved,
            open_asks,
            (&files.html, &files.json),
        ));
    }
    Ok(messages)
}

/// The run of `push`'s command with `message` (decision 22's environment).
pub fn request(push: &PushConfig, target: &PushTarget, message: &PushMessage) -> PushRequest {
    PushRequest {
        command: push.command.clone(),
        env: vec![
            ("DAGQ_PUSH_KIND".into(), message.kind.as_str().into()),
            ("DAGQ_QUEUE".into(), target.queue.clone()),
            (
                "DAGQ_REPORT_HTML".into(),
                message.report_html.clone().unwrap_or_default(),
            ),
            (
                "DAGQ_REPORT_JSON".into(),
                message.report_json.clone().unwrap_or_default(),
            ),
        ],
        stdin: message.stdin(),
        timeout: Duration::from_secs(push.timeout_secs),
    }
}

/// How a run of `push`'s command failed: timed out, the error that kept
/// it from running, or its exit code or signal.
fn failed_how(push: &PushConfig, outcome: &PushOutcome) -> String {
    if outcome.timed_out {
        format!("timed out after {}s", push.timeout_secs)
    } else if let Some(error) = &outcome.error {
        error.clone()
    } else {
        match (outcome.exit_code, outcome.signal) {
            (Some(code), _) => format!("exit code {code}"),
            (None, Some(signal)) => format!("signal {signal}"),
            (None, None) => "failed".to_owned(),
        }
    }
}

/// Why a run of `push`'s command failed, for a person: how it failed and
/// the tail of its stderr with the command's arguments masked
/// ([`push::stderr_tail`]).
pub fn failure_reason(push: &PushConfig, outcome: &PushOutcome) -> String {
    let how = failed_how(push, outcome);
    let stderr = push::stderr_tail(&outcome.stderr, &push.command);
    if stderr.trim().is_empty() {
        how
    } else {
        format!("{how}: {}", stderr.trim())
    }
}

/// Record how the `attempt`-th run of `message` went: `kpi_push_sent`, or
/// `kpi_push_failed` with what of the failure may be kept (no argument of
/// the command, no part of the message), and once the last attempt failed
/// `kpi_push_abandoned` unless one already waits for a person. Returns
/// whether the message is to be tried again.
pub fn record_attempt(
    queue: &(impl QueueRecords + RunLog + ?Sized),
    push: &PushConfig,
    message: &PushMessage,
    attempt: usize,
    outcome: &PushOutcome,
) -> Result<bool> {
    let kind = message.kind.as_str();
    if outcome.success {
        queue.record_queue_event(
            EventKind::KpiPushSent,
            json!({"push_kind": kind, "period": message.period, "attempt": attempt}),
        )?;
        return Ok(false);
    }
    let again = push::retry_after(attempt).is_some();
    let stderr = push::stderr_tail(&outcome.stderr, &push.command);
    queue.record_queue_event(
        EventKind::KpiPushFailed,
        json!({
            "push_kind": kind,
            "period": message.period,
            "attempt": attempt,
            "exit_code": outcome.exit_code,
            "signal": outcome.signal,
            "timed_out": outcome.timed_out,
            "error": outcome.error,
            "stderr_tail": stderr,
            "gave_up": !again,
        }),
    )?;
    if !again {
        let why = failed_how(push, outcome);
        queue.record_kpi_push_abandoned(json!({
            "push_kind": kind,
            "period": message.period,
            "attempts": attempt,
            "reason_category": "recovery_failed",
            "message": format!(
                "the KPI push command failed {attempt} times on the {kind} message of {} ({why}); the message was given up",
                message.period
            ),
        }))?;
    }
    Ok(again)
}

#[cfg(test)]
mod tests;
