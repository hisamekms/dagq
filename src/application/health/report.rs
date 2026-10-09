//! `status` and `doctor`: the supervisors' slots and waits, what holds the
//! claims and the unfinished runs (observation and analysis).

use super::*;

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub checked_at: i64,
    pub supervisors: Vec<SupervisorHealth>,
    pub runs: Vec<RunHealth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_env: Option<Value>,
    /// Where each AI actor runs and how far that holds it.
    pub actors: Vec<ActorExecution>,
}

/// Characters of an ask's question `status` keeps before `…`.
pub(super) const ASK_QUESTION_CHARS: usize = 200;

/// A reason or error is cut to this many characters in the attention.
pub(super) const REASON_CHARS: usize = 300;

/// `status --role`: registered supervisors, lease holders and the
/// unfinished runs with their leases, without inspecting the runs'
/// processes, plus what waits for a person narrowed to what `role` acts on
/// (`attention`, ADR-0022; `None` is all of it), every open ask with its
/// question cut to 200 characters, the proposals waiting for plan review
/// or being revised (ADR-0041 decision 7) in plan review's order, and the
/// newest event id (`cursor`) to `watch` from (ADR-0016), with `actors`,
/// each AI actor's backend and enforcement ([`actor_executions`]), the
/// worker's per provider once a supervisor can run Codex. `ci_named_jobs`
/// is what [`attention`] reads `ci_jobs_missing` against.
pub fn status(
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
    control: &dyn ProcessControl,
    clock: &dyn Clock,
    role: Option<SessionRole>,
    ci_named_jobs: Option<&[String]>,
) -> Result<Value> {
    // Read before the state it describes, so a transition in between is
    // seen again by `watch --after cursor` rather than missed.
    let cursor = queue.latest_event_id()?;
    let now = clock.now();
    let registrations = queue.supervisors()?;
    let leases = queue.run_leases()?;
    let runs = listed_runs(queue, &leases)?
        .into_iter()
        .map(|run| {
            let lease = leases
                .iter()
                .find(|l| l.run_id == *run.id())
                .map(|l| lease_health(l, now, control));
            let events = queue.run_events(run.id())?;
            let mut entry = json!({
                "run_id": run.id(),
                "task_id": run.task_id(),
                "status": run.status(),
                "workspace_id": run.workspace_id(),
                "worktree_path": run.worktree_path(),
                "lease": lease,
                // What it does now and how it uses its slot (goal 98).
                "progress": Progress::of(run.status(), progress_lease(lease.as_ref()), &events, now),
            });
            let events = queue.run_events(run.id())?;
            // Why it waits or failed (ADR-0034), for a run that has an error.
            if let Some(code) = reason::run_error_code(&run, &events) {
                entry["last_error_code"] = json!(code);
            }
            // A session in the background: its wrapper's pid and its log
            // (`run log`, ADR-t1404-1 decision 6).
            if let Some(session) =
                crate::domain::background_wrapper::current_background_session(&events)
            {
                entry["background"] = json!(session);
            }
            Ok(entry)
        })
        .collect::<Result<Vec<_>>>()?;
    let asks = queue
        .asks(AskQuery {
            open: true,
            ..Default::default()
        })?
        .into_iter()
        .map(|ask| {
            let mut entry = json!({
                "id": ask.id,
                "kind": ask.kind,
                "question": truncate(&ask.question, ASK_QUESTION_CHARS)
                    .unwrap_or(ask.question),
                "task_id": ask.task_id,
                "run_id": ask.run_id,
                "asked_by": ask.asked_by,
                "reason_category": ask.reason_category,
                "affected": ask.affected,
                "age_secs": now - ask.created_at,
                // The asking AI's recommendation and confidence
                // (ADR-t451-1 decision 1), null without them.
                "recommendation": ask.recommendation,
                "confidence": ask.confidence,
            });
            // What a worker_question left undecided (ADR-t947-2), primary
            // first; absent for the kinds and the asks without topics.
            if !ask.topics.is_empty() {
                entry["topics"] = json!(ask.topics);
            }
            entry
        })
        .collect::<Vec<_>>();
    let (supervisors, waiting) = slots_and_waits(
        queue,
        supervisors(&registrations, &leases, now, control),
        &registrations,
        &leases,
        now,
    )?;
    Ok(json!({
        "checked_at": now,
        "supervisors": supervisors,
        "runs": runs,
        "waiting": waiting,
        "attention": attention(queue, &registrations, now, control, ci_named_jobs)?
            .into_iter()
            .filter(|_| for_role(role))
            .collect::<Vec<_>>(),
        "asks": asks,
        // The claims deferred now on a conflict hotspot (ADR-0069): each
        // task's open `claim_deferred`, with the files and the runs.
        "claim_deferrals": queue
            .latest_task_events(&crate::domain::claim_defer::DEFERRAL_KINDS)?
            .iter()
            .filter_map(crate::domain::claim_defer::OpenDeferral::of)
            .collect::<Vec<_>>(),
        "proposals": queue.proposals(false)?,
        // The latest landing recheck of the waiting runs (ADR-0068
        // decision 6), with when it finished.
        "landing_recheck": queue
            .latest_event_of(recheck::LANDING_RECHECK_FINISHED)?
            .map(|event| {
                let mut payload = event.payload;
                payload["at"] = json!(event.created_at);
                payload
            }),
        // The build identifier this `status` runs, and the automatic update
        // of the supervisors' binary (ADR-0045 decision 17); each
        // supervisor's own build is its `binary_version`.
        "version": crate::VERSION,
        // Where each AI actor runs and how far that holds it (goal 55): on
        // the host, advisory, not a sandbox (ADR-t728-1 decision 6); a
        // Codex worker confined (ADR-t813-3 decision 7).
        "actors": actor_executions(&provider_checks(&registrations, control))?,
        "auto_update": crate::application::update::status(
            &registrations,
            &queue.update_events(20)?,
            control,
            now,
        ),
        "cursor": cursor,
    }))
}

