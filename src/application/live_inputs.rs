//! What a supervisor's pass reads to judge now (ADR-0043 decision 5,
//! docs/design/measurement.md "今の判定の記録"): the slots, the unfinished
//! runs with their run directories' markers and the processes of their
//! background wrappers (no cmux, ADR-t1433-1), and the `[stall]`
//! thresholds. The pass records them as the inputs of the judgments of
//! now; `stats` reads the judgments the ledger folds of those records,
//! never these reads.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::SystemTime,
};

use anyhow::Result;

use super::{
    AgentSignals, CoordinationReads, EventStore, PlanningRecords, ProcessControl, RunFiles, RunLog,
    RunReads, StatusFilter, SupervisorReads, TaskQuery, TaskReads,
};
use crate::domain::{
    EventKind, GoalStatus, RunId, RunStatus, SupervisorPulse, TaskRun, TaskStatus,
    live_alerts::{EVENTS_PER_READ, LiveHistory, LiveInputs, first_pass_reads},
    stall::{BackgroundTask, IDLE_LOG, StallConfig, background_first_seen},
    stats::{LiveRun, SlotSnapshot, outlier_thresholds},
};

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
    queue: &(impl TaskReads + PlanningRecords + SupervisorReads + CoordinationReads + RunLog + ?Sized),
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
    queue: &(impl TaskReads + PlanningRecords + SupervisorReads + CoordinationReads + RunLog + ?Sized),
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
/// events found one past the newest the pass started at.
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
    let mut events = newest;
    if let Some(held) = hold.filter(|held| reads.opens_hold(held)) {
        events.extend(events_of_kinds(queue, &[reads.hold_ends], held.id, upto)?);
    }
    events.extend(events_of_kinds(
        queue,
        &reads.work,
        crate::domain::EventId::new(0),
        upto,
    )?);
    let mut history = LiveHistory::seeded(upto, events);
    for run in history.unsettled() {
        history.refold(&run, &queue.run_events(&run)?);
    }
    Ok(Some(history))
}

/// The events of one of `kinds` with `after < id <= upto`,
/// [`EVENTS_PER_READ`] at a time.
fn events_of_kinds(
    queue: &(impl RunLog + ?Sized),
    kinds: &[EventKind],
    mut after: crate::domain::EventId,
    upto: crate::domain::EventId,
) -> Result<Vec<crate::domain::RunEvent>> {
    let kinds: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
    let mut events = Vec::new();
    loop {
        let page = queue.events_of_between(&kinds, after, upto, EVENTS_PER_READ)?;
        let Some(last) = page.last() else {
            return Ok(events);
        };
        after = last.id;
        let full = page.len() >= EVENTS_PER_READ;
        events.extend(page);
        if !full {
            return Ok(events);
        }
    }
}

/// Fold into `history` the events past its cursor, [`EVENTS_PER_READ`] at
/// a time, then read the events of the runs not finished: every run of the
/// queue, and the events the judgments of now read, which judge as the
/// whole history does ([`LiveHistory::events`]). Each run not finished has
/// its work clock folded again from its own events ([`LiveHistory::refold`]).
/// Only a first pass that [`seed_history`] left at no event reads every
/// event, a page at a time.
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
/// which are alive) less the runs executing, and the candidates and ready
/// tasks.
fn slot_snapshot(
    queue: &(impl EventStore + TaskReads + SupervisorReads + CoordinationReads + RunReads + ?Sized),
    processes: &dyn ProcessControl,
    now: i64,
) -> Result<SlotSnapshot> {
    let registrations = queue.supervisors()?;
    let slots: i64 = registrations
        .iter()
        .filter(|registration| {
            !SupervisorPulse::judge(registration, processes.alive(registration.pid), now).stale
        })
        .map(|registration| i64::from(registration.parallel))
        .sum();
    // A run that waits for a person, or waits to go back to its slot,
    // holds no slot (ADR-0071 decision 13), nor does a claim of
    // `already_satisfied` resting on its ask (ADR-t1818-1). Every other run a registered
    // supervisor leases holds its slot as the supervisor counts it, landing,
    // in review or resumed alike; an unfinished run under no such lease holds
    // one unless it is landed under a token no supervisor registered (a
    // person's `integrate`) (ADR-t610-1).
    let supervisor_leases: HashSet<RunId> = queue
        .run_leases()?
        .into_iter()
        .filter(|lease| {
            registrations
                .iter()
                .any(|registration| registration.token == lease.token)
        })
        .map(|lease| lease.run_id)
        .collect();
    let mut holding: HashSet<RunId> = queue
        .active_runs()?
        .into_iter()
        .filter(|run| run.status() != RunStatus::Integrating)
        .map(|run| run.id().clone())
        .collect();
    holding.extend(supervisor_leases);
    let mut executing: i64 = 0;
    for run in &holding {
        if crate::domain::waiting::WaitState::holds_a_slot(&queue.run_events(run)?) {
            executing += 1;
        }
    }
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
        free_slots: slots - executing,
        candidates: queue.candidates()?.len(),
        ready: ready.saturating_sub(ready_in_draft_goals),
    })
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
