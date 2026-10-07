//! `stats` (ADR-0023 decision 5): the queue's reads that
//! [`crate::domain::stats::stats`] derives the per-run and per-goal times
//! and the thresholds crossed from, and what the running alerts (ADR-0043
//! decision 5) read outside the queue: the run directories' markers and
//! the processes of the runs' background wrappers (no cmux, ADR-t1433-1);
//! and main's Git history for `conflict_hotspots`.

use std::{collections::HashSet, path::Path, time::SystemTime};

use anyhow::Result;

use super::{AgentSignals, ProcessControl, Queue, RunFiles, StatusFilter, TaskQuery};
use crate::domain::{
    GoalStatus, RunId, RunStatus, SupervisorPulse, TaskRun, TaskStatus,
    host_metrics::HostSummary,
    stall::{BackgroundTask, IDLE_LOG, StallConfig, background_first_seen},
    stats::{
        ConflictConfig, ConflictConfigReport, History, LiveRun, LiveSnapshot, SlotSnapshot,
        StallConfigReport, Stats, StatsQuery,
        conflicts::{MainHistory, earliest_conflict},
        stats as aggregate, timestamp_millis, with_areas, with_changes, without_cargo_measures,
    },
};

/// The marker the agent's `UserPromptSubmit` hook writes when the session
/// takes an input (ADR-0043 decision 2), in the run directory.
pub const PROMPT_SUBMIT_MARKER: &str = "prompt-submit.json";

/// What `stats` reads outside the queue.
pub struct StatsSources<'a> {
    pub files: &'a dyn RunFiles,
    pub signals: &'a dyn AgentSignals,
    /// The `[stall]` of `dagq.toml`; `None` when there is no file.
    pub config_file: &'a dyn Fn() -> Result<Option<StallConfig>>,
    /// The `[conflicts]` of `dagq.toml`; `None` when there is no file.
    pub conflicts_file: &'a dyn Fn() -> Result<Option<ConflictConfig>>,
    /// Main's history since a unix second (see
    /// [`crate::application::Repository::main_history`]).
    pub history: &'a dyn Fn(i64) -> Result<MainHistory>,
    /// The summary of the host's load between two unix seconds (task
    /// 516); `None` reads none.
    pub host_metrics: Option<&'a dyn Fn(i64, i64) -> HostSummary>,
    /// The `[areas]` of `dagq.toml` and the landed commits' changes, for
    /// the runs' areas (ADR-t980-1).
    pub areas: &'a super::areas::AreaReader,
    /// The host's offset from UTC in seconds, east positive: when an ask
    /// counts as opened at night.
    pub utc_offset_secs: i64,
    /// Whether the queue's repository is dagq's source (ADR-t614-1): only
    /// then are the cargo-only measures shown.
    pub dagq_source: bool,
}

/// Main's history since the earliest event of `events` (a second before
/// it), so that the landings of any window before its first conflict are
/// counted too, or why it could not be read; none is read without a
/// conflict.
pub fn conflict_history(
    events: &[crate::domain::RunEvent],
    read: &dyn Fn(i64) -> Result<MainHistory>,
) -> History {
    let earliest = events
        .iter()
        .filter_map(|event| timestamp_millis(&event.created_at))
        .min();
    match earliest_conflict(events).and(earliest) {
        None => History::Read(MainHistory::default()),
        Some(millis) => match read(millis.div_euclid(1000) - 1) {
            Ok(history) => History::Read(history),
            Err(error) => History::Unavailable(format!("{error:#}")),
        },
    }
}

/// The `[conflicts]` thresholds and where they came from.
pub fn conflict_config(file: Option<ConflictConfig>) -> ConflictConfigReport {
    match file {
        Some(config) => ConflictConfigReport {
            config,
            source: "file",
        },
        None => ConflictConfigReport::default(),
    }
}