/// Each registered supervisor's `slots` (`used` of `parallel`) and
/// `waiting` (`count` of `limit`, with the `returning` among them), and the
/// runs that wait for a person or for a slot to go back to (ADR-0071
/// decision 12), from the leases and the run events. `used` counts the
/// leased runs that do not wait, landing ones included (ADR-t610-1), less
/// `landing_queue`: those that wait only for their landing turn, up to
/// `parallel` (ADR-t1591-1). A heavy task is claimed while `used` and
/// `landing_queue` together are under `parallel`, a light one (of
/// `[supervisor] light_changes`, with no returning run) while `used` is;
/// `count` is what
/// `--max-waiting` bounds (ADR-0071 (f2)): the runs that wait and those
/// that wait to go back (`state: returning` in `waiting`). A supervisor that holds its claims
/// has `claim_hold`: its latest `claim_held` payload and `since` (task
/// 327); one that holds the landings' verification for the disk has
/// `landing_hold` in the same shape (task 377), and one that holds its
/// claims for the CI watch or a landing branch that does not resolve
/// `ci_watch_hold` / `landing_branch_hold`.
pub(super) fn slots_and_waits(
    queue: &(impl RunLog + ?Sized),
    health: Vec<SupervisorHealth>,
    registrations: &[SupervisorRegistration],
    leases: &[RunLease],
    now: i64,
) -> Result<(Vec<Value>, Vec<Value>)> {
    let mut waiting = Vec::new();
    // Per token: the slots in use and the runs out of them, counted as
    // `--max-waiting` counts them.
    // Per token: the slots in use, the landing queue among them, and the
    // runs out of them.
    let mut counts: HashMap<&str, (usize, usize, WaitCount)> = HashMap::new();
    for lease in leases {
        let run = queue.run(&lease.run_id)?;
        let entry = counts.entry(lease.token.as_str()).or_default();
        let events = queue.run_events(run.id())?;
        // The same wait as the run's `progress.slot` in `runs` ([`crate::domain::run_progress::SlotUse`]).
        let Some(state) = WaitState::of(&events) else {
            entry.0 += 1;
            if crate::domain::run_progress::in_landing_queue(run.status(), &events) {
                entry.1 += 1;
            }
            continue;
        };
        let since = state.since_ms.div_euclid(1000);
        let mut wait = json!({
            "run_id": run.id(),
            "task_id": run.task_id(),
            "asks": state.asks_json(),
            "phase": state.phase,
            "status": run.status(),
            "state": if state.ended.is_some() { "returning" } else { "waiting" },
            "since": since,
            "waited_secs": state.ended.map_or(now, |(ms, _)| ms.div_euclid(1000)) - since,
        });
        if let Some((ms, cause)) = state.ended {
            wait["ended_at"] = json!(ms.div_euclid(1000));
            wait["cause"] = json!(cause.as_str());
        }
        entry.2.add(state.ended.is_some());
        waiting.push(wait);
    }
    // The hold on new claims in progress (task 327), on its supervisor.
    let held = |kinds: crate::domain::claim_hold::HoldKinds| -> Result<Option<RunEvent>> {
        Ok(queue
            .latest_queue_event(&kinds.kinds())?
            .filter(|event| event.kind == kinds.held))
    };
    // Codex held for the workers (ADR-t813-2 decision 6), until its time.
    let provider_hold = queue
        .latest_queue_event(&[
            crate::domain::event_kind::PROVIDER_HELD,
            crate::domain::event_kind::PROVIDER_RELEASED,
        ])?
        .filter(|event| event.kind == crate::domain::event_kind::PROVIDER_HELD);
    // The queue's latest claim, which the claim spacing counts from
    // (ADR-t1479-1).
    let last_claim = queue.latest_event_of(crate::domain::event_kind::RUN_CLAIMED)?;
    let holds = [
        ("claim_hold", held(crate::domain::claim_hold::CLAIMS)?),
        ("landing_hold", held(crate::domain::claim_hold::LANDINGS)?),
        ("provider_hold", provider_hold),
    ];
    // Each supervisor's own holds, by its own latest record.
    let tokens: Vec<&str> = registrations
        .iter()
        .map(|registration| registration.token.as_str())
        .collect();
    let own_holds = [
        (
            "ci_watch_hold",
            crate::domain::ci_watch::CI_WATCH_HOLD,
            own_hold_records(queue, crate::domain::ci_watch::CI_WATCH_HOLD, &tokens)?,
        ),
        (
            "landing_branch_hold",
            crate::domain::landing_branch::LANDING_BRANCH_HOLD,
            own_hold_records(
                queue,
                crate::domain::landing_branch::LANDING_BRANCH_HOLD,
                &tokens,
            )?,
        ),
    ];
    let supervisors = health
        .into_iter()
        .enumerate()
        .map(|(index, health)| {
            let mut value = serde_json::to_value(health)?;
            if let Some(registration) = registrations.get(index) {
                let (held, queued, count) = counts
                    .get(registration.token.as_str())
                    .copied()
                    .unwrap_or_default();
                // Up to `parallel` of the landing queue are out of the
                // slots (ADR-t1591-1).
                let parallel = usize::try_from(registration.parallel).unwrap_or(0);
                let landing_queue = crate::domain::light_slots::outside_the_slots(queued, parallel);
                value["slots"] = json!({
                    "used": held - landing_queue,
                    "landing_queue": landing_queue,
                    "parallel": registration.parallel,
                    "source": registration.parallel_source,
                });
                value["waiting"] = json!({
                    "count": count.count(),
                    "returning": count.returning,
                    "limit": registration.max_waiting,
                    "source": registration.max_waiting_source,
                });
                if let Some(spacing) = claim_spacing(registration, last_claim.as_ref(), now) {
                    value["claim_spacing"] = spacing;
                }
                for (key, hold) in &holds {
                    if let Some(event) = hold.as_ref().filter(|event| {
                        event.payload.get("supervisor").and_then(Value::as_str)
                            == Some(registration.token.as_str())
                    }) {
                        let mut held = event.payload.clone();
                        held["since"] = json!(event.created_at);
                        value[*key] = held;
                    }
                }
                for (key, kinds, records) in &own_holds {
                    if let Some(event) = own_held(*kinds, records, registration.token.as_str()) {
                        let mut held = event.payload.clone();
                        held["since"] = json!(event.created_at);
                        value[*key] = held;
                    }
                }
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((supervisors, waiting))
}

/// A supervisor's `claim_spacing` in `status` (ADR-t1479-1): the seconds
/// and where they come from, its `--max-load`, whether the spacing is in
/// effect (the load hold on and the seconds above 0), the queue's latest
/// claim and, while the spacing is in effect, when the next claim may be
/// made (`next_claim_at`, with `waiting` while it is ahead of `now`).
/// `None` for a registration of an older binary, which records no spacing.
pub(super) fn claim_spacing(
    registration: &SupervisorRegistration,
    last_claim: Option<&RunEvent>,
    now: i64,
) -> Option<Value> {
    use crate::domain::claim_spacing::{in_effect, next_claim_ms, waits};
    let secs = registration.claim_spacing?;
    let spacing = in_effect(registration.max_load, usize::try_from(secs).ok()?);
    let last_ms =
        last_claim.and_then(|event| crate::domain::stats::timestamp_millis(&event.created_at));
    let next = next_claim_ms(spacing, last_ms);
    let at = |ms: i64| {
        crate::application::timestamp(
            std::time::UNIX_EPOCH
                + std::time::Duration::from_millis(u64::try_from(ms).unwrap_or(0)),
        )
    };
    Some(json!({
        "secs": secs,
        "source": registration.claim_spacing_source,
        "max_load": registration.max_load,
        "in_effect": spacing.is_some(),
        "last_claim_at": last_claim.map(|event| event.created_at.clone()),
        "next_claim_at": next.map(at),
        "waiting": waits(next, now.saturating_mul(1000)),
    }))
}

/// What holds the claims now, as the supervisors recorded it, for
/// `candidates`' `held` (ADR-t1992-1): `no_supervisor` while no registered
/// supervisor is alive with a fresh heartbeat, then the latest record of
/// each hold that is on (`claim_held` for the load, the disk or a queue
/// hold, `run_env_program_missing`, `ci_watch_held`,
/// `landing_branch_unresolved`) of a live
/// supervisor or of none named, then each live
/// supervisor's wait for its claim spacing. A hold no supervisor records
/// is not shown, nor guessed at.
pub fn claim_holds(
    queue: &(impl crate::application::RunLog + crate::application::SupervisorRegistry + ?Sized),
    control: &dyn ProcessControl,
    clock: &dyn Clock,
) -> Result<Vec<Value>> {
    let now = clock.now();
    let registrations = queue.supervisors()?;
    let live: Vec<&SupervisorRegistration> = registrations
        .iter()
        .filter(|registration| {
            !heartbeat_stale(
                control.alive(registration.pid),
                now - registration.heartbeat_at,
            )
        })
        .collect();
    let on = |kinds: &[&str], held: &str| -> Result<Option<RunEvent>> {
        Ok(queue
            .latest_queue_event(kinds)?
            .filter(|event| event.kind == held))
    };
    let claims = crate::domain::claim_hold::CLAIMS;
    let records = [
        on(&claims.kinds(), claims.held.as_str())?,
        on(&RUN_ENV_PROGRAM_KINDS, RUN_ENV_PROGRAM_MISSING)?,
    ];
    let last_claim = queue.latest_event_of(event_kind::RUN_CLAIMED)?;
    let spacing = live
        .iter()
        .filter_map(|registration| {
            claim_spacing(registration, last_claim.as_ref(), now).map(|spacing| {
                let mut spacing = spacing;
                spacing["supervisor"] = json!(registration.token);
                spacing
            })
        })
        .collect::<Vec<_>>();
    // A hold of a supervisor that is gone holds nothing, as `status` and
    // the supervisors read it.
    let of_live = |event: &RunEvent| {
        event
            .payload
            .get("supervisor")
            .and_then(Value::as_str)
            .is_none_or(|token| live.iter().any(|r| r.token.as_str() == token))
    };
    let mut records: Vec<RunEvent> = records.into_iter().flatten().filter(of_live).collect();
    // Each live supervisor's own holds, by its own latest record.
    let tokens: Vec<&str> = live
        .iter()
        .map(|registration| registration.token.as_str())
        .collect();
    for kinds in [
        crate::domain::ci_watch::CI_WATCH_HOLD,
        crate::domain::landing_branch::LANDING_BRANCH_HOLD,
    ] {
        let own = own_hold_records(queue, kinds, &tokens)?;
        records.extend(
            live.iter()
                .filter_map(|registration| own_held(kinds, &own, registration.token.as_str())),
        );
    }
    Ok(held(live.is_empty(), records, spacing))
}

/// Each of `tokens`' latest record of either kind of its own hold
/// ([`OwnHold`](crate::domain::claim_hold::OwnHold)), however many the
/// other supervisors recorded since; one that recorded none has no entry
/// ([`OwnHold::latest_of`](crate::domain::claim_hold::OwnHold::latest_of)
/// picks a supervisor's).
pub(crate) fn own_hold_records(
    queue: &(impl crate::application::RunLog + ?Sized),
    kinds: crate::domain::claim_hold::OwnHold,
    tokens: &[&str],
) -> Result<Vec<RunEvent>> {
    queue.latest_events_by_supervisor(&kinds.kinds(), tokens)
}

/// The latest record of `token`'s own hold of `kinds` among `records`
/// when it holds.
pub(super) fn own_held(
    kinds: crate::domain::claim_hold::OwnHold,
    records: &[RunEvent],
    token: &str,
) -> Option<RunEvent> {
    let latest = crate::domain::claim_hold::OwnHold::latest_of(records, token);
    kinds.holds(latest).then(|| latest.cloned()).flatten()
}

/// The entries of `held` (`reason`, `since` when known, the `record` as
/// is): a record's kind is its reason; a claim spacing is one only while
/// it waits.
pub(super) fn held(
    no_supervisor: bool,
    records: impl IntoIterator<Item = RunEvent>,
    spacing: impl IntoIterator<Item = Value>,
) -> Vec<Value> {
    let none = no_supervisor.then(|| json!({"reason": "no_supervisor"}));
    let records = records.into_iter().map(
        |event| json!({"reason": event.kind, "since": event.created_at, "record": event.payload}),
    );
    let spacing = spacing
        .into_iter()
        .filter(|spacing| spacing["waiting"] == true)
        .map(|spacing| {
            json!({"reason": "claim_spacing", "since": spacing["last_claim_at"],
                   "record": spacing})
        });
    none.into_iter().chain(records).chain(spacing).collect()
}

/// `doctor`: with `full`, every registered supervisor and every unfinished
/// run with its lease, processes and paths; without it, one line's worth
/// per run and per supervisor ([`RunHealth::summary`],
/// [`SupervisorHealth::summary`]). Both add `run_env` when the repository
/// has a `dagq.toml` or the supervisor recorded a check: the programs its
/// `[run.env]` names, where they resolved on the caller's PATH (`run_env`,
/// or why they could not be checked), and the supervisor's latest
/// `run_env_program_missing` / `run_env_program_found` (ADR-0049 decision
/// 9). Both show `actors`, each AI actor's backend and enforcement
/// ([`actor_executions`]: the host, advisory, not sandboxed; a Codex
/// worker confined). Reads only.
pub fn doctor(
    queue: &(impl RunCoordination + SupervisorRegistry + RunLog + ?Sized),
    control: &dyn ProcessControl,
    files: &dyn RunFiles,
    clock: &dyn Clock,
    full: bool,
    run_env: std::result::Result<RunEnvCheck, String>,
) -> Result<Value> {
    let now = clock.now();
    let registrations = queue.supervisors()?;
    let leases = queue.run_leases()?;
    let runs = listed_runs(queue, &leases)?
        .into_iter()
        .map(|run| {
            let processes = queue.processes(run.id())?;
            let lease = leases
                .iter()
                .find(|l| l.run_id == *run.id())
                .map(|l| lease_health(l, now, control));
            let progress = Progress::of(
                run.status(),
                progress_lease(lease.as_ref()),
                &queue.run_events(run.id())?,
                now,
            );
            let mut health = run_health(&run, &processes, lease, now, control, files);
            health.progress = Some(progress);
            Ok(health)
        })
        .collect::<Result<Vec<_>>>()?;
    let supervisors = supervisors(&registrations, &leases, now, control);
    let last = queue
        .latest_queue_event(&RUN_ENV_PROGRAM_KINDS)?
        .map(|event| json!({"kind": event.kind, "created_at": event.created_at, "payload": event.payload}));
    // A repository without dagq.toml, whose supervisor never found a
    // program missing, has nothing to report.
    let run_env = match run_env {
        Ok(check) if !check.config && last.is_none() => None,
        Ok(check) => Some(json!({
            "config": check.config,
            "path": check.path,
            "programs": check.programs,
            "missing": check.missing().len(),
            "supervisor_last": last,
        })),
        Err(error) => Some(json!({"error": error, "supervisor_last": last})),
    };
    if !full {
        let mut summary = json!({
            "checked_at": now,
            "supervisors": supervisors.iter().map(SupervisorHealth::summary).collect::<Vec<_>>(),
            "runs": runs.iter().map(RunHealth::summary).collect::<Vec<_>>(),
            "actors": actor_executions(&provider_checks(&registrations, control))?,
        });
        if let Some(run_env) = run_env {
            summary["run_env"] = run_env;
        }
        return Ok(summary);
    }
    Ok(serde_json::to_value(DoctorReport {
        checked_at: now,
        supervisors,
        runs,
        run_env,
        actors: actor_executions(&provider_checks(&registrations, control))?,
    })?)
}

/// The sections `status` adds for `role` beyond [`status`]'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSections {
    /// The `language` the prompt and the `SessionStart` hook of the inbox
    /// and a planner carry (ADR-t616-2).
    pub language: bool,
    /// What the inbox watches: `inbox_guardrail` (ADR-t1228-2 decision 4),
    /// `inbox_watcher` (ADR-t906-1) and `queue_service` (ADR-t1233-4); a
    /// planner's `status` has none of them.
    pub inbox: bool,
}

/// The sections of `status --role` for `role`; without a role, all but
/// `language`.
pub fn status_sections(role: Option<SessionRole>) -> StatusSections {
    StatusSections {
        language: matches!(role, Some(SessionRole::Inbox | SessionRole::Planner)),
        inbox: matches!(role, None | Some(SessionRole::Inbox)),
    }
}

/// Whether attention is for `role`: all of it is the inbox's
/// ([`crate::domain::ATTENTION_ROLE`]), and without a role everything is
/// shown. The supervisors' health follows the same rule: the person
/// restarts them.
pub fn for_role(role: Option<SessionRole>) -> bool {
    role.is_none_or(|role| role == crate::domain::ATTENTION_ROLE)
}
