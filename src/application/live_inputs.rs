//! What a supervisor's pass reads to judge now (ADR-0043 decision 5,
//! docs/design/measurement.md "今の判定の記録"): the slots, the unfinished
//! runs with their run directories' markers and the processes of their
//! background wrappers (no cmux, ADR-t1433-1), and the `[stall]`
//! thresholds. The pass records them as the inputs of the judgments of
//! now; `stats` reads the judgments the ledger folds of those records,
//! never these reads.

use std::{collections::HashMap, path::Path, time::SystemTime};

use super::{
    AgentSignals, AskQuery, AskStore, CoordinationReads, PlanningRecords, ProcessControl, RunFiles,
    RunLog, StatusFilter, SupervisorReads, TaskQuery, TaskReads,
};
use crate::domain::{
    EventKind, GoalStatus, RunId, RunLease, RunStatus, SupervisorPulse, SupervisorRegistration,
    TaskRun, TaskStatus, admission,
    landing_queue::{Resources, waiting_for_answer},
    live_alerts::{
        EVENTS_PER_READ, LiveHistory, LiveInputs, SeededFold, WORK_EVENTS_PER_READ,
        first_pass_reads,
    },
    run_progress::{LandingStage, landing_stage},
    stall::{BackgroundTask, IDLE_LOG, StallConfig, background_first_seen},
    stats::{LiveRun, SlotSnapshot, outlier_thresholds},
};
use anyhow::Result;

/// The marker the agent's `UserPromptSubmit` hook writes when the session
/// takes an input (ADR-0043 decision 2), in the run directory.
pub const PROMPT_SUBMIT_MARKER: &str = "prompt-submit.json";

/// What the judgments of now read outside the queue: the run directories'
/// markers through `files` and `signals`, and the `[stall]` of `dagq.toml`
/// (`None` when there is no file).
pub struct LiveSources<'a> {
    pub files: &'a dyn RunFiles,
    pub signals: &'a dyn AgentSignals,
    pub config_file: &'a dyn Fn() -> Result<Option<StallConfig>>,
}

/// Everything `stats`' judgments of now read besides the events
/// ([`crate::domain::live_alerts::judge`]), read at `now` from the queue,
/// the processes and `sources`: what a supervisor's pass records
/// (docs/design/measurement.md "今の判定の記録").
pub fn live_inputs(
    queue: &(
         impl TaskReads
         + PlanningRecords
         + SupervisorReads
         + CoordinationReads
         + RunLog
         + AskStore
         + ?Sized
     ),
    processes: &dyn ProcessControl,
    now: i64,
    events: &[crate::domain::RunEvent],
    sources: &LiveSources<'_>,
) -> Result<LiveInputs> {
    let (runs, positions) = live_runs(&queue.all_runs()?, processes, events, sources)?;
    let outliers = outlier_thresholds(events, &queue.task_goals()?, &runs);
    Ok(LiveInputs {
        slots: slot_snapshot(queue, processes, now)?,
        config: super::stats::stall_config(events, sources.config_file)?,
        runs,
        outliers,
        positions,
    })
}

/// [`live_inputs`] on `history`, the events a supervisor folded on its
/// passes before, and the events the judgments read with them
/// ([`carried_events`]): what its pass judges and records without reading
/// the whole history.
pub fn carried_live_inputs(
    queue: &(
         impl TaskReads
         + PlanningRecords
         + SupervisorReads
         + CoordinationReads
         + RunLog
         + AskStore
         + ?Sized
     ),
    processes: &dyn ProcessControl,
    now: i64,
    history: &mut LiveHistory,
    sources: &LiveSources<'_>,
) -> Result<(LiveInputs, Vec<crate::domain::RunEvent>)> {
    seed_history(queue, history)?;
    let (all, events) = carried_events(queue, history)?;
    let (runs, positions) = live_runs(&all, processes, &events, sources)?;
    let outliers = history.outlier_thresholds(&queue.task_goals()?, &runs);
    let inputs = LiveInputs {
        slots: slot_snapshot(queue, processes, now)?,
        config: super::stats::stall_config(&events, sources.config_file)?,
        runs,
        outliers,
        positions,
    };
    Ok((inputs, events))
}