/// Per-run and per-goal times and the thresholds crossed, derived from
/// `run_events`. The idle alert looks at the live supervisors' slots at
/// `now`, `processes` telling which are alive. The running alerts read the
/// unfinished runs' directories through `sources`, and `workspace_mismatch`
/// the processes of their background wrappers through `processes` (its pid
/// and the start its handle recorded; no cmux, no heartbeat). Reads only.
pub fn stats(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    now: i64,
    query: &StatsQuery,
    sources: &StatsSources<'_>,
) -> Result<Stats> {
    let events = queue.all_events()?;
    let goals = queue.task_goals()?;
    let registrations = queue.supervisors()?;
    let slots: i64 = registrations
        .iter()
        .filter(|registration| {
            !SupervisorPulse::judge(registration, processes.alive(registration.pid), now).stale
        })
        .map(|registration| i64::from(registration.parallel))
        .sum();
    // A run that waits for a person, or waits to go back to its slot,
    // holds no slot (ADR-0071 decision 13). Every other run a registered
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
        if crate::domain::waiting::WaitState::of(&queue.run_events(run)?).is_none() {
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
    let ready = ready.saturating_sub(ready_in_draft_goals);
    let snapshot = SlotSnapshot {
        free_slots: slots - executing,
        candidates: queue.candidates()?.len(),
        ready,
    };
    let config = match StallConfig::loaded(&events) {
        Some(config) => StallConfigReport {
            config,
            source: "supervisor",
        },
        None => match (sources.config_file)()? {
            Some(config) => StallConfigReport {
                config,
                source: "file",
            },
            None => StallConfigReport {
                config: StallConfig::default(),
                source: "default",
            },
        },
    };
    let all_runs = queue.all_runs()?;
    let mut runs = Vec::new();
    for run in all_runs.iter().filter(|run| !finished(run.status())) {
        let mut live = live_run(run, sources)?;
        live.background_alive = background_alive(&events, run.id(), run.workspace_id(), processes);
        runs.push(live);
    }
    let live = LiveSnapshot {
        runs,
        config,
        history: conflict_history(&events, sources.history),
        conflicts: conflict_config((sources.conflicts_file)()?),
        draft_origins: queue.draft_origins()?,
        utc_offset_secs: sources.utc_offset_secs,
    };
    let mut stats = aggregate(&events, &goals, now, snapshot, query, &live);
    let titles = queue.task_titles()?;
    for run in &mut stats.runs {
        run.title = titles.get(&run.task_id).cloned();
    }
    with_changes(&mut stats, &queue.task_changes()?);
    // Only the listed runs' landings are read from Git.
    let listed: HashSet<&RunId> = stats.runs.iter().map(|run| &run.run_id).collect();
    let landings: Vec<crate::domain::RunEvent> = events
        .iter()
        .filter(|event| {
            event.kind == "run_integrated"
                && event
                    .run_id
                    .as_ref()
                    .is_some_and(|run| listed.contains(run))
        })
        .cloned()
        .collect();
    let areas = sources.areas.run_areas(&landings);
    with_areas(&mut stats, areas.as_ref());
    if !sources.dagq_source {
        without_cargo_measures(&mut stats);
    }
    if let Some(read) = sources.host_metrics {
        let (from, until) = stats.window_ms;
        stats.host = Some(read(from.div_euclid(1000), until.div_euclid(1000)));
    }
    Ok(stats)
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

/// The unfinished `run` with the write times of its idle marker, receipt
/// and prompt-submit marker. A file that is not there, or a run without a
/// directory, has none.
/// Whether the wrapper of the session the run `run` opened last runs, when
/// that session is a background wrapper's (ADR-t1404-1): its handle's pid
/// shows the start the handle recorded, so a pid another process took is
/// not taken for it. `first` is the run's record of its first session.
/// `None` for a session in a workspace.
fn background_alive(
    events: &[crate::domain::RunEvent],
    run: &RunId,
    first: Option<&str>,
    processes: &dyn ProcessControl,
) -> Option<bool> {
    use crate::domain::background_wrapper::{BackgroundHandle, last_session};
    let run_events = events
        .iter()
        .filter(|event| event.run_id.as_ref() == Some(run));
    let handle = BackgroundHandle::parse(last_session(run_events, first)?)?;
    Some(handle.is(handle.pid, processes.start_identity(handle.pid).as_deref()))
}

fn live_run(run: &TaskRun, sources: &StatsSources<'_>) -> Result<LiveRun> {
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
    sources: &StatsSources<'_>,
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
            kind: kind.to_owned(),
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
        let events = [of(&run, 1, "WS-FIRST"), of(&run, 2, &handle)];
        let alive =
            |processes: &Starts| background_alive(&events, &run, Some("WS-FIRST"), processes);
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
            background_alive(&events[..1], &run, Some("WS-FIRST"), &processes),
            None
        );
        assert_eq!(
            background_alive(&[], &run, Some(&handle), &processes),
            Some(true)
        );
        assert_eq!(
            background_alive(
                &[of(&other, 3, &handle)],
                &run,
                Some("WS-FIRST"),
                &processes
            ),
            None
        );
    }

    /// The history is read from the earliest event, not the earliest
    /// conflict, and only when there is a conflict; a failure to read it
    /// is reported, not raised.
    #[test]
    fn conflict_history_reads_from_the_earliest_event() {
        let quiet = [event(
            1,
            "run_claimed",
            json!({}),
            "1970-01-01T00:01:40.000Z",
        )];
        let never = |_: i64| -> Result<MainHistory> { unreachable!() };
        assert_eq!(
            conflict_history(&quiet, &never),
            History::Read(MainHistory::default())
        );
        let events = [
            quiet[0].clone(),
            event(
                2,
                "conflict_precheck",
                json!({"main": "m", "conflicts": ["a"]}),
                "1970-01-01T00:03:20.000Z",
            ),
        ];
        let since = std::cell::Cell::new(0);
        let read = |at: i64| {
            since.set(at);
            Ok(MainHistory::default())
        };
        conflict_history(&events, &read);
        assert_eq!(since.get(), 99);
        let failing = |_: i64| -> Result<MainHistory> { anyhow::bail!("no git") };
        assert_eq!(
            conflict_history(&events, &failing),
            History::Unavailable("no git".into())
        );
        assert_eq!(conflict_config(None).source, "default");
        assert_eq!(
            conflict_config(Some(ConflictConfig::default())).source,
            "file"
        );
    }
}
