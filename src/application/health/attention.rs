//! What waits for a person (`attention`) and the events as the inbox reads
//! them (observation and analysis).

use super::*;

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
    if event.kind == crate::domain::event_kind::EventKind::AskOpened.as_str() {
        // The asking AI's recommendation and confidence (ADR-t451-1
        // decision 1), null without them and for an ask opened before.
        for key in ["recommendation", "confidence"] {
            object.insert(key.into(), payload.get(key).cloned().unwrap_or(Value::Null));
        }
    }
    if crate::domain::UPDATE_EVENT_KINDS.contains(&event.kind.as_str()) {
        // Which build a step of the automatic update is about, and where
        // a failure happened.
        for key in ["commit", "release", "version", "stage", "plugin"] {
            if let Some(value) = payload.get(key) {
                object.insert(key.into(), value.clone());
            }
        }
    }
    if event.kind == crate::domain::event_kind::THROUGHPUT_REVIEW_REPORTED {
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
    if event.kind == crate::domain::event_kind::THROUGHPUT_REVIEW_FINISHED {
        // Which review ended how, and the directory whose `output.out`
        // and `output.err` say why a failed one did (task 1099).
        for key in ["mode", "period", "outcome", "dir"] {
            if let Some(value) = payload.get(key) {
                object.insert(key.into(), value.clone());
            }
        }
    }
    if event.kind == crate::domain::event_kind::OBSERVE_FINISHED {
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

/// What waits for a person now: stale or missing supervisors first,
/// then the latest run of every `in_progress` task that rests where only a
/// person or the supervisor moves it on or that is unfinished without a lease,
/// then every landed run whose push of `main` failed with no successful push
/// since, then every ask nobody closed: an open one as `ask_opened` for the
/// inbox, an answered one as `ask_answered` for the person to act on
/// through the inbox (ADR-0022, ADR-0024 decision 6).
/// `kind` is the event that brought the run there (for a run without
/// a lease, its latest `runtime_error`).
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

/// What waits for a person. `ci_named_jobs` is the jobs `required_jobs`
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
         + RunCoordination
         + SupervisorRegistry
         + RunLog
         + SessionRegistry
         + TaskStore
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
        // A run a `land` answer approved, or one recovered from a landing
        // it may land again, waits for the supervisor to land it (tasks 949
        // and 1118), not for a person's `integrate`; one recovered from a
        // landing it may not land again waits for the supervisor's review.
        if run.status() == RunStatus::AwaitingIntegration && !leased {
            let history = RunHistory::from_events(&events);
            let waits = if history.queued_to_land().is_some() {
                Some((event_kind::LANDING_QUEUED, AttentionNext::QueuedToLand))
            } else if history.recovered_landing().is_some() {
                Some((event_kind::RUN_RECOVERED, AttentionNext::Reviewing))
            } else {
                None
            };
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
                kind: event.kind.clone(),
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
                kind: event.kind.clone(),
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
                kind: failed.kind.clone(),
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
    ])? && event.kind == crate::domain::sccache::RESTART_FAILED
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
            kind: event.kind,
            last_error: event.payload["error"].as_str().map(truncate_reason),
            last_error_code: None,
            next: AttentionNext::CheckSccache,
        });
    }
    // A program [run.env] names that the supervisor did not find stops its
    // claims and landings until a person installs it or takes it out of
    // dagq.toml (ADR-0049 decision 9).
    if let Some(event) = queue.latest_queue_event(&RUN_ENV_PROGRAM_KINDS)?
        && event.kind == RUN_ENV_PROGRAM_MISSING
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
    // The CI the supervisor watches cannot be read: its claims and
    // landings wait until a person installs or logs in `gh`, or fixes
    // `dagq.toml` (ADR-t1920-1 decision 2).
    if let Some(event) =
        queue.latest_queue_event(&crate::domain::ci_watch::CI_WATCH_ACCESS_KINDS)?
        && event.kind == crate::domain::ci_watch::CI_WATCH_UNAVAILABLE
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
        && event.kind == KPI_PUSH_ABANDONED
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
        && crate::domain::queue_service::attention_stands(Some(event.kind.as_str()))
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
        if hold.kind == event_kind::PLAN_REVIEW_FAILED
            && held(HoldJob::PlanReview(hold.proposal_id))
        {
            continue;
        }
        let (status, next) = match hold.kind {
            event_kind::PLAN_REVIEW_FAILED => ("submitted", AttentionNext::PlanReviewByHand),
            _ => ("revising", AttentionNext::CheckPlanner),
        };
        attention.push(Attention {
            run_id: None,
            task_id: Some(hold.anchor),
            pid: None,
            ask_id: None,
            reason_category: None,
            status: status.into(),
            kind: hold.kind.into(),
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
            // The supervisor holding a running worker's lease types the
            // answer into its terminal, also while it revises (task 238); a
            // failed send, a run no longer running or one nobody supervises
            // leaves it to the inbox.
            let failed = queue.run_events(run_id)?.iter().any(|e| {
                e.kind == event_kind::ASK_DELIVERY_FAILED
                    && e.payload
                        .get("ask_id")
                        .and_then(Value::as_i64)
                        .map(AskId::new)
                        == Some(ask.id)
            });
            if failed {
                (
                    "answered",
                    event_kind::ASK_DELIVERY_FAILED,
                    AttentionNext::DeliverAnswer { ask_id: ask.id },
                )
            } else if session_takes_answers(
                queue.run(run_id)?.status(),
                &queue.run_events(run_id)?,
                ask.created_at,
            ) && queue
                .run_lease(run_id)?
                .is_some_and(|lease| !lease_is_stale(&lease, now, control))
            {
                (
                    "answered",
                    event_kind::ASK_ANSWERED,
                    AttentionNext::DeliveringAnswer { ask_id: ask.id },
                )
            } else {
                (
                    "answered",
                    event_kind::ASK_ANSWERED,
                    AttentionNext::DeliverAnswer { ask_id: ask.id },
                )
            }
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
                .is_some_and(|answer| LandingAnswer::parse(answer).is_some())
        {
            // The supervisor lands, sends back or cancels the run itself.
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

/// Whether `task` is a draft, read from the task store every context reads.
pub(super) fn is_draft(queue: &(impl TaskStore + ?Sized), task: TaskId) -> Result<bool> {
    let page = queue.list(&crate::application::TaskQuery {
        status: crate::application::StatusFilter::Only(vec![TaskStatus::Draft]),
        goal_id: None,
        limit: 1,
        before: Some(task),
        full: false,
    })?;
    Ok(page.tasks.first().is_some_and(|item| item.id == task))
}