/// How many times the first pass of a process reads what it folds from
/// ([`seeded_history`]) before it folds every event instead.
const SEED_ATTEMPTS: usize = 3;

/// On the first pass of a process (`history` at no event), fold into
/// `history` the events up to the newest from the reads
/// [`first_pass_reads`] names, not from every event. When an event past
/// the newest came while it read, it reads again, and after
/// [`SEED_ATTEMPTS`] it leaves `history` to [`carried_events`], which
/// then folds every event a page at a time.
fn seed_history(queue: &(impl RunLog + ?Sized), history: &mut LiveHistory) -> Result<()> {
    if history.upto() > crate::domain::EventId::new(0) {
        return Ok(());
    }
    for _ in 0..SEED_ATTEMPTS {
        if let Some(seeded) = seeded_history(queue)? {
            *history = seeded;
            break;
        }
    }
    Ok(())
}

/// The history up to the queue's newest event from the reads
/// [`first_pass_reads`] names and the whole events of the runs it leaves
/// unsettled ([`LiveHistory::unsettled`]); `None` when a read of the newest
/// events found one past the newest the pass started at. The work events
/// are folded page by page and let go ([`SeededFold`]), so they hold a page
/// at any size of the history. What the deferrals' read answers is one event
/// of each task, held until the fold passes it: it grows with the tasks, by
/// design, 0.9 KB of each in the example's queue and 1.2 KB while they are read
/// (`CAP_FIRST_PASS_PEAK` of `examples/supervisor_startup_memory.rs`).
fn seeded_history(queue: &(impl RunLog + ?Sized)) -> Result<Option<LiveHistory>> {
    let upto = queue.latest_event_id()?;
    let reads = first_pass_reads();
    let mut newest = Vec::new();
    for kind in reads.hold {
        newest.extend(queue.latest_event_of(kind.as_str())?);
    }
    let hold = newest.iter().max_by_key(|event| event.id).cloned();
    newest.extend(queue.latest_event_of(reads.stall.as_str())?);
    newest.extend(queue.latest_task_events(&reads.deferrals.map(EventKind::as_str))?);
    if newest.iter().any(|event| event.id > upto) {
        return Ok(None);
    }
    let mut whole = newest;
    if let Some(held) = hold.filter(|held| reads.opens_hold(held)) {
        each_page_of_kinds(
            queue,
            &[reads.hold_ends],
            (held.id, upto),
            EVENTS_PER_READ,
            |page| whole.extend(page),
        )?;
    }
    let mut fold = SeededFold::new(upto, whole);
    each_page_of_kinds(
        queue,
        &reads.work,
        (crate::domain::EventId::new(0), upto),
        WORK_EVENTS_PER_READ,
        |page| fold.page(&page),
    )?;
    let mut history = fold.finish();
    for run in history.unsettled() {
        history.refold(&run, &queue.run_events(&run)?);
    }
    Ok(Some(history))
}

/// Hand `each` the events of one of `kinds` in `(after, upto]`, `limit` at a
/// time, oldest first, letting each page go before reading the next.
fn each_page_of_kinds(
    queue: &(impl RunLog + ?Sized),
    kinds: &[EventKind],
    (mut after, upto): (crate::domain::EventId, crate::domain::EventId),
    limit: usize,
    mut each: impl FnMut(Vec<crate::domain::RunEvent>),
) -> Result<()> {
    let kinds: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
    loop {
        let page = queue.events_of_between(&kinds, after, upto, limit)?;
        let Some(last) = page.last() else {
            return Ok(());
        };
        after = last.id;
        let full = page.len() >= limit;
        each(page);
        if !full {
            return Ok(());
        }
    }
}

