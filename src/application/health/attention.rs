//! What waits for a person (`attention`) and the events as the inbox reads
//! them (observation and analysis).

use super::*;
use crate::domain::EventKind;

/// `text` cut to `limit` characters with `…` appended, or `None` when it fits.
pub fn truncate(text: &str, limit: usize) -> Option<String> {
    let mut chars = text.char_indices();
    let (end, _) = chars.nth(limit)?;
    Some(format!("{}…", &text[..end]))
}

/// `text` cut to [`REASON_CHARS`] characters, with `…` when it was longer.
pub fn truncate_reason(text: &str) -> String {
    truncate(text, REASON_CHARS).unwrap_or_else(|| text.to_owned())
}

/// One event as the inbox reads it: the row's ids and kind, and from the
/// payload only `status`, `exit_code`, the reason `code`, a truncated `reason` (from
/// `reason`, `message` or `error`) and, for a step of the automatic or the
/// release update, its `commit` or `release`, `version`, `stage` and
/// `plugin` (ADR-t618-2). Paths and receipts are left out but for the
/// directory of a throughput review's or an observation's finish.
/// An attention event also carries its `next`.
pub fn compact_event(event: &RunEvent) -> Value {
    let mut value = json!({"id": event.id, "kind": event.kind});
    let object = value.as_object_mut().expect("object literal");
    if let Some(task_id) = event.task_id {
        object.insert("task_id".into(), json!(task_id));
    }
    if let Some(goal_id) = event.goal_id {
        object.insert("goal_id".into(), json!(goal_id));
    }
    if let Some(run_id) = &event.run_id {
        object.insert("run_id".into(), json!(run_id));
    }
    let payload = &event.payload;
    if let Some(status) = payload.get("status").or_else(|| payload.get("to")) {
        object.insert("status".into(), status.clone());
    }
    if let Some(code) = payload.get("exit_code") {
        object.insert("exit_code".into(), code.clone());
    }
    if let Some(ask_id) = payload.get("ask_id") {
        object.insert("ask_id".into(), ask_id.clone());
    }
    if let Some(reason) = payload.get("reason_category") {
        object.insert("reason_category".into(), reason.clone());
    }
    if event.kind == EventKind::AskOpened {
        // The asking AI's recommendation and confidence (ADR-t451-1
        // decision 1), null without them and for an ask opened before.
        for key in ["recommendation", "confidence"] {
            object.insert(key.into(), payload.get(key).cloned().unwrap_or(Value::Null));
        }
    }
    if event.kind.is_in(crate::domain::UPDATE_EVENT_KINDS) {
        // Which build a step of the automatic update is about, and where
        // a failure happened.
        for key in ["commit", "release", "version", "stage", "plugin"] {
            if let Some(value) = payload.get(key) {
                object.insert(key.into(), value.clone());
            }
        }
    }
    if event.kind == EventKind::ThroughputReviewReported {
        // What the inbox shows the person of a throughput review
        // (ADR-t996-1): its period, conclusion and where the whole is.
        for key in [
            "mode",
            "period",
            "reasons",
            "conclusion",
            "path",
            "finding_id",
        ] {
            if let Some(value) = payload.get(key) {
                object.insert(key.into(), value.clone());
            }
        }
    }
    if event.kind == EventKind::ThroughputReviewFinished {
        // Which review ended how, and the directory whose `output.out`
        // and `output.err` say why a failed one did (task 1099).
        for key in ["mode", "period", "outcome", "dir"] {
            if let Some(value) = payload.get(key) {
                object.insert(key.into(), value.clone());
            }
        }
    }
    if event.kind == EventKind::ObserveFinished {
        // Which observation ended how, how many in a row failed, and the
        // directory whose `output.out` and `output.err` say why (task
        // 1574); `reason` carries its error.
        for key in [
            "mode",
            "outcome",
            crate::domain::CONSECUTIVE_FAILURES,
            "dir",
        ] {
            if let Some(value) = payload.get(key) {
                object.insert(key.into(), value.clone());
            }
        }
    }
    if let Some(code) = payload.get(reason::CODE_KEY) {
        object.insert(reason::CODE_KEY.into(), code.clone());
    }
    if let Some(reason) = ["reason", "message", "error"]
        .iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_str))
    {
        object.insert("reason".into(), json!(truncate_reason(reason)));
    }
    if let Some(next) = event_attention(&event.kind, payload) {
        object.insert("next".into(), json!(next));
    }
    object.insert("created_at".into(), json!(event.created_at));
    value
}

/// Whether a live supervisor applies the answer of the `update_failed` ask
/// `ask`: one with the automatic update for a build's failure, one of a
/// release build for a release's job's (ADR-t618-1).
pub(super) fn update_failure_applied(
    queue: &(impl AskStore + RunLog + ?Sized),
    registrations: &[SupervisorRegistration],
    ask: &crate::domain::Ask,
    now: i64,
    control: &dyn ProcessControl,
) -> Result<bool> {
    let updates = queue.update_events(crate::application::update::UPDATE_HISTORY)?;
    let alive = |pid| control.alive(pid);
    // A person's install's failure is the inbox's to read and close
    // (ADR-0073 decision 14).
    if crate::application::update::failed_step(&updates, ask.id)
        .is_some_and(crate::application::update::step_install)
    {
        return Ok(false);
    }
    Ok(
        if crate::application::update::failed_release(&updates, ask.id).is_some() {
            let releases = queue.release_updates_on();
            registrations
                .iter()
                .any(|registration| registration.applies_releases(now, alive, releases))
        } else {
            registrations
                .iter()
                .any(|registration| registration.applies_updates(now, alive))
        },
    )
}

