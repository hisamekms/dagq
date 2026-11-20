//! `status` and `doctor`: the supervisors' slots and waits, what holds the
//! claims and the unfinished runs (observation and analysis).

use super::*;
use crate::domain::run_progress::{LandingStage, landing_stage};
use crate::{
    application::claim_view,
    domain::{EventKind, event_kind::PlannerStartBlocked},
};

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
/// is what [`attention`] reads `ci_jobs_missing` against, `ci_freshness`
/// what it reads `ci_workflow_stale` against.
pub fn status(
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
    control: &dyn ProcessControl,
    clock: &dyn Clock,
    role: Option<SessionRole>,
    ci_named_jobs: Option<&[String]>,
    ci_freshness: Option<&[crate::domain::ci_watch::freshness::FreshnessWatch]>,
) -> Result<Value> {
    // Read before the state it describes, so a transition in between is
    // seen again by `watch --after cursor` rather than missed.
    let cursor = queue.latest_event_id()?;
    let now = clock.now();
    let registrations = queue.supervisors()?;
    let leases = queue.run_leases()?;
    let shared = crate::application::live_inputs::shared_landing_entries(
        queue,
        control,
        &registrations,
        &leases,
        now,
    )?;
    let (runs, supervisors, waiting) =
        runs_slots_and_waits(queue, control, &registrations, &leases, &shared, now)?;
    let claim_hold = claim_hold_in_place(queue, &registrations, control, now)?;
    let shown_claim_hold = claim_hold.as_ref().map(hold_view).or_else(|| {
        let mut live = supervisors
            .iter()
            .filter(|entry| entry["registered"] == true && entry["stale"] == false);
        let first = live.next()?;
        (live.all(|entry| entry["claim_admission"]["claims_held"] == true))
            .then(|| claim_view::admission_hold(&first["claim_admission"]))
            .flatten()
    });
    // Fetch just the newest relaxation record; both outputs share it.
    let latest_relax =
        queue.latest_queue_event(&crate::domain::disk::RELAX_KINDS.map(EventKind::as_str))?;
    let disk_relax =
        crate::domain::disk::relax_view(latest_relax.as_ref(), now.saturating_mul(1000));
    let graph = queue.graph_input()?;
    let landing_queue = landing_queue_view(queue, &graph, control, now)?;
    let landing_batch = landing_batch_view(queue)?;
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
    Ok(json!({
        "checked_at": now,
        "supervisors": supervisors,
        "runs": runs,
        "waiting": waiting,
        "landing_queue": landing_queue,
        "landing_batch": landing_batch,
        "attention": attention(
            queue,
            &registrations,
            now,
            control,
            ci_named_jobs,
            ci_freshness,
        )?
            .into_iter()
            .chain(claim_hold_attention(claim_hold.as_ref(), graph.candidates.len(), now))
            .chain(disk_relax.as_ref().map(disk_relax_attention))
            .filter(|_| for_role(role))
            .collect::<Vec<_>>(),
        // The queue-wide hold on new claims in place now, whichever
        // supervisor recorded it, null while none holds them.
        "claim_hold": shown_claim_hold,
        // Active terms and provenance for every role, null after expiry or clear.
        "disk_relax": disk_relax,
        "asks": asks,
        // The claims deferred now on a conflict hotspot (ADR-0069): each
        // task's open `claim_deferred`, with the files and the runs.
        "claim_deferrals": queue
            .latest_task_events(&crate::domain::claim_defer::DEFERRAL_KINDS.map(EventKind::as_str))?
            .iter()
            .filter_map(crate::domain::claim_defer::OpenDeferral::of)
            .collect::<Vec<_>>(),
        // The ready tasks the claim order never takes, by why: they are
        // out before any deferral, so `claim_deferrals` misses them.
        "claim_excluded": crate::application::claim_view::ClaimExclusions::of(&graph),
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

/// `status`'s `runs`, `supervisors` and `waiting`. Each listed run's
/// events are read once and shared by its entry in `runs` and its slot or
/// wait in [`slots_and_waits`]; every leased run is listed
/// ([`listed_runs`]).
pub(super) fn runs_slots_and_waits(
    queue: &(impl RunLog + SupervisorReads + ?Sized),
    control: &dyn ProcessControl,
    registrations: &[SupervisorRegistration],
    leases: &[RunLease],
    shared: &HashMap<String, usize>,
    now: i64,
) -> Result<(Vec<Value>, Vec<Value>, Vec<Value>)> {
    let listed = listed_runs(queue, leases)?;
    let mut events = HashMap::with_capacity(listed.len());
    for run in &listed {
        events.insert(run.id().clone(), queue.run_events(run.id())?);
    }
    let runs = listed
        .iter()
        .map(|run| {
            let lease = leases
                .iter()
                .find(|l| l.run_id == *run.id())
                .map(|l| lease_health(l, now, control));
            let events = &events[run.id()];
            let mut entry = json!({
                "run_id": run.id(),
                "task_id": run.task_id(),
                "status": run.status(),
                "workspace_id": run.workspace_id(),
                "worktree_path": run.worktree_path(),
                "lease": lease,
                // What it does now and how it uses its slot (goal 98).
                "progress": Progress::of(run.status(), progress_lease(lease.as_ref()), events, now),
            });
            // Why it waits or failed (ADR-0034), for a run that has an error.
            if let Some(code) = reason::run_error_code(run, events) {
                entry["last_error_code"] = json!(code);
            }
            // A session in the background: its wrapper's pid and its log
            // (`run log`, ADR-t1404-1 decision 6).
            if let Some(session) =
                crate::domain::background_wrapper::current_background_session(events)
            {
                entry["background"] = json!(session);
            }
            entry
        })
        .collect();
    let (supervisors, waiting) = slots_and_waits(
        queue,
        supervisors(registrations, leases, now, control),
        registrations,
        leases,
        &events,
        shared,
        now,
    )?;
    Ok((runs, supervisors, waiting))
}

/// Each registered supervisor's `worker_slots` object has `used`, `parallel`
/// and `source`: usage is the claim/resume/return judgment's worker count
/// from [`crate::domain::light_slots::worker_slots`]. `parallel` is the
/// worker limit and `source` its setting origin (null if not recorded).
/// `landing_lane.integrating` and `e2e` count integration and e2e leases.
/// `queued` (up to `parallel`) and `overflow` (counted in `used`) split
/// the backpressure inputs: local landing-turn waits and the common
/// queue count read with the supervisor's recorded gates. Eligible
/// common entries may include an e2e lease, still in the queue during e2e.
/// The top-level `landing_queue` lists individual entries. Human waits and
/// returning runs count in `waiting`.
/// Only registered tokens have these capacity objects; unregistered leases
/// do not occupy a supervisor's worker slots. Runs come from leases and
/// their events (`events` by run, read here for a leased run not in it).
/// `count` is what
/// `--max-waiting` bounds (ADR-0071 (f2)): the runs that wait and those
/// that wait to go back (`state: returning` in `waiting`), with `limit`
/// and `source` the registration's `max_waiting` and where it comes from.
/// A wait adds no attention: its asks are the attention. A supervisor that holds its claims
/// has `claim_hold`: its start record with current disk state and `since` (task
/// 327); one that holds the landings' verification for the disk has
/// `landing_hold` in the same shape (task 377), and one that holds its
/// claims for the CI watch or a landing branch that does not resolve
/// `ci_watch_hold`, `landing_branch_hold` and `repository_config_hold`.
pub(super) fn slots_and_waits(
    queue: &(impl RunLog + SupervisorReads + ?Sized),
    health: Vec<SupervisorHealth>,
    registrations: &[SupervisorRegistration],
    leases: &[RunLease],
    events: &HashMap<RunId, Vec<RunEvent>>,
    shared: &HashMap<String, usize>,
    now: i64,
) -> Result<(Vec<Value>, Vec<Value>)> {
    let mut waiting = Vec::new();
    #[derive(Default)]
    struct Counts {
        workers: usize,
        integrating: usize,
        e2e: usize,
        queued: usize,
        waiting: WaitCount,
    }
    let mut counts: HashMap<&str, Counts> = HashMap::new();
    for lease in leases {
        let run = queue.run(&lease.run_id)?;
        let entry = counts.entry(lease.token.as_str()).or_default();
        let read;
        let events = match events.get(run.id()) {
            Some(events) => events,
            None => {
                read = queue.run_events(run.id())?;
                &read
            }
        };
        // The same wait as the run's `progress.slot` in `runs` ([`crate::domain::run_progress::SlotUse`]).
        let Some(state) = WaitState::of(events) else {
            match landing_stage(run.status(), events) {
                None => entry.workers += 1,
                Some(LandingStage::Integrating) => entry.integrating += 1,
                Some(LandingStage::E2e) => entry.e2e += 1,
                Some(LandingStage::Queued) => entry.queued += 1,
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
        entry.waiting.add(state.ended.is_some());
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
        .filter(|event| event.kind == EventKind::ProviderHeld);
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
            "repository_config_hold",
            crate::domain::claim_hold::REPOSITORY_CONFIG_HOLD,
            own_hold_records(
                queue,
                crate::domain::claim_hold::REPOSITORY_CONFIG_HOLD,
                &tokens,
            )?,
        ),
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
    let live_tokens: Vec<_> = registrations
        .iter()
        .zip(&health)
        .filter(|(_, health)| !health.stale)
        .map(|(registration, _)| registration.token.as_str())
        .collect();
    let admission = claim_view::admission_records(queue, &live_tokens)?;
    let supervisors = health
        .into_iter()
        .enumerate()
        .map(|(index, health)| {
            let live = !health.stale;
            let mut value = serde_json::to_value(health)?;
            if let Some(registration) = registrations.get(index) {
                let token = registration.token.as_str();
                let count = counts.remove(token).unwrap_or_default();
                let queued = count.queued + shared.get(token).copied().unwrap_or(0);
                let parallel = usize::try_from(registration.parallel).unwrap_or(0);
                let outside = queued.min(parallel);
                value["worker_slots"] = json!({
                    "used": crate::domain::light_slots::worker_slots(count.workers, queued, parallel),
                    "parallel": registration.parallel,
                    "source": registration.parallel_source,
                });
                value["landing_lane"] = json!({
                    "integrating": count.integrating,
                    "e2e": count.e2e,
                    "queued": outside,
                    "overflow": queued - outside,
                });
                value["waiting"] = json!({
                    "count": count.waiting.count(),
                    "returning": count.waiting.returning,
                    "limit": registration.max_waiting,
                    "source": registration.max_waiting_source,
                });
                let decision = admission
                    .iter()
                    .find(|event| event.payload["supervisor"] == registration.token.as_str());
                if let Some(decision) = decision {
                    let view = claim_view::admission_view(decision);
                    if let Some(hold) = claim_view::admission_hold(&view) {
                        value["claim_hold"] = hold;
                    }
                    value["claim_admission"] = view;
                }
                if let Some(mut spacing) = claim_spacing(registration, last_claim.as_ref(), now) {
                    if let Some(decision) = decision {
                        claim_view::show_spacing_decision(&mut spacing, decision);
                    }
                    value["claim_spacing"] = spacing;
                }
                if let Some(classes) = claim_classes(registration) {
                    value["claim_classes"] = classes;
                }
                for (key, hold) in &holds {
                    if let Some(event) = hold.as_ref().filter(|event| {
                        event.payload.get("supervisor").and_then(Value::as_str)
                            == Some(registration.token.as_str())
                    }) {
                        let mut event = event.clone();
                        if *key == "claim_hold" && live {
                            refresh_disk_hold(queue, &mut event)?;
                        }
                        value[*key] = hold_view(&event);
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

/// The queue-wide hold on new claims in place at `now`: the queue's latest
/// `claim_held` / `claim_resumed` when it is a `claim_held` of a registered
/// supervisor alive with a fresh heartbeat, the rule the supervisors record
/// it by ([`crate::domain::claim_hold::in_place`]). Reads those two kinds'
/// latest event and the live holder's latest disk state.
pub(crate) fn claim_hold_in_place(
    queue: &(impl RunLog + SupervisorReads + ?Sized),
    registrations: &[SupervisorRegistration],
    control: &dyn ProcessControl,
    now: i64,
) -> Result<Option<RunEvent>> {
    let claims = crate::domain::claim_hold::CLAIMS;
    let last = queue.latest_queue_event(&claims.kinds())?;
    let live = |holder: &str| {
        registrations.iter().any(|registration| {
            registration.token.as_str() == holder && is_live(registration, control, now)
        })
    };
    let mut held = crate::domain::claim_hold::in_place(claims, last.as_ref(), live).cloned();
    if let Some(event) = held.as_mut() {
        refresh_disk_hold(queue, event)?;
    }
    Ok(held)
}

/// Attach current state only to a claim hold owned by a live registration.
/// Event identity/time still describe when the hold began. Older binaries
/// with no state keep their event snapshot as a compatibility fallback.
fn refresh_disk_hold(queue: &(impl SupervisorReads + ?Sized), event: &mut RunEvent) -> Result<()> {
    if event.payload["reason"] == "disk_space"
        && let Some(token) = event.payload["supervisor"].as_str()
        && let Some(current) =
            queue.supervisor_disk_status(&crate::domain::LeaseToken::new(token))?
    {
        crate::application::claim_view::refresh_disk_hold(event, current);
    }
    Ok(())
}

/// A hold's record as `status` shows it: its payload (`reason`, `value`,
/// `threshold`, `message`, `supervisor`, the disk's `disk`) and when it
/// started (`since`).
fn hold_view(event: &RunEvent) -> Value {
    let mut held = event.payload.clone();
    held["since"] = json!(event.created_at);
    held
}

/// The attention of the claims' hold in place (`held`) at `now`, when it is
/// one ([`crate::domain::claim_hold::disk_hold_attention`]): `candidates`
/// ready tasks wait to be claimed.
pub(super) fn claim_hold_attention(
    held: Option<&RunEvent>,
    candidates: usize,
    now: i64,
) -> Option<Attention> {
    let told =
        crate::domain::claim_hold::disk_hold_attention(held, candidates, now.saturating_mul(1000))?;
    Some(Attention {
        run_id: None,
        task_id: None,
        pid: None,
        ask_id: None,
        reason_category: None,
        status: "held".into(),
        kind: crate::domain::claim_hold::CLAIM_HELD.into(),
        last_error: Some(truncate_reason(&told)),
        last_error_code: None,
        next: AttentionNext::FreeDisk,
    })
}

/// A report of the relaxation in effect and its deadline, never an ask.
fn disk_relax_attention(view: &crate::domain::disk::DiskRelaxView) -> Attention {
    Attention {
        run_id: None,
        task_id: None,
        pid: None,
        ask_id: None,
        reason_category: None,
        status: "relaxed".into(),
        kind: EventKind::DiskRelaxed.as_str().into(),
        last_error: Some(view.relaxed.message()),
        last_error_code: None,
        next: AttentionNext::ReportDiskRelaxation,
    }
}

/// A supervisor's settings of the execution classes' claims in `status`
/// (ADR-t1487-2): `max_spikes` (null for no limit) and `class_aging` (in
/// seconds), each with its `*_source` (`dagq.toml` or `default`). `None`
/// for a registration of an older binary, which records none.
pub(super) fn claim_classes(registration: &SupervisorRegistration) -> Option<Value> {
    registration.class_aging_source.map(|source| {
        json!({
            "max_spikes": registration.max_spikes,
            "max_spikes_source": registration.max_spikes_source,
            "class_aging": registration.class_aging,
            "class_aging_source": source,
        })
    })
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
/// supervisor is alive with a fresh heartbeat, then the queue-wide hold on
/// the claims in place (`claim_held` for the load, the disk or a queue
/// hold, [`claim_hold_in_place`], as `status`' `claim_hold`), then the
/// queue's latest `run_env_program_missing` of a live supervisor or of none
/// named, then each live supervisor's own holds (`ci_watch_held`,
/// `landing_branch_unresolved`, `repository_config_held`) by its own latest record,
/// however many the others recorded since. The candidate samples supply
/// the admission reasons, including actual spacing waits. A registration
/// with only legacy samples keeps its original spacing view.
pub fn claim_holds(
    queue: &(impl crate::application::RunLog + crate::application::SupervisorReads + ?Sized),
    control: &dyn ProcessControl,
    clock: &dyn Clock,
) -> Result<Vec<Value>> {
    let now = clock.now();
    let registrations = queue.supervisors()?;
    let live: Vec<&SupervisorRegistration> = registrations
        .iter()
        .filter(|registration| is_live(registration, control, now))
        .collect();
    let on = |kinds: &[&str], held: EventKind| -> Result<Option<RunEvent>> {
        Ok(queue
            .latest_queue_event(kinds)?
            .filter(|event| event.kind == held))
    };
    let records = [
        claim_hold_in_place(queue, &registrations, control, now)?,
        on(&RUN_ENV_PROGRAM_KINDS, EventKind::RunEnvProgramMissing)?,
    ];
    let tokens: Vec<&str> = live.iter().map(|r| r.token.as_str()).collect();
    let admission = claim_view::admission_records(queue, &tokens)?;
    let last_claim = queue.latest_event_of(event_kind::RUN_CLAIMED)?;
    let spacing = live
        .iter()
        .filter_map(|registration| {
            claim_spacing(registration, last_claim.as_ref(), now).map(|spacing| {
                let mut spacing = spacing;
                spacing["supervisor"] = json!(registration.token);
                if let Some(decision) = admission
                    .iter()
                    .find(|event| event.payload["supervisor"] == registration.token.as_str())
                {
                    claim_view::show_spacing_decision(&mut spacing, decision);
                }
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
    for kinds in [
        crate::domain::claim_hold::REPOSITORY_CONFIG_HOLD,
        crate::domain::ci_watch::CI_WATCH_HOLD,
        crate::domain::landing_branch::LANDING_BRANCH_HOLD,
    ] {
        let own = own_hold_records(queue, kinds, &tokens)?;
        records.extend(
            live.iter()
                .filter_map(|registration| own_held(kinds, &own, registration.token.as_str())),
        );
    }
    let decisions = admission
        .iter()
        .filter(|event| {
            let reason = &event.payload["admission_reason"];
            !reason.is_null()
                && reason != "ClaimSpacing"
                && !records.iter().any(|record| {
                    record.kind == EventKind::ClaimHeld
                        && record.payload["supervisor"] == event.payload["supervisor"]
                        && record.payload["admission_reason"] == *reason
                })
        })
        .map(|event| {
            json!({
                "reason": "claim_admission",
                "since": event.created_at,
                "record": claim_view::admission_view(event),
            })
        })
        .collect::<Vec<_>>();
    let mut view = held(live.is_empty(), records, spacing);
    view.extend(decisions);
    Ok(view)
}

/// Whether the supervisor of `registration` is alive at `now` with a
/// fresh heartbeat.
fn is_live(registration: &SupervisorRegistration, control: &dyn ProcessControl, now: i64) -> bool {
    !heartbeat_stale(
        control.alive(registration.pid),
        now - registration.heartbeat_at,
    )
}

/// The tokens of the supervisors alive now with a fresh heartbeat, as
/// [`claim_holds`] counts them.
pub fn live_supervisors(
    queue: &(impl crate::application::SupervisorReads + ?Sized),
    control: &dyn ProcessControl,
    clock: &dyn Clock,
) -> Result<Vec<String>> {
    let now = clock.now();
    Ok(queue
        .supervisors()?
        .into_iter()
        .filter(|registration| is_live(registration, control, now))
        .map(|registration| registration.token.as_str().to_owned())
        .collect())
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
    queue: &(impl CoordinationReads + SupervisorReads + RunLog + ?Sized),
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
            let events = queue.run_events(run.id())?;
            let progress = Progress::of(run.status(), progress_lease(lease.as_ref()), &events, now);
            let mut health = run_health(&run, &processes, lease, now, control, files);
            super::runs::block_resting_claim(&mut health, &run, &events);
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

/// `status`' and `doctor`'s `local_config`: the main checkout's
/// `dagq.local.toml` as `read` finds it now (`now` in Unix milliseconds),
/// compared with the newest `supervisor_local_config_changed` (`last`), in
/// the shape [`crate::domain::local_config::LocalLayerRead::report`]
/// gives; absent while there is no file. `ignored` is `doctor`'s check
/// that Git ignores the file, `Err` with why when it is not known (an
/// ignore file that cannot be read): `doctor` adds it as `ignored` and,
/// when not ignored, a warning, the first of `warnings`; when not known,
/// `ignored` is null, `ignore_error` the reason, and a warning says so,
/// first.
pub fn local_config(
    read: &crate::domain::local_config::LocalLayerRead,
    last: Option<&RunEvent>,
    now: i64,
    ignored: Option<std::result::Result<bool, String>>,
) -> Option<Value> {
    // The record is read as its type (a payload that does not read is no
    // record): the comparison is a decision on another context's event.
    let last =
        last.and_then(|event| crate::domain::local_config::LocalLayerRecord::of(&event.payload));
    let mut report = read.report(last.as_ref(), now)?;
    let warning = match ignored {
        None => None,
        Some(Ok(ignored)) => {
            report["ignored"] = json!(ignored);
            (!ignored).then(|| {
                "dagq.local.toml is not ignored by .gitignore: committing it disables the layer"
                    .to_owned()
            })
        }
        Some(Err(reason)) => {
            report["ignored"] = Value::Null;
            report["ignore_error"] = json!(reason);
            Some(format!(
                "cannot tell whether Git ignores dagq.local.toml: {reason}"
            ))
        }
    };
    if let Some(warning) = warning
        && let Some(warnings) = report["warnings"].as_array_mut()
    {
        warnings.insert(0, json!(warning));
    }
    Some(report)
}

/// The sections `status` adds for `role` beyond [`status`]'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSections {
    /// The `language` the prompt and the `SessionStart` hook of the inbox
    /// and a planner carry (ADR-t616-2).
    pub language: bool,
    /// What the inbox watches: `inbox_guardrail` (ADR-t1228-2 decision 4),
    /// `inbox_watcher` (ADR-t906-1), `queue_service` (ADR-t1233-4) and
    /// the open planning requests ([`requests`]); a planner's `status` has
    /// none of them: it is not who tells a person a request waits.
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

/// Add `status`'s `held` to `status` at `clock`'s now ([`held_tasks`]),
/// the overdue holds joining its `attention` when that is for `role`
/// ([`for_role`]), as the rest of the attention does.
pub fn add_held_tasks(
    status: &mut Value,
    holds: &[crate::domain::task_hold::HeldTask],
    clock: &dyn Clock,
    role: Option<SessionRole>,
) -> Result<()> {
    let (held, attention) = held_tasks(holds, clock.now());
    status["held"] = held;
    if for_role(role)
        && let Some(listed) = status["attention"].as_array_mut()
    {
        for item in attention {
            listed.push(serde_json::to_value(item)?);
        }
    }
    Ok(())
}

/// `status`'s `held`: each open hold of a task that has not ended
/// (ADR-t1879-1), oldest first, with its reason, `until`, when and by whom
/// it was held (`held_at`, `by_role`, `by_id`), the run it named, when its
/// task stopped moving under it (`settled_at`, null while `state` is
/// `settling`), its task's latest run and that run's status now, and
/// `overdue`, why it waits for a person ([`TaskHold::overdue`], null
/// before). With it, the attention of the overdue ones: the hold stands
/// until a person releases it, and `watch` tells of it once a day
/// (`task_hold_reminded`).
///
/// [`TaskHold::overdue`]: crate::domain::task_hold::TaskHold::overdue
pub fn held_tasks(
    holds: &[(
        crate::domain::task_hold::TaskHold,
        Option<(RunId, RunStatus)>,
    )],
    now: i64,
) -> (Value, Vec<Attention>) {
    let mut attention = Vec::new();
    let listed = holds
        .iter()
        .map(|(hold, run)| {
            let overdue = hold.overdue(now);
            if overdue.is_some() {
                attention.push(Attention {
                    run_id: run.as_ref().map(|(id, _)| id.clone()),
                    task_id: Some(hold.task_id),
                    pid: None,
                    ask_id: None,
                    reason_category: None,
                    status: hold.state().as_str().into(),
                    kind: event_kind::TASK_HOLD_REMINDED.into(),
                    last_error: Some(truncate_reason(&hold.reason)),
                    last_error_code: None,
                    next: AttentionNext::ReleaseOrKeepHold {
                        task_id: hold.task_id,
                    },
                });
            }
            json!({
                "hold_id": hold.id,
                "task_id": hold.task_id,
                "reason": truncate_reason(&hold.reason),
                "until": hold.until,
                "held_at": hold.created_at,
                "by_role": hold.by_role,
                "by_id": hold.by_id,
                "requested_run_id": hold.run_id,
                "state": hold.state(),
                "settled_at": hold.settled_at,
                "run_id": run.as_ref().map(|(id, _)| id),
                "run_status": run.as_ref().map(|(_, status)| status),
                "overdue": overdue,
            })
        })
        .collect::<Vec<_>>();
    (Value::Array(listed), attention)
}

/// How many characters of a request's words `status`'s `requests` shows.
const REQUEST_TEXT_CHARS: usize = 120;

/// `status`'s `requests` for the inbox (ADR-t1394-1): each `open` planning
/// request, oldest first, as `open` gives them with the planner of the
/// runtime's open for it and the asks about it, so a person sees the ones
/// no planner has taken.
/// Its words are cut, not summarized, to [`REQUEST_TEXT_CHARS`]
/// characters. `planner_questions` are the unanswered `planner_question`s
/// about it. `waiting_for` is why it is still open: `planner` while an
/// open planner of the runtime's works on it, `answer` while one of
/// `planner_questions` waits for a person, `no_provider` while the request
/// is named by the standing `planner_start_blocked` event, `planner_start`
/// while it is one the supervisor opens a planner for (`waiting` holds it: no slot, or
/// planners do not start), and `held` otherwise: it waits for an answered
/// question to be carried to a planner, for the answer to a question about
/// a draft its planner left, for another planner (one replanning the same
/// goal or holding a draft it names) or for its replanning operation's
/// snapshot, as `waiting`'s rule tells. Only `no_provider` carries
/// `provider_reasons`, the unavailable providers and reasons from that event.
pub fn requests(
    open: &[(
        crate::domain::plan_request::PlanRequest,
        Option<crate::domain::PlannerId>,
        Vec<crate::domain::Ask>,
    )],
    waiting: &[crate::domain::RequestId],
    blocked: Option<&PlannerStartBlocked>,
) -> Value {
    open.iter()
        .map(|(request, planner, asks)| {
            let questions: Vec<AskId> = asks
                .iter()
                .filter(|ask| {
                    ask.kind == AskKind::PlannerQuestion
                        && ask.closed_at.is_none()
                        && ask.answered_at.is_none()
                })
                .map(|ask| ask.id)
                .collect();
            let blocked = blocked.filter(|blocked| blocked.requests.contains(&request.id));
            let (waiting_for, provider_reasons) = if planner.is_some() {
                ("planner", None)
            } else if !questions.is_empty() {
                ("answer", None)
            } else if let Some(blocked) = blocked {
                ("no_provider", Some(&blocked.providers))
            } else if waiting.contains(&request.id) {
                ("planner_start", None)
            } else {
                ("held", None)
            };
            let mut row = json!({
                "id": request.id,
                "text": truncate(&request.text, REQUEST_TEXT_CHARS)
                    .unwrap_or_else(|| request.text.clone()),
                "priority": request.priority,
                "requested_by": request.requested_by,
                "created_at": request.created_at,
                "planner_id": planner,
                "planner_questions": questions,
                "waiting_for": waiting_for,
            });
            if let Some(reasons) = provider_reasons {
                row["provider_reasons"] = json!(reasons);
            }
            row
        })
        .collect()
}

/// `[replan]` of the bound main checkout as `status` and `doctor` read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplanConfigRead {
    /// No main checkout is bound to the queue: nothing is detected.
    NoCheckout,
    /// The settings read, the defaults when the file or the table is not
    /// there: the ones a supervisor reading the same file uses.
    Read(crate::domain::replan_detect::ReplanConfig),
    /// The file cannot be read or parsed: a supervisor keeps the settings
    /// it read last, which `status` cannot know, so no window is shown.
    Unreadable(String),
}

impl ReplanConfigRead {
    /// From the read of the bound main checkout's `dagq.toml` (`None`
    /// without a checkout): no file or no table is the defaults.
    pub fn of(read: Option<Result<Option<crate::domain::replan_detect::ReplanConfig>>>) -> Self {
        match read {
            None => Self::NoCheckout,
            Some(Ok(read)) => Self::Read(read.unwrap_or_default()),
            Some(Err(error)) => Self::Unreadable(format!("{error:#}")),
        }
    }
}

/// The queue's reads of the replanning `status` and `doctor` show.
pub trait ReplanSource {
    /// The open operations, oldest first, with their task's status.
    fn open_replans(
        &self,
    ) -> Result<Vec<(crate::domain::task_replan::ReplanOperation, TaskStatus)>>;
    /// Every operation, closed ones included.
    fn replan_operations(&self) -> Result<Vec<crate::domain::task_replan::ReplanOperation>>;
    fn task_events(&self, task: TaskId) -> Result<Vec<crate::domain::RunEvent>>;
    /// The operations of `task`'s lineage.
    fn replan_lineage(
        &self,
        task: TaskId,
    ) -> Result<Vec<crate::domain::task_replan::ReplanOperation>>;
}

/// `status`'s and `doctor`'s `replans` from `source` under `config`
/// ([`replans`]): each task of an open operation once, with its turns by
/// cause, its window of reviews as the detection reads it under the
/// settings read (none when they cannot be read) and its lineage's parks.
pub fn replans_report(source: &impl ReplanSource, config: &ReplanConfigRead) -> Result<Value> {
    use crate::domain::replan_detect;
    let open = source.open_replans()?;
    let all = source.replan_operations()?;
    let settings = match config {
        ReplanConfigRead::NoCheckout => Some(replan_detect::ReplanConfig::default()),
        ReplanConfigRead::Read(read) => Some(*read),
        ReplanConfigRead::Unreadable(_) => None,
    };
    let mut tasks: Vec<ReplanTask> = Vec::new();
    for (op, _) in &open {
        if tasks.iter().any(|task| task.task == op.task_id) {
            continue;
        }
        let events = source.task_events(op.task_id)?;
        let lineage = source.replan_lineage(op.task_id)?;
        let own: Vec<_> = lineage
            .iter()
            .filter(|other| other.task_id == op.task_id)
            .cloned()
            .collect();
        tasks.push(ReplanTask {
            task: op.task_id,
            turns: replan_detect::count_turns(&events),
            window: settings.map(|settings| replan_detect::window(&settings, &events, &own)),
            auto_parks: replan_detect::auto_parks(&lineage),
        });
    }
    Ok(replans(
        &open,
        &ReplanReads {
            all: &all,
            tasks: &tasks,
            config,
        },
    ))
}

/// The task of an open operation as `status` shows it: its turns by
/// cause, its window of reviews (none when `[replan]` cannot be read) and
/// the parks a diagnosis made in its lineage without a person.
pub struct ReplanTask {
    pub task: TaskId,
    pub turns: crate::domain::replan_detect::TaskTurns,
    pub window: Option<crate::domain::replan_detect::Window>,
    pub auto_parks: usize,
}

/// What `status` and `doctor` read of the replanning beside its open
/// operations: every operation, the task of each open one and `[replan]`
/// of the bound main checkout as read.
pub struct ReplanReads<'a> {
    pub all: &'a [crate::domain::task_replan::ReplanOperation],
    pub tasks: &'a [ReplanTask],
    pub config: &'a ReplanConfigRead,
}

/// `status`'s and `doctor`'s `replans`: the open replanning operations
/// (ADR-t1521-2), oldest first, each with its task's status now, where it
/// is and why (its last move's reason, else its request's), its diagnosis,
/// its task's turns by cause and its window of reviews as the detection
/// reads them (how many reviews did not pass against the threshold, the
/// threshold after a diagnosis that went on being the continuation's
/// limit), how many guard their task, and the ones whose task ended while
/// they stayed open or that are `blocked` (`problems`), which only the
/// runtime's outcome closes or moves on; then `[replan]` as read
/// (`detection`) and every operation counted (`counts`): by status, by
/// origin, by the diagnosis's choice and who decided it, by its primary
/// cause, how many were fenced for a replanning, and the tasks each
/// applied one was replaced by.
pub fn replans(
    open: &[(
        crate::domain::task_replan::ReplanOperation,
        crate::domain::TaskStatus,
    )],
    reads: &ReplanReads<'_>,
) -> Value {
    use crate::domain::task_replan::ReplanStatus;
    let listed: Vec<Value> = open
        .iter()
        .map(|(op, task_status)| {
            let task = reads.tasks.iter().find(|task| task.task == op.task_id);
            json!({
                "id": op.id,
                "task_id": op.task_id,
                "task_status": task_status,
                "run_id": op.run_id,
                "origin": op.origin,
                "status": op.status,
                "guards": op.status.guards(),
                "reason": truncate_reason(op.status_reason.as_deref().unwrap_or(&op.reason)),
                "generation": op.generation,
                "request_id": op.request_id,
                "snapshot_id": op.snapshot_id,
                "updated_at": op.updated_at,
                "diagnosis": op.diagnosis,
                "turns": task.map(|task| &task.turns.total),
                "window": task.and_then(|task| task.window.as_ref()),
                "lineage_auto_parks": task.map(|task| task.auto_parks),
            })
        })
        .collect();
    let mut problems: Vec<Value> = open
        .iter()
        .filter(|(_, task_status)| *task_status != crate::domain::TaskStatus::InProgress)
        .map(|(op, task_status)| {
            json!({"id": op.id, "task_id": op.task_id, "task_status": task_status,
                   "problem": "task_not_in_progress"})
        })
        .collect();
    // A blocked operation waits for whoever fixes the step it failed at.
    problems.extend(
        open.iter()
            .filter(|(op, _)| op.status == ReplanStatus::Blocked)
            .map(|(op, _)| {
                json!({"id": op.id, "task_id": op.task_id, "run_id": op.run_id,
                       "problem": "blocked", "return_phase": op.return_phase,
                       "reason": truncate_reason(op.status_reason.as_deref().unwrap_or(&op.reason))})
            }),
    );
    let mut counts =
        std::collections::BTreeMap::<&str, std::collections::BTreeMap<String, usize>>::new();
    let mut count = |what: &'static str, key: &str| {
        *counts
            .entry(what)
            .or_default()
            .entry(key.to_owned())
            .or_default() += 1;
    };
    let mut fenced = 0;
    let mut replaced_by = Vec::new();
    for op in reads.all {
        count("by_status", op.status.as_str());
        count("by_origin", op.origin.as_str());
        if let Some(choice) = op.diagnosis.choice {
            count("by_choice", choice.as_str());
        }
        if let Some(by) = &op.diagnosis.decided_by {
            count("by_decider", by);
        }
        if let Some(cause) = op.diagnosis.causes.first() {
            count("by_primary_cause", cause.as_str());
        }
        let past_fence = op.status.guards()
            || matches!(
                op.status,
                ReplanStatus::Applied | ReplanStatus::ContinuingAfterPark | ReplanStatus::Withdrawn
            );
        fenced += usize::from(past_fence);
        if !op.replaced_by.is_empty() {
            replaced_by.push(json!({"operation_id": op.id, "task_id": op.task_id,
                                    "replaced_by": op.replaced_by}));
        }
    }
    let mut counts: serde_json::Map<String, Value> = counts
        .into_iter()
        .map(|(what, by)| (what.to_owned(), json!(by)))
        .collect();
    counts.insert("operations".into(), json!(reads.all.len()));
    counts.insert("fenced".into(), json!(fenced));
    counts.insert("replaced_by".into(), Value::Array(replaced_by));
    let mut report = json!({
        "open": listed,
        "guarding": open.iter().filter(|(op, _)| op.status.guards()).count(),
        "problems": problems,
        "detection": match reads.config {
            ReplanConfigRead::Read(read) => json!(read),
            ReplanConfigRead::NoCheckout | ReplanConfigRead::Unreadable(_) => Value::Null,
        },
        "counts": counts,
    });
    if let ReplanConfigRead::Unreadable(error) = reads.config {
        report["detection_error"] = json!(format!(
            "[replan] of dagq.toml cannot be read ({error}): a supervisor keeps the settings it read last, which are not shown"
        ));
    }
    report
}

/// `status`'s, `stats`' and `doctor`'s `recurring` (ADR-0072) of `read`,
/// the `[[recurring]]` tables of the bound main checkout's `dagq.toml` as
/// read: `{schedule}` with one entry per key from `schedule`, the queue's
/// history of them ([`crate::domain::recurring::RecurringSchedule`]),
/// `{error}` when the file cannot be read (how `doctor` tells of a mistake
/// of their format), none without a table.
pub fn recurring(
    read: Result<Vec<crate::domain::recurring::RecurringDraft>>,
    schedule: impl FnOnce(
        &[crate::domain::recurring::RecurringDraft],
    ) -> Result<Vec<crate::domain::recurring::RecurringSchedule>>,
) -> Result<Option<Value>> {
    match read {
        Err(error) => Ok(Some(json!({"error": format!("{error:#}")}))),
        Ok(items) if items.is_empty() => Ok(None),
        Ok(items) => Ok(Some(json!({"schedule": schedule(&items)?}))),
    }
}

/// The most recent batch's members, verification stage and removals.
/// A batch interrupted by a dead supervisor without its closing event
/// disappears when recovery releases its members' leases.
fn landing_batch_view(
    queue: &(impl crate::application::EventStore + CoordinationReads + ?Sized),
) -> Result<Option<crate::domain::landing_batch::Progress>> {
    use crate::domain::landing_batch::Progress;
    let Some(event) = queue.latest_event_of(EventKind::LandingBatchProgress.as_str())? else {
        return Ok(None);
    };
    let Ok(progress) = serde_json::from_value::<Progress>(event.payload) else {
        return Ok(None);
    };
    if progress.finished {
        return Ok(Some(progress));
    }
    for run in &progress.runs {
        if queue.run_lease(run)?.is_some() {
            return Ok(Some(progress));
        }
    }
    Ok(None)
}

/// Status's `landing_queue` is in domain priority/time order. `position`
/// is one-based. Each row includes `priority` and `entered_at` for order,
/// `basis` and `judgment` for authorization, and the `e2e` and `disk`
/// requirements from [`crate::domain::landing_queue::Entry`]. `waiting_for`
/// is the latest supervisor probe (null means eligible at that probe),
/// or `lease` while an owner is still preparing the run. Active entries
/// show `e2e_running` or `integrating` through their execution. An integrating run’s
/// provenance is reconstructed immediately before integration started.
fn landing_queue_view(
    queue: &(impl RunLog + TaskReads + SupervisorReads + CoordinationReads + ?Sized),
    graph: &crate::application::GraphInput,
    control: &dyn ProcessControl,
    now: i64,
) -> Result<Vec<Value>> {
    use crate::domain::landing_queue::{Entry, EntryJournal, LandingQueue, recorded_wait};
    let effective: HashMap<_, _> = crate::application::dependency_graph(graph.clone(), None)
        .tasks
        .into_iter()
        .map(|task| (task.id, task.effective_priority))
        .collect();
    let mut entries = Vec::new();
    let mut waits = HashMap::new();
    for status in [
        RunStatus::AwaitingIntegration,
        RunStatus::NeedsSession,
        RunStatus::Integrating,
    ] {
        for run in queue.runs_with_status(status)? {
            let all_events = queue.run_events(run.id())?;
            if status != RunStatus::Integrating
                && !RunHistory::from_events(&all_events).can_queue_landing(status)
            {
                continue;
            }
            if EntryJournal::read(&all_events).open.is_none() {
                continue;
            }
            // Read the active landing's authorization at its start. Its
            // entry's lifetime above comes from the entire journal.
            let events = if status == RunStatus::Integrating {
                let at = all_events
                    .iter()
                    .rposition(|e| e.kind == EventKind::IntegrationStarted)
                    .unwrap_or(all_events.len());
                &all_events[..at]
            } else {
                &all_events[..]
            };
            let priority = effective.get(&run.task_id()).copied().unwrap_or_default();
            let head = run.result_commit().map(|commit| commit.as_str());
            let ready = Entry::from_events(run.id().clone(), run.task_id(), events, priority, head);
            let awaiting = ready.is_none();
            if let Some(entry) = ready.or_else(|| {
                Entry::awaiting_reapproval(run.id().clone(), run.task_id(), events, priority, head)
            }) {
                let waiting = if status == RunStatus::Integrating {
                    json!("integrating")
                } else if awaiting {
                    json!("awaiting_reapproval")
                } else if let Some(lease) = queue.run_lease(run.id())? {
                    if crate::domain::run_e2e::running(events)
                        && !lease_is_stale(&lease, now, control)
                    {
                        json!("e2e_running")
                    } else {
                        json!("lease")
                    }
                } else if !RunHistory::from_events(events).landing_ready() {
                    json!("preparing")
                } else {
                    recorded_wait(events, entry.judgment)
                        .map_or(Value::Null, |e| e.payload["waiting_for"].clone())
                };
                waits.insert(run.id().clone(), waiting);
                entries.push(entry);
            }
        }
    }
    Ok(LandingQueue::new(entries)
        .entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut view = json!(entry);
            view["position"] = json!(index + 1);
            view["waiting_for"] = waits.remove(&entry.run_id).unwrap_or(Value::Null);
            view
        })
        .collect())
}