/// Fold into `history` the events past its cursor, [`EVENTS_PER_READ`] at
/// a time, then read the events of the runs not finished: every run of the
/// queue, and the events the judgments of now read, which judge as the
/// whole history does ([`LiveHistory::events`]). Each run not finished has
/// its work clock folded again from its own events ([`LiveHistory::refold`]).
/// Only a first pass that [`seed_history`] left at no event reads every
/// event, a page at a time. Every run's row is answered and held for the
/// pass ([`live_runs`] takes each one's position in that list): it grows
/// with the runs, by design, 1.3 KB of each in the example's queue
/// (`CAP_FIRST_PASS_PEAK` of `examples/supervisor_startup_memory.rs`).
fn carried_events(
    queue: &(impl RunLog + ?Sized),
    history: &mut LiveHistory,
) -> Result<(Vec<TaskRun>, Vec<crate::domain::RunEvent>)> {
    let latest = queue.latest_event_id()?;
    while latest > history.upto() {
        let new = queue.events_between(
            history.upto(),
            latest,
            &crate::domain::EventFilter::default(),
            EVENTS_PER_READ,
        )?;
        if new.is_empty() {
            history.reach(latest);
        }
        history.mark(&new);
    }
    let all = queue.all_runs()?;
    let mut runs = Vec::new();
    for run in all.iter().filter(|run| !finished(run.status())) {
        let of_run = queue.run_events(run.id())?;
        history.refold(run.id(), &of_run);
        runs.extend(of_run);
    }
    let events = history.events(runs);
    Ok((all, events))
}

/// The queue's slots at `now`: the live supervisors' (`processes` telling
/// which are alive) less worker-slot usage, and the candidates and ready
/// tasks.
fn slot_snapshot(
    queue: &(impl RunLog + TaskReads + SupervisorReads + CoordinationReads + AskStore + ?Sized),
    processes: &dyn ProcessControl,
    now: i64,
) -> Result<SlotSnapshot> {
    let registrations = queue.supervisors()?;
    let leases = queue.run_leases()?;
    let shared = shared_landing_entries(queue, processes, &registrations, &leases, now)?;
    let free_slots = free_worker_slots(queue, processes, &registrations, &leases, &shared, now)?;
    let ready = queue
        .list(&TaskQuery {
            status: StatusFilter::Only(vec![TaskStatus::Ready]),
            limit: 1,
            ..Default::default()
        })?
        .total;
    // A draft goal's ready tasks wait for `goal ready`, not for a
    // predecessor, so they do not make free slots an alert.
    let ready_in_draft_goals: usize = queue
        .list_goals()?
        .iter()
        .filter(|goal| goal.status == GoalStatus::Draft)
        .map(|goal| goal.tasks.ready)
        .sum();
    Ok(SlotSnapshot {
        free_slots,
        candidates: queue.candidates()?.len(),
        ready: ready.saturating_sub(ready_in_draft_goals),
    })
}

/// Shared landing entries that contribute claim backpressure. Use the
/// same landing-ready and admission predicates as the supervisor's pass,
/// with the recorded gates and current queue rows. The local landing-turn
/// waits are counted from supervisor leases separately.
pub(super) fn shared_landing_entries(
    queue: &(impl RunLog + TaskReads + AskStore + ?Sized),
    processes: &dyn ProcessControl,
    registrations: &[SupervisorRegistration],
    leases: &[RunLease],
    now: i64,
) -> Result<HashMap<String, usize>> {
    use crate::domain::{
        RunHistory, ci_watch::CI_WATCH_HOLD, claim_hold::OwnHold,
        landing_branch::LANDING_BRANCH_HOLD, run_env::RUN_ENV_PROGRAM_KINDS,
    };
    let mut tokens: Vec<_> = registrations
        .iter()
        .filter(|r| !SupervisorPulse::judge(r, processes.alive(r.pid), now).stale)
        .map(|r| r.token.as_str())
        .collect();
    if tokens.is_empty() {
        return Ok(HashMap::new());
    }
    let env = queue
        .latest_queue_event(&RUN_ENV_PROGRAM_KINDS)?
        .is_some_and(|event| event.kind == EventKind::RunEnvProgramMissing);
    let ci = queue.latest_events_by_supervisor(&CI_WATCH_HOLD.kinds(), &tokens)?;
    let branch = queue.latest_events_by_supervisor(&LANDING_BRANCH_HOLD.kinds(), &tokens)?;
    tokens.retain(|token| {
        admission::Progress {
            gates: admission::Gates {
                ci: CI_WATCH_HOLD.holds(OwnHold::latest_of(&ci, token)),
                env,
                branch: LANDING_BRANCH_HOLD.holds(OwnHold::latest_of(&branch, token)),
            },
            ..Default::default()
        }
        .can_progress()
    });
    if tokens.is_empty() {
        return Ok(HashMap::new());
    }
    let candidates = super::integrate::landing_candidates(queue)?;
    if candidates.is_empty() {
        return Ok(HashMap::new());
    }
    let mut asks = HashMap::<_, Vec<_>>::new();
    for ask in queue.asks(AskQuery {
        open: true,
        ..Default::default()
    })? {
        if let Some(run) = &ask.run_id {
            asks.entry(run.clone()).or_default().push(ask);
        }
    }
    let by_run: HashMap<_, _> = leases
        .iter()
        .map(|lease| (&lease.run_id, &lease.token))
        .collect();
    let mut queued = 0;
    for (run, events) in candidates {
        let history = RunHistory::from_events(&events);
        let lease = by_run.get(run.id());
        let facts = Resources {
            preparing: !history.landing_ready(),
            leased: lease.is_some(),
            e2e_lease: lease.is_some_and(|token| registrations.iter().any(|r| &r.token == *token))
                && crate::domain::run_e2e::landing_lease(&events),
            ..Default::default()
        };
        if facts.counts_toward_backpressure()
            && !waiting_for_answer(asks.get(run.id()).map_or(&[], Vec::as_slice), &history)
            && !queue.task_fenced(run.task_id())?
        {
            queued += 1;
        }
    }
    Ok(tokens
        .into_iter()
        .map(|token| (token.to_owned(), queued))
        .collect())
}