/// What waits for a person now: stale or missing supervisors first,
/// then the latest run of every `in_progress` task that rests where only a
/// person or the supervisor moves it on or that is unfinished without a lease,
/// then every landed run whose push of `main` failed with no successful push
/// since, then what the queue, the planning and the goals hold, then every
/// ask nobody closed: an open one as `ask_opened` for the
/// inbox, an answered one as `ask_answered` for the person to act on
/// through the inbox (ADR-0022, ADR-0024 decision 6).
/// `kind` is the event that brought the run there (for a run without
/// a lease, its latest `runtime_error`).
/// By the run's own judgement (`run_attention_of`), an unfinished run whose
/// lease is stale is no attention: its dead supervisor's is, or an adoption
/// takes it, and a lease a dead `integrate` left is none. The run-level
/// notices judged before it are not bound by the lease: an `integrating`
/// run's `landing_release_stuck` and a past `recovery_failed` of a live
/// session still show.
/// `ci_named_jobs` is the jobs `required_jobs`
/// of `[ci_watch]` names now (empty without the table, `None` when
/// `dagq.toml` cannot be read): a `ci_jobs_missing` stands only while it
/// names a missing job ([`crate::domain::ci_watch::WatchState::standing_jobs_missing`]).
pub fn attention(
    queue: &(
         impl AskStore
         + DraftPlannerStore
         + GoalReviewStore
         + PlanReviewStore
         + QueueRecords
         + CoordinationReads
         + SupervisorReads
         + RunLog
         + SessionReads
         + TaskReads
         + ?Sized
     ),
    registrations: &[SupervisorRegistration],
    now: i64,
    control: &dyn ProcessControl,
    ci_named_jobs: Option<&[String]>,
) -> Result<Vec<Attention>> {
    let mut attention = supervisor_attention(&pulses(registrations, now, control));
    let no_claude = registrations.iter().any(|r| {
        !crate::domain::SupervisorPulse::judge(r, control.alive(r.pid), now).stale
            && r.claude_disabled()
    });
    if no_claude {
        attention.push(Attention {
            run_id: None, task_id: None, pid: None, ask_id: None, reason_category: None,
            status: "manual".into(), kind: "provider_disabled".into(),
            last_error: Some("Claude is disabled by --no-claude: use a manual inbox and planner; handle review, plan review, recovery, observer and throughput review manually. Workers wait when Codex is unavailable; Claude workers from earlier runs must be recovered manually.".into()),
            last_error_code: None, next: AttentionNext::ManualRoles,
        });
    }

    // A headless job that failed at a login or the usage limit waits in
    // the hold ask that lists it, which is the attention (task 438).
    let asks = queue.asks(AskQuery::default())?;
    let held = |job: HoldJob| queue_hold::job_held(&asks, &job);
    for mut run in queue.latest_runs_in_progress()? {
        let leased = queue.run_lease(run.id())?.is_some();
        if !leased {
            // A supervisor leaves the unfinished statuses before it releases
            // the lease, so a run read before a release and its lease read
            // after it would look abandoned: judge it by its status now.
            run = queue.run(run.id())?;
        }
        // A run whose review raised a concern waits in its
        // `approve_landing` ask, which is the attention (ADR-0027).
        if run.status() == RunStatus::AwaitingIntegration
            && !leased
            && queue.has_unclosed_ask(run.id(), AskKind::ApproveLanding)?
        {
            continue;
        }
        let events = queue.run_events(run.id())?;
        // A turn or a verification of the run waits for the dagq machine's
        // memory (ADR-t3163-1 decision 4): the runtime's while it is short,
        // the person's once it has gone on long.
        if let Some(event) = crate::domain::machine_memory::standing_wait(&events) {
            attention.push(Attention {
                run_id: Some(run.id().clone()),
                task_id: Some(run.task_id()),
                pid: None,
                ask_id: None,
                reason_category: None,
                status: run.status().as_str().into(),
                kind: event.kind.to_string(),
                last_error: event.payload["why"].as_str().map(truncate_reason),
                last_error_code: None,
                next: if event.kind == EventKind::ContainerMemoryWaitLong {
                    AttentionNext::CheckMachineMemory
                } else {
                    AttentionNext::WaitingForMemory
                },
            });
            continue;
        }
        // A claim of `already_satisfied` at rest waits in its
        // `approve_close` ask likewise, or, before its verification, for
        // the container its verification is set to, which is the queue's
        // attention (ADR-t1818-1).
        if crate::domain::satisfied::blocks_recovery(run.status(), leased, &events) {
            continue;
        }
        // A run a `land` answer approved, or one recovered from a landing
        // it may land again, waits for the supervisor to land it (tasks 949
        // and 1118), not for a person's `integrate`; one recovered from a
        // landing it may not land again, or whose failed review a person
        // answered `retry_review` (ADR-t2521-1), waits for the supervisor's
        // review; one whose lease was given back after its review passed,
        // with nobody asked, waits for a supervisor to take it up.
        if run.status() == RunStatus::AwaitingIntegration && !leased {
            let seen = crate::domain::stranded_landing::Seen {
                status: run.status(),
                leased,
                ask_open: asks
                    .iter()
                    .any(|ask| ask.run_id.as_ref() == Some(run.id()) && ask.closed_at.is_none()),
                fenced: queue.task_fenced(run.task_id())?,
                task: TaskStatus::InProgress,
            };
            let waits = runtime_wait(&events, seen);
            if let Some((kind, next)) = waits {
                attention.push(Attention {
                    run_id: Some(run.id().clone()),
                    task_id: Some(run.task_id()),
                    pid: None,
                    ask_id: None,
                    reason_category: None,
                    status: run.status().as_str().into(),
                    kind: kind.to_owned(),
                    last_error: None,
                    last_error_code: None,
                    next,
                });
                continue;
            }
        }
        // A run whose e2e after its review could not run several times in a
        // row waits on the host (ADR-t1233-2 decision 3) until it runs.
        if run.status() == RunStatus::AwaitingIntegration
            && let Some(event) = crate::domain::run_e2e::standing_attention(&events)
        {
            attention.push(Attention {
                run_id: Some(run.id().clone()),
                task_id: Some(run.task_id()),
                pid: None,
                ask_id: None,
                reason_category: None,
                status: run.status().as_str().into(),
                kind: event.kind.to_string(),
                last_error: event.payload["error"].as_str().map(truncate_reason),
                last_error_code: None,
                next: AttentionNext::CheckE2e,
            });
            continue;
        }
        // A dead landing whose worktree's processes the runtime could not
        // stop holds the integration slot until a person stops them (task
        // 1129).
        if run.status() == RunStatus::Integrating
            && let Some(event) = crate::domain::landing_release::stuck(&events)
        {
            attention.push(Attention {
                run_id: Some(run.id().clone()),
                task_id: Some(run.task_id()),
                pid: None,
                ask_id: None,
                reason_category: None,
                status: run.status().as_str().into(),
                kind: event.kind.to_string(),
                last_error: event.payload["error"].as_str().map(truncate_reason),
                last_error_code: None,
                next: AttentionNext::StopLandingProcesses,
            });
            continue;
        }
        // A live session whose recovery job failed under a runtime from
        // before ADR-t609-1 waits for a person to recover it by hand
        // (ADR-0047 decision 40), whatever its status; a failed job opens
        // the alert's own ask now.
        if let Some(failed) = crate::domain::recovery::failed_live(&events, None) {
            attention.push(Attention {
                run_id: Some(run.id().clone()),
                task_id: Some(run.task_id()),
                pid: None,
                ask_id: None,
                reason_category: Some(crate::domain::AskReason::RecoveryFailed),
                status: run.status().as_str().into(),
                kind: failed.kind.to_string(),
                last_error: failed.payload["error"].as_str().map(truncate_reason),
                last_error_code: Some(ReasonCode::JobFailed),
                next: AttentionNext::RecoverByHand,
            });
            continue;
        }
        let Some((mut next, kind)) =
            run_attention_of(&RunHistory::from_events(&events), run.status(), leased)
        else {
            continue;
        };
        if matches!(next, AttentionNext::TriageByHand) && held(HoldJob::Recovery(run.id().clone()))
        {
            continue;
        }
        if no_claude
            && run.actual_provider() == crate::domain::Provider::Claude
            && next == AttentionNext::Resuming
        {
            next = AttentionNext::RecoverByHand;
        }
        let kind = kind.unwrap_or(run.status().as_str()).to_owned();
        attention.push(Attention {
            run_id: Some(run.id().clone()),
            task_id: Some(run.task_id()),
            pid: None,
            ask_id: None,
            reason_category: None,
            status: run.status().as_str().into(),
            kind,
            last_error: run.last_error().map(truncate_reason),
            last_error_code: reason::run_error_code(&run, &events),
            next,
        });
    }
    for run in queue.runs_with_pending_push()? {
        let Some(next) = run_attention(run.status(), false, true, false) else {
            continue;
        };
        let events = queue.run_events(run.id())?;
        let error = RunHistory::from_events(&events)
            .push_failure()
            .map(truncate_reason);
        attention.push(Attention {
            run_id: Some(run.id().clone()),
            task_id: Some(run.task_id()),
            pid: None,
            ask_id: None,
            reason_category: None,
            status: run.status().as_str().into(),
            kind: event_kind::PUSH_FAILED.into(),
            last_error_code: error.as_ref().map(|_| ReasonCode::PushFailed),
            last_error: error,
            next,
        });
    }
    if let Some(event) = queue.latest_queue_event(&[
        crate::domain::sccache::RESTART_FAILED,
        crate::domain::sccache::SCCACHE_SERVER_STARTED,
    ])? && event.kind == EventKind::SccacheServerRestartFailed
    {
        attention.push(Attention {
            run_id: None,
            task_id: None,
            pid: event.payload["pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok()),
            ask_id: None,
            reason_category: None,
            status: "failed".into(),
            kind: event.kind.to_string(),
            last_error: event.payload["error"].as_str().map(truncate_reason),
            last_error_code: None,
            next: AttentionNext::CheckSccache,
        });
    }
    // A program [run.env] names that the supervisor did not find stops its
    // claims and landings until a person installs it or takes it out of
    // dagq.toml (ADR-0049 decision 9).
    if let Some(event) = queue.latest_queue_event(&RUN_ENV_PROGRAM_KINDS)?
        && event.kind == EventKind::RunEnvProgramMissing
    {
        attention.push(Attention {
            run_id: None,
            task_id: None,
            pid: None,
            ask_id: None,
            reason_category: None,
            status: "missing".into(),
            kind: RUN_ENV_PROGRAM_MISSING.into(),
            last_error: event
                .payload
                .get("message")
                .and_then(Value::as_str)
                .map(truncate_reason),
            last_error_code: None,
            next: AttentionNext::InstallTool,
        });
    }
    // The landing recheck could not write its build output, so it runs no
    // command and records nothing on the runs it would have judged until a
    // recheck writes there again (ADR-0068 decision 2).
    if let Some(event) = queue.latest_queue_event(&crate::domain::recheck::TARGET_KINDS)?
        && event.kind == EventKind::LandingRecheckTargetUnwritable
    {
        attention.push(Attention {
            run_id: None,
            task_id: None,
            pid: None,
            ask_id: None,
            reason_category: None,
            status: "unwritable".into(),
            kind: event.kind.to_string(),
            last_error: Some(truncate_reason(&format!(
                "{}: {}",
                event.payload["path"].as_str().unwrap_or_default(),
                event.payload["error"].as_str().unwrap_or_default()
            ))),
            last_error_code: None,
            next: AttentionNext::FixRecheckTarget,
        });
    }
    attention.extend(landing_recheck_main_attention(
        queue
            .latest_queue_event(&crate::domain::recheck::LANDING_RECHECK_MAIN_KINDS)?
            .as_ref(),
    ));
    // A target dagq.toml sets to the container cannot be used: its claims,
    // resumes or landings wait, never on the host, until the container can
    // be used or a task sets it back to the host (goal 38).
    if let Some(event) =
        queue.latest_queue_event(&crate::domain::container_runtime::CONTAINER_RUNTIME_KINDS)?
        && event.kind == EventKind::ContainerRuntimeUnavailable
    {
        attention.push(Attention {
            run_id: None,
            task_id: None,
            pid: None,
            ask_id: None,
            reason_category: None,
            status: "unavailable".into(),
            kind: event.kind.to_string(),
            last_error: event
                .payload
                .get("message")
                .and_then(Value::as_str)
                .map(truncate_reason),
            last_error_code: None,
            next: AttentionNext::FixDagqToml,
        });
    }
    // The CI the supervisor watches cannot be read: its claims and
    // landings wait until a person installs or logs in `gh`, or fixes
    // `dagq.toml` (ADR-t1920-1 decision 2).
    if let Some(event) =
        queue.latest_queue_event(&crate::domain::ci_watch::CI_WATCH_ACCESS_KINDS)?
        && event.kind == EventKind::CiWatchUnavailable
    {
        attention.push(Attention {
            run_id: None,
            task_id: None,
            pid: None,
            ask_id: None,
            reason_category: None,
            status: "unavailable".into(),
            kind: crate::domain::ci_watch::CI_WATCH_UNAVAILABLE.into(),
            last_error: event
                .payload
                .get("message")
                .and_then(Value::as_str)
                .map(truncate_reason),
            last_error_code: None,
            next: crate::domain::ci_watch::Unavailable::next_of(
                event.payload.get("reason").and_then(Value::as_str),
            ),
        });
    }
    // `required_jobs` of `[ci_watch]` names a job the workflow's runs lack:
    // no success run reads green until a task fixes `dagq.toml` or the
    // workflow (ADR-t2034-1 decision 5); a green run ends it, and so does
    // a setting that no longer names the missing jobs.
    if let Some(payload) = crate::domain::ci_watch::WatchState::fold(&queue.ci_watch_events()?)
        .standing_jobs_missing(ci_named_jobs)
    {
        attention.push(Attention {
            run_id: None,
            task_id: None,
            pid: None,
            ask_id: None,
            reason_category: None,
            status: "missing".into(),
            kind: crate::domain::ci_watch::CI_JOBS_MISSING.into(),
            last_error: payload
                .get("message")
                .and_then(Value::as_str)
                .map(truncate_reason),
            last_error_code: None,
            next: AttentionNext::FixDagqToml,
        });
    }
    // The host's KPI push command that failed a message three times waits
    // for a person until a push succeeds (ADR-0051 decision 23).
    if let Some(event) = queue.latest_queue_event(&KPI_PUSH_ATTENTION_KINDS)?
        && event.kind == EventKind::KpiPushAbandoned
    {
        attention.push(Attention {
            run_id: None,
            task_id: None,
            pid: None,
            ask_id: None,
            reason_category: Some(crate::domain::AskReason::RecoveryFailed),
            status: "failed".into(),
            kind: KPI_PUSH_ABANDONED.into(),
            last_error: event
                .payload
                .get("message")
                .and_then(Value::as_str)
                .map(truncate_reason),
            last_error_code: None,
            next: AttentionNext::FixPush,
        });
    }
    // The queue service the supervisor could not keep running waits for a
    // person until it runs again (ADR-t1233-4 decision 3): the supervisor
    // writes it here, on the DB, not through the service.
    if let Some(event) =
        queue.latest_queue_event(&crate::domain::queue_service::QUEUE_SERVICE_ATTENTION_KINDS)?
        && crate::domain::queue_service::attention_stands(Some(&event.kind))
    {
        let reason = event.payload.get("reason").and_then(Value::as_str);
        let message = event.payload.get("message").and_then(Value::as_str);
        attention.push(Attention {
            run_id: None,
            task_id: None,
            pid: None,
            ask_id: None,
            reason_category: None,
            status: reason.unwrap_or("down").into(),
            kind: crate::domain::queue_service::QUEUE_SERVICE_DOWN.into(),
            last_error: message.map(truncate_reason),
            last_error_code: None,
            next: AttentionNext::QueueServiceStatus,
        });
    }
    // So does the queue broker (ADR-t2114-1 decision 1), written the same
    // way.
    if let Some(event) =
        queue.latest_queue_event(&crate::domain::queue_broker::QUEUE_BROKER_ATTENTION_KINDS)?
        && crate::domain::queue_broker::attention_stands(Some(&event.kind))
    {
        let reason = event.payload.get("reason").and_then(Value::as_str);
        let message = event.payload.get("message").and_then(Value::as_str);
        attention.push(Attention {
            run_id: None,
            task_id: None,
            pid: None,
            ask_id: None,
            reason_category: None,
            status: reason.unwrap_or("down").into(),
            kind: crate::domain::queue_broker::QUEUE_BROKER_DOWN.into(),
            last_error: message.map(truncate_reason),
            last_error_code: None,
            next: AttentionNext::QueueBrokerLog,
        });
    }
    // And the relays of the runs' containers (ADR-t2114-3 decision 4).
    if let Some(event) =
        queue.latest_queue_event(&crate::domain::broker_relay::QUEUE_RELAY_ATTENTION_KINDS)?
        && crate::domain::broker_relay::attention_stands(Some(&event.kind))
    {
        let reason = event.payload.get("reason").and_then(Value::as_str);
        let message = event.payload.get("message").and_then(Value::as_str);
        attention.push(Attention {
            run_id: None,
            task_id: None,
            pid: None,
            ask_id: None,
            reason_category: None,
            status: reason.unwrap_or("down").into(),
            kind: crate::domain::broker_relay::QUEUE_RELAY_DOWN.into(),
            last_error: message.map(truncate_reason),
            last_error_code: None,
            next: AttentionNext::QueueRelayMachine,
        });
    }
    // A proposal whose plan review failed, or whose planner did not answer
    // a revise, waits for a person outside any ask (ADR-0041 decisions 13,
    // 17); it is shown on the proposal's first task.
    // A draft the runtime's planners left undecided waits for a planning
    // request the inbox records (ADR-0041 decision 16, ADR-t1394-1 decision
    // 8).
    for draft in queue.exhausted_drafts()? {
        attention.push(Attention {
            run_id: None,
            task_id: Some(draft.id()),
            pid: None,
            ask_id: None,
            reason_category: None,
            status: draft.status().as_str().into(),
            kind: event_kind::DRAFT_PLANNER_EXHAUSTED.into(),
            last_error: None,
            last_error_code: None,
            next: AttentionNext::DecideDraft,
        });
    }
    // A finding the runtime's planners left undecided waits for a planning
    // request the inbox records (ADR-0044 decision 19, ADR-t1394-1 decision
    // 8); it is shown on its task,
    // or on none for a finding on the queue or a goal.
    for finding in queue.exhausted_findings()? {
        attention.push(Attention {
            run_id: finding.run_id.clone(),
            task_id: finding.task_id,
            pid: None,
            ask_id: None,
            reason_category: None,
            status: finding.status.as_str().into(),
            kind: event_kind::FINDING_PLANNER_EXHAUSTED.into(),
            last_error: Some(truncate_reason(&format!(
                "finding {}: {}",
                finding.id, finding.summary
            ))),
            last_error_code: None,
            next: AttentionNext::DecideFinding,
        });
    }
    // A task of a closed goal that will not complete strands the tasks
    // waiting on it until a planning request's planner or a person decides
    // them (task 421); it is
    // shown on that task, once however many wait.
    for stranded in queue.stranded_dependencies()? {
        attention.push(Attention {
            run_id: None,
            task_id: Some(stranded.task_id),
            pid: None,
            ask_id: None,
            reason_category: None,
            status: "stranded".into(),
            kind: event_kind::DEPENDENCY_STRANDED.into(),
            last_error: Some(truncate_reason(&stranded.summary())),
            last_error_code: None,
            next: AttentionNext::DecideWaiting,
        });
    }
    // A planner of the runtime's whose turn its wrapper stopped at the
    // turn's limit (`planner_unresponsive` with `subject: "planner"`;
    // before task 1441 also one nothing was seen of within the planner
    // timeout, task 805) waits for a person to look at it until its row
    // closes; it is shown on its draft's task, if it has one.
    for (planner, event) in queue.silent_planners()? {
        attention.push(Attention {
            run_id: None,
            task_id: planner.draft_task_id,
            pid: None,
            ask_id: None,
            reason_category: None,
            status: "working".into(),
            kind: event_kind::PLANNER_UNRESPONSIVE.into(),
            last_error: event
                .payload
                .get("reason")
                .and_then(Value::as_str)
                .map(truncate_reason),
            last_error_code: None,
            next: AttentionNext::CheckPlanner,
        });
    }
    for hold in queue.plan_review_holds()? {
        if hold.kind == EventKind::PlanReviewFailed && held(HoldJob::PlanReview(hold.proposal_id)) {
            continue;
        }
        let (status, next) = match hold.kind {
            EventKind::PlanReviewFailed => ("submitted", AttentionNext::PlanReviewByHand),
            _ => ("revising", AttentionNext::CheckPlanner),
        };
        attention.push(Attention {
            run_id: None,
            task_id: Some(hold.anchor),
            pid: None,
            ask_id: None,
            reason_category: None,
            status: status.into(),
            kind: hold.kind.as_str().into(),
            last_error: hold.error.as_deref().map(truncate_reason),
            last_error_code: hold.error.as_ref().map(|_| ReasonCode::JobFailed),
            next,
        });
    }
    // A goal whose goal review failed waits for a person until its tasks
    // change or `goal review ID` (ADR-0047 decision 43); it is shown on the
    // goal's first task.
    let goal_holds = queue.goal_review_holds()?;
    for hold in &goal_holds {
        if held(HoldJob::GoalReview(hold.goal_id)) {
            continue;
        }
        attention.push(Attention {
            run_id: None,
            task_id: hold.anchor,
            pid: None,
            ask_id: None,
            reason_category: Some(crate::domain::AskReason::RecoveryFailed),
            status: "open".into(),
            kind: event_kind::GOAL_REVIEW_FAILED.into(),
            last_error: Some(truncate_reason(&format!(
                "goal {}: {}",
                hold.goal_id,
                hold.error.as_deref().unwrap_or("the goal review failed")
            ))),
            last_error_code: Some(ReasonCode::JobFailed),
            next: AttentionNext::GoalReviewByHand,
        });
    }
    // A replaced task whose replacement record is empty, names a task that
    // is not there or loops is never resolved (ADR-t1521-2 decision 5): its
    // goal neither starts a review nor closes as achieved, and a recurring
    // key it holds stays held; a person decides the goal. It is shown on
    // the broken replaced task, with the goal it holds back.
    for (goal, unresolved) in queue.unresolved_replacements()? {
        attention.push(Attention {
            run_id: None,
            task_id: Some(unresolved.task),
            pid: None,
            ask_id: None,
            reason_category: None,
            status: "open".into(),
            kind: crate::domain::replan_apply::REPLACEMENT_UNRESOLVED.into(),
            last_error: Some(truncate_reason(&match goal {
                Some(goal) => format!(
                    "goal {goal}: replaced task {}: {}",
                    unresolved.task, unresolved.fault
                ),
                None => format!("replaced task {}: {}", unresolved.task, unresolved.fault),
            })),
            last_error_code: None,
            next: AttentionNext::GoalReviewByHand,
        });
    }
    // A goal whose own work is done but whose goal review waits for the
    // membership of its follow-ups nothing else shows waits for a planning
    // request the inbox records (task 1660, ADR-t1504-2 decision 8); it is
    // shown on the first of those follow-ups.
    for goal in queue.goal_follow_ups()? {
        if goal_holds.iter().any(|hold| hold.goal_id == goal.goal) {
            continue;
        }
        let unsettled = crate::domain::follow_up::unshown_unsettled(&goal);
        let Some(&(anchor, _)) = unsettled.first() else {
            continue;
        };
        attention.push(Attention {
            run_id: None,
            task_id: Some(anchor),
            pid: None,
            ask_id: None,
            reason_category: None,
            status: "open".into(),
            kind: crate::domain::follow_up::GOAL_FOLLOW_UPS_UNSETTLED.into(),
            last_error: Some(truncate_reason(
                &crate::domain::follow_up::unsettled_summary(goal.goal, &unsettled),
            )),
            last_error_code: None,
            next: AttentionNext::DecideFollowUps { goal_id: goal.goal },
        });
    }
    // Use the supervisor's selection: the recorded delivery decision survives
    // changes to the goal after the answer. Closed asks are excluded by the store.
    let goal_answers = queue.goal_answers()?;
    let correction_answers = queue.correction_answers()?;
    for ask in asks.iter().cloned() {
        let (status, kind, next) = if ask.is_open() {
            (
                "open",
                event_kind::ASK_OPENED,
                AttentionNext::AnswerAsk { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::WorkerQuestion
            && let Some(run_id) = ask.run_id.as_ref()
        {
            let (kind, next) = worker_answer_attention(
                ask.id,
                ask.created_at,
                queue.run(run_id)?.status(),
                &queue.run_events(run_id)?,
                queue.run_lease(run_id)?.as_ref(),
                now,
                control,
            );
            ("answered", kind, next)
        } else if ask.kind == AskKind::Decide
            && ask.asked_by == TRIAGE_ASKER
            && let Some(run_id) = ask.run_id.as_ref()
            && matches!(
                queue.run(run_id)?.status(),
                RunStatus::Failed | RunStatus::Interrupted
            )
            && ask
                .answer
                .as_deref()
                .is_some_and(|answer| ask.options.iter().any(|option| option == answer.trim()))
        {
            // The supervisor retries, resumes or cancels the recovered run,
            // or hands another option of the ask back to the recovery job.
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::UpdateFailed
            && ask
                .answer
                .as_deref()
                .is_some_and(|answer| UPDATE_FAILED_OPTIONS.contains(&answer.trim()))
            && update_failure_applied(queue, registrations, &ask, now, control)?
        {
            // The supervisor that updates its binary retries or leaves the
            // update (ADR-0045 decision 17).
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::ApproveRelease
            && ask
                .answer
                .as_deref()
                .is_some_and(|answer| APPROVE_RELEASE_OPTIONS.contains(&answer.trim()))
            && registrations.iter().any(|registration| {
                registration.applies_releases(
                    now,
                    |pid| control.alive(pid),
                    queue.release_updates_on(),
                )
            })
        {
            // A supervisor of a release build installs or skips the
            // release (ADR-t618-1 decision 4).
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::QueueHold
            && ask
                .answer
                .as_deref()
                .is_some_and(|answer| crate::domain::queue_hold::applies(&ask.options, answer))
        {
            // The supervisor ends the hold, tells the held sessions to go
            // on or gives them up, as the option says (task 437; the
            // disk's `done` / `wait`, task 377).
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::ApprovePlan && queue.applies_plan_answer(&ask)? {
            // The supervisor readies, sends back or cancels the proposal
            // (ADR-0041 decision 11).
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::ApproveGoal
            && goal_answers.iter().any(|answer| answer.id == ask.id)
        {
            // The supervisor closes the goal, registers its gaps or leaves
            // it open (ADR-0047 decision 43).
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::CorrectGoal
            && correction_answers.iter().any(|answer| answer.id == ask.id)
        {
            // The supervisor reopens the goal, records that its achieved
            // verdict was wrong, or keeps it (ADR-t1504-2 decision 9).
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::PlannerQuestion
            && queue.planner_answer_route(&ask)? != PlannerAnswerRoute::Person
        {
            // The supervisor delivers the answer to the runtime's planner
            // that works on the task as its next turn, or opens one with it
            // (ADR-0041 decision 13, ADR-t1433-2).
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::DeliveringAnswer { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::PlannerQuestion {
            // A planner asked it: the inbox plans the draft it is about with
            // the answer, or reads anything else (never "to the worker").
            let draft = match AttentionNext::planner_answer_task(&ask) {
                Some(task) if is_draft(queue, task)? => Some(task),
                _ => None,
            };
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::of_person_planner_answer(ask.id, draft),
            )
        } else if ask.kind == AskKind::Stalled
            && ask.answer.as_deref().map(str::trim) == Some("wait")
            && let Some(run_id) = ask.run_id.as_ref()
            && queue.run(run_id)?.status() == RunStatus::Running
            && queue
                .run_lease(run_id)?
                .is_some_and(|lease| !lease_is_stale(&lease, now, control))
        {
            // The supervisor watching the session counts its idle again
            // and closes the ask (ADR-0043 decision 1).
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::ApproveLanding
            && let Some(run_id) = ask.run_id.as_ref()
            && queue.run(run_id)?.status() == RunStatus::AwaitingIntegration
            && ask
                .answer
                .as_deref()
                .is_some_and(|answer| LandingAnswer::accepted(answer, &ask.options).is_some())
        {
            // The supervisor lands, sends back, cancels or reviews again
            // the run itself (ADR-t2521-1).
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::ApproveClose
            && let Some(run_id) = ask.run_id.as_ref()
            && queue.run(run_id)?.status() == RunStatus::Validating
            && ask.answer.as_deref().is_some_and(|answer| {
                crate::domain::satisfied::SatisfiedAnswer::parse(answer).is_some()
            })
        {
            // The supervisor closes, retries or cancels the claim itself
            // (ADR-t1818-1).
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else {
            (
                "answered",
                event_kind::ASK_ANSWERED,
                AttentionNext::ReadAnswer { ask_id: ask.id },
            )
        };
        attention.push(Attention {
            run_id: ask.run_id,
            task_id: ask.task_id,
            pid: None,
            ask_id: Some(ask.id),
            reason_category: Some(ask.reason_category),
            status: status.into(),
            kind: kind.into(),
            last_error: None,
            last_error_code: None,
            next,
        });
    }
    Ok(attention)
}

/// The attention kind and next step of the answered `worker_question`
/// `ask_id`, created at `ask_created_at` (unix seconds), of a run in
/// `status` with `events`, whose lease, if any, is `lease` at `now`.
/// A running worker's answer is the runtime's with or without a live lease,
/// as `answer` records it: the supervisor that adopts the run delivers it,
/// so sending it by hand would deliver it twice. A revise or resume session
/// takes it only under a live lease (task 584); a failed send is the
/// inbox's in either case.
fn worker_answer_attention(
    ask_id: AskId,
    ask_created_at: i64,
    status: RunStatus,
    events: &[RunEvent],
    lease: Option<&RunLease>,
    now: i64,
    control: &dyn ProcessControl,
) -> (&'static str, AttentionNext) {
    let failed = events.iter().any(|e| {
        e.kind == EventKind::AskDeliveryFailed
            && e.payload
                .get("ask_id")
                .and_then(Value::as_i64)
                .map(AskId::new)
                == Some(ask_id)
    });
    if failed {
        (
            event_kind::ASK_DELIVERY_FAILED,
            AttentionNext::DeliverAnswer { ask_id },
        )
    } else if status == RunStatus::Running
        || (session_takes_answers(status, events, ask_created_at)
            && lease.is_some_and(|lease| !lease_is_stale(lease, now, control)))
    {
        (
            event_kind::ASK_ANSWERED,
            AttentionNext::DeliveringAnswer { ask_id },
        )
    } else {
        (
            event_kind::ASK_ANSWERED,
            AttentionNext::DeliverAnswer { ask_id },
        )
    }
}

/// The standing row of the landing recheck's command failing on main's
/// tree alone (ADR-t2141-1), given the latest of
/// [`crate::domain::recheck::LANDING_RECHECK_MAIN_KINDS`] on the queue
/// (`latest`): while it is `landing_recheck_main_failed`, the runs that
/// failed wait to land instead of being resumed, so `status` keeps the
/// row (its `last_error` the log and main) even after `watch` told the
/// event once; `landing_recheck_main_passed`, or neither, has
/// none.
fn landing_recheck_main_attention(latest: Option<&RunEvent>) -> Option<Attention> {
    let event = latest.filter(|event| event.kind == EventKind::LandingRecheckMainFailed)?;
    Some(Attention {
        run_id: None,
        task_id: None,
        pid: None,
        ask_id: None,
        reason_category: None,
        status: "failed".into(),
        kind: event.kind.to_string(),
        last_error: Some(truncate_reason(&format!(
            "{} (main {})",
            event.payload["log_path"].as_str().unwrap_or_default(),
            event.payload["main"].as_str().unwrap_or_default()
        ))),
        last_error_code: None,
        next: AttentionNext::FixMainOrRecheck,
    })
}

/// Whether `task` is a draft, read from the task store every context reads.
pub(super) fn is_draft(queue: &(impl TaskReads + ?Sized), task: TaskId) -> Result<bool> {
    let page = queue.list(&crate::application::TaskQuery {
        status: crate::application::StatusFilter::Only(vec![TaskStatus::Draft]),
        goal_id: None,
        limit: 1,
        before: Some(task),
        full: false,
    })?;
    Ok(page.tasks.first().is_some_and(|item| item.id == task))
}

/// What a run awaiting integration that nobody leases (`seen`, with its
/// `events`) waits for when the runtime, not a person, moves it on: the
/// event that set it waiting and its attention's next. A `land` answer or
/// a landing recovered to land again queues it to land; a landing
/// recovered to be reviewed, or a `retry_review` answer, queues its
/// review; and one the supervisor takes up again
/// ([`crate::domain::stranded_landing::judge`], ADR-t3049-1) is queued to
/// land as well, judged with this binary's build, the supervisors' when
/// both are the installed one. `None` when it waits for a person.
fn runtime_wait(
    events: &[RunEvent],
    seen: crate::domain::stranded_landing::Seen,
) -> Option<(&'static str, AttentionNext)> {
    use crate::domain::stranded_landing::{Stranded, judge};
    let history = RunHistory::from_events(events);
    if history.queued_to_land().is_some() {
        Some((event_kind::LANDING_QUEUED, AttentionNext::QueuedToLand))
    } else if history.recovered_landing().is_some() {
        Some((event_kind::RUN_RECOVERED, AttentionNext::Reviewing))
    } else if history.queued_review_retry().is_some() {
        Some((event_kind::REVIEW_RETRY_QUEUED, AttentionNext::Reviewing))
    } else if judge(seen, events, Some(crate::VERSION)) == Stranded::TakeUp {
        Some((event_kind::LEASE_RELEASED, AttentionNext::QueuedToLand))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::stranded_landing::Seen;
    use crate::domain::{EventId, TaskId};

    fn event(id: i64, kind: EventKind, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.as_str().into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    /// Pid 1 is alive, every other pid is dead.
    struct OnlyOne;

    impl ProcessControl for OnlyOne {
        fn alive(&self, pid: u32) -> bool {
            pid == 1
        }
        fn terminate(&self, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
        fn interrupt(&self, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
        fn kill(&self, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// A running run's answer is the runtime's to deliver with no lease, a
    /// dead holder's or a silent one's, as `answer` records it, unless its
    /// send failed; a revising run's needs a lease that is not stale.
    #[test]
    fn a_running_runs_answer_is_the_runtimes_whatever_its_lease() {
        let now = 1_000;
        let ask = AskId::new(7);
        let lease = |pid, heartbeat_at| RunLease {
            run_id: RunId::new("r").unwrap(),
            token: LeaseToken::new("a"),
            pid,
            heartbeat_at,
        };
        let fresh = lease(1, now);
        let dead = lease(2, now);
        let silent = lease(1, now - HEARTBEAT_TIMEOUT_SECS - 1);
        let revise = event(1, EventKind::ReviseRequested, json!({"sent_at": 0}));
        let failed = event(2, EventKind::AskDeliveryFailed, json!({"ask_id": 7}));
        let other_failed = event(2, EventKind::AskDeliveryFailed, json!({"ask_id": 8}));
        let delivering = (
            event_kind::ASK_ANSWERED,
            AttentionNext::DeliveringAnswer { ask_id: ask },
        );
        let to_send = (
            event_kind::ASK_ANSWERED,
            AttentionNext::DeliverAnswer { ask_id: ask },
        );
        let judge = |status, events: &[RunEvent], lease: Option<&RunLease>| {
            worker_answer_attention(ask, now, status, events, lease, now, &OnlyOne)
        };
        let running = RunStatus::Running;
        for held in [None, Some(&fresh), Some(&dead), Some(&silent)] {
            assert_eq!(judge(running, &[], held), delivering, "{held:?}");
            assert_eq!(
                judge(running, std::slice::from_ref(&other_failed), held),
                delivering,
                "{held:?}"
            );
            assert_eq!(
                judge(running, std::slice::from_ref(&failed), held),
                (
                    event_kind::ASK_DELIVERY_FAILED,
                    AttentionNext::DeliverAnswer { ask_id: ask }
                ),
                "{held:?}"
            );
        }
        let awaiting = RunStatus::AwaitingIntegration;
        let revising = [revise];
        assert_eq!(judge(awaiting, &revising, Some(&fresh)), delivering);
        assert_eq!(judge(awaiting, &revising, Some(&dead)), to_send);
        assert_eq!(judge(awaiting, &revising, Some(&silent)), to_send);
        assert_eq!(judge(awaiting, &revising, None), to_send);
        assert_eq!(judge(awaiting, &[], Some(&fresh)), to_send);
        assert_eq!(judge(RunStatus::Failed, &[], Some(&fresh)), to_send);
    }

    /// The latest of the recheck's main kinds being a failure keeps one
    /// row whose `last_error` names the log and main; a pass after it, or
    /// neither event, keeps none.
    #[test]
    fn a_failure_on_main_alone_stands_until_main_passes() {
        let failed = event(
            1,
            EventKind::LandingRecheckMainFailed,
            json!({"main": "abc", "command": "cargo test", "exit_code": 101,
                   "class": "test_failure", "log_path": "/q/recheck/main-abc.log",
                   "failed_runs": 2}),
        );
        let row = landing_recheck_main_attention(Some(&failed)).unwrap();
        assert_eq!(row.kind, "landing_recheck_main_failed");
        assert_eq!(row.next, AttentionNext::FixMainOrRecheck);
        assert_eq!(row.next.to_string(), "fix main or the recheck environment");
        let last_error = row.last_error.unwrap();
        assert!(
            last_error.contains("/q/recheck/main-abc.log"),
            "{last_error}"
        );
        assert!(last_error.contains("abc"), "{last_error}");
        let passed = event(
            2,
            EventKind::LandingRecheckMainPassed,
            json!({"main": "def", "command": "cargo test", "log_path": "/q/recheck/main-def.log"}),
        );
        assert_eq!(landing_recheck_main_attention(Some(&passed)), None);
        assert_eq!(landing_recheck_main_attention(None), None);
    }

    /// A passed run whose lease was given back is queued to land in the
    /// attention only while the supervisor would take it up: not while its
    /// task is held or replanned, nor with an ask of it open, nor once a
    /// take-up of it failed, when it is a person's (`review and integrate`,
    /// or the ask) as the take-up judges.
    #[test]
    fn a_given_back_run_is_queued_to_land_only_while_the_supervisor_takes_it_up() {
        let events = [
            event(1, EventKind::ReviewFinished, json!({"verdict": "pass"})),
            event(2, EventKind::ExitRequested, json!({})),
            event(3, EventKind::LandingQueued, json!({"via": "exit"})),
            event(4, EventKind::LeaseReleased, json!({"reason": "finished"})),
        ];
        let seen = Seen {
            status: RunStatus::AwaitingIntegration,
            leased: false,
            ask_open: false,
            fenced: false,
            task: TaskStatus::InProgress,
        };
        assert_eq!(
            runtime_wait(&events, seen),
            Some((event_kind::LEASE_RELEASED, AttentionNext::QueuedToLand))
        );
        let fenced = Seen {
            fenced: true,
            ..seen
        };
        assert_eq!(runtime_wait(&events, fenced), None);
        let asked = Seen {
            ask_open: true,
            ..seen
        };
        assert_eq!(runtime_wait(&events, asked), None);
        // A take-up that failed let go of it with a runtime error: it is a
        // person's (`review and integrate`) and not queued to land again.
        let mut given_up = events.to_vec();
        given_up.push(event(
            5,
            EventKind::RuntimeError,
            json!({"message": "run r could not be taken up: x", "lease_released": true}),
        ));
        assert_eq!(runtime_wait(&given_up, seen), None);
        // A `land` answer queues it whatever else the take-up judges.
        let approved = [
            events[0].clone(),
            event(
                5,
                EventKind::IntegrationApproved,
                json!({"ask_id": 9, "push": true}),
            ),
            event(
                6,
                EventKind::LandingQueued,
                json!({"via": "approve", "ask_id": 9}),
            ),
        ];
        assert_eq!(
            runtime_wait(&approved, seen),
            Some((event_kind::LANDING_QUEUED, AttentionNext::QueuedToLand))
        );
    }
}