/// Free capacity for any task, summed over live supervisors using the
/// same worker judgment as claims. Each supervisor's usable capacity
/// bottoms at zero; a stale or full supervisor cannot spend another's slots.
pub(super) fn free_worker_slots(
    queue: &(impl RunLog + ?Sized),
    processes: &dyn ProcessControl,
    registrations: &[SupervisorRegistration],
    leases: &[RunLease],
    shared: &HashMap<String, usize>,
    now: i64,
) -> Result<i64> {
    let live: Vec<_> = registrations
        .iter()
        .filter(|r| !SupervisorPulse::judge(r, processes.alive(r.pid), now).stale)
        .collect();
    let mut free = 0;
    for registration in live {
        let parallel = usize::try_from(registration.parallel).unwrap_or(0);
        let mut workers = 0;
        let mut queued = shared
            .get(registration.token.as_str())
            .copied()
            .unwrap_or(0);
        for lease in leases
            .iter()
            .filter(|lease| lease.token == registration.token)
        {
            let run = queue.run(&lease.run_id)?;
            let events = queue.run_events(run.id())?;
            if !crate::domain::waiting::WaitState::holds_a_slot(&events) {
                continue;
            }
            match landing_stage(run.status(), &events) {
                None => workers += 1,
                Some(LandingStage::Queued) => queued += 1,
                Some(LandingStage::Integrating | LandingStage::E2e) => {}
            }
        }
        let used = crate::domain::light_slots::worker_slots(workers, queued, parallel);
        free += i64::try_from(parallel.saturating_sub(used)).unwrap_or(i64::MAX);
    }
    Ok(free)
}

/// The runs of `all` (every run of the queue) not finished yet, in its order, with their markers and
/// whether their background wrapper runs, and each one's position in the
/// list of every run.
fn live_runs(
    all: &[TaskRun],
    processes: &dyn ProcessControl,
    events: &[crate::domain::RunEvent],
    sources: &LiveSources<'_>,
) -> Result<(Vec<LiveRun>, HashMap<RunId, usize>)> {
    let mut runs = Vec::new();
    let mut positions = HashMap::new();
    for (position, run) in all.iter().enumerate() {
        if finished(run.status()) {
            continue;
        }
        let mut live = live_run(run, sources)?;
        live.workspace_id = last_session(events, run.id(), run.workspace_id()).map(str::to_owned);
        live.background_alive = background_alive(live.workspace_id.as_deref(), processes);
        positions.insert(run.id().clone(), position);
        runs.push(live);
    }
    Ok((runs, positions))
}

fn finished(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Integrated | RunStatus::Succeeded | RunStatus::Failed | RunStatus::Interrupted
    )
}

fn millis(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// The session the run `run` opened last, of `events`
/// ([`crate::domain::background_wrapper::last_session`]): `first` is the
/// run's record of its first session.
fn last_session<'a>(
    events: &'a [crate::domain::RunEvent],
    run: &RunId,
    first: Option<&'a str>,
) -> Option<&'a str> {
    crate::domain::background_wrapper::last_session(
        events
            .iter()
            .filter(|event| event.run_id.as_ref() == Some(run)),
        first,
    )
}

/// Whether the wrapper of `session`, the session a run opened last, runs,
/// when that session is a background wrapper's (ADR-t1404-1): its handle's
/// pid shows the start the handle recorded, so a pid another process took
/// is not taken for it. `None` for a session in a workspace.
fn background_alive(session: Option<&str>, processes: &dyn ProcessControl) -> Option<bool> {
    use crate::domain::background_wrapper::BackgroundHandle;
    let handle = BackgroundHandle::parse(session?)?;
    Some(handle.is(handle.pid, processes.start_identity(handle.pid).as_deref()))
}

/// The unfinished `run` with the write times of its idle marker, receipt
/// and prompt-submit marker. A file that is not there, or a run without a
/// directory, has none.
fn live_run(run: &TaskRun, sources: &LiveSources<'_>) -> Result<LiveRun> {
    let modified = |path: &Path| {
        sources
            .files
            .is_file(path)
            .then(|| sources.files.modified(path).ok().map(millis))
            .flatten()
    };
    let (idle, background_since, input) = match run.run_dir() {
        Some(dir) => {
            let idle = sources
                .files
                .read_stamped(&Path::new(dir).join("idle.json"))?
                .map(|(modified, bytes)| {
                    (
                        millis(modified),
                        sources.signals.idle_hook(&bytes).background_tasks,
                    )
                });
            let since = match &idle {
                Some((at, tasks)) if !tasks.is_empty() => {
                    background_since(sources, &Path::new(dir).join(IDLE_LOG), *at, tasks)?
                }
                _ => None,
            };
            (
                idle,
                since,
                modified(&Path::new(dir).join(PROMPT_SUBMIT_MARKER)),
            )
        }
        None => (None, None, None),
    };
    Ok(LiveRun {
        run_id: run.id().clone(),
        task_id: run.task_id(),
        status: run.status(),
        workspace_id: run.workspace_id().map(str::to_owned),
        idle,
        receipt: run
            .receipt_path()
            .and_then(|path| modified(Path::new(path))),
        input,
        background_since,
        background_alive: None,
    })
}

/// When the longest running of the marker's background `tasks` was first
/// listed: the earliest of their first appearances in the unbroken streak
/// of markers in the hook's `log` that ends with the marker written `at`.
/// A task the log does not show (no log, as from a session started before
/// the hook wrote one) is timed from the marker. `None` when the log shows
/// none earlier than the marker.
fn background_since(
    sources: &LiveSources<'_>,
    log: &Path,
    at: i64,
    tasks: &[BackgroundTask],
) -> Result<Option<i64>> {
    let Some((_, bytes)) = sources.files.read_stamped(log)? else {
        return Ok(None);
    };
    let history = String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let (secs, marker) = line.split_once('\t')?;
            let secs: i64 = secs.trim().parse().ok()?;
            Some((
                secs.saturating_mul(1000),
                sources
                    .signals
                    .idle_hook(marker.as_bytes())
                    .background_tasks,
            ))
        })
        // The marker itself ends the streak, whether or not its line made it.
        .chain([(at, tasks.to_vec())])
        .collect::<Vec<_>>();
    let seen = background_first_seen(history);
    Ok(tasks
        .iter()
        .filter_map(|task| seen.get(&task.id).copied())
        .min()
        .filter(|&since| since < at))
}

#[cfg(test)]
mod carried_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, RunEvent, TaskId};
    use serde_json::json;

    fn event(id: i64, kind: &str, payload: serde_json::Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: at.to_owned(),
            actor: None,
        }
    }

    /// The process control of [`background_alive`]'s test: the starts of
    /// the processes running now, by pid.
    struct Starts(Vec<(u32, &'static str)>);

    impl ProcessControl for Starts {
        fn alive(&self, pid: u32) -> bool {
            self.0.iter().any(|(p, _)| *p == pid)
        }
        fn terminate(&self, _: u32) -> Result<()> {
            unreachable!()
        }
        fn interrupt(&self, _: u32) -> Result<()> {
            unreachable!()
        }
        fn kill(&self, _: u32) -> Result<()> {
            unreachable!()
        }
        fn start_identity(&self, pid: u32) -> Option<String> {
            self.0
                .iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, start)| (*start).to_owned())
        }
    }

    /// A run's last session in the background is alive while its handle's
    /// pid shows the start the handle recorded: dead when the pid runs
    /// nothing, and dead when another process took the pid. A session in a
    /// workspace has no such judgment, and only the run's own events count.
    #[test]
    fn a_background_wrapper_is_alive_only_with_its_recorded_start() {
        let run = RunId::new("3aa21145-c873-4cec-aee3-ee7f07f52e4a").unwrap();
        let other = RunId::new("4bb21145-c873-4cec-aee3-ee7f07f52e4a").unwrap();
        let started = "Sat Oct  3 10:00:01 2026";
        let handle =
            crate::domain::background_wrapper::BackgroundHandle::new(4242, started).to_string();
        let of = |run: &RunId, id: i64, workspace: &str| RunEvent {
            run_id: Some(run.clone()),
            ..event(
                id,
                "workspace_created",
                json!({"workspace_id": workspace}),
                "1970-01-01T00:01:40.000Z",
            )
        };
        let judged = |events: &[RunEvent], run: &RunId, first: Option<&str>, processes: &Starts| {
            background_alive(last_session(events, run, first), processes)
        };
        let events = [of(&run, 1, "WS-FIRST"), of(&run, 2, &handle)];
        let alive = |processes: &Starts| judged(&events, &run, Some("WS-FIRST"), processes);
        assert_eq!(alive(&Starts(vec![(4242, started)])), Some(true));
        assert_eq!(alive(&Starts(Vec::new())), Some(false));
        assert_eq!(
            alive(&Starts(vec![(4242, "Sat Oct  3 11:30:00 2026")])),
            Some(false),
            "a pid another process took"
        );
        // A session in a workspace, the run's first (no event read), or
        // another run's handle: nothing to judge.
        let processes = Starts(vec![(4242, started)]);
        assert_eq!(
            judged(&events[..1], &run, Some("WS-FIRST"), &processes),
            None
        );
        assert_eq!(judged(&[], &run, Some(&handle), &processes), Some(true));
        assert_eq!(
            judged(
                &[of(&other, 3, &handle)],
                &run,
                Some("WS-FIRST"),
                &processes
            ),
            None
        );
    }

    /// A run's session now, what its running alerts are given as its
    /// workspace, is the one it opened last: a resume's wrapper, recorded
    /// by its `wrapper_launched`, takes the place of the first session's
    /// handle the run's row keeps.
    #[test]
    fn a_runs_session_is_the_wrapper_it_launched_last() {
        let run = RunId::new("3aa21145-c873-4cec-aee3-ee7f07f52e4a").unwrap();
        let other = RunId::new("4bb21145-c873-4cec-aee3-ee7f07f52e4a").unwrap();
        let first = "background:6274:Sat_Oct_3_10:00:01_2026";
        let resume = "background:82867:Sat_Oct_3_11:00:01_2026";
        let of = |run: &RunId, id: i64, kind: &str, workspace: &str| RunEvent {
            run_id: Some(run.clone()),
            ..event(
                id,
                kind,
                json!({"workspace_id": workspace}),
                "1970-01-01T00:01:40.000Z",
            )
        };
        let events = [
            of(&run, 1, "workspace_created", first),
            of(&run, 2, "wrapper_launched", first),
            of(&run, 3, "wrapper_launched", resume),
            of(&other, 4, "wrapper_launched", "background:1:x"),
        ];
        assert_eq!(last_session(&events, &run, Some(first)), Some(resume));
        assert_eq!(last_session(&events[..2], &run, Some(first)), Some(first));
        assert_eq!(last_session(&[], &run, Some(first)), Some(first));
    }
}
