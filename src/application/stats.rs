//! `stats` (ADR-0023 decision 5): the queue's reads that
//! [`crate::domain::stats::stats`] derives the per-run and per-goal times
//! and the thresholds crossed from, and what the running alerts (ADR-0043
//! decision 5) read outside the queue: the run directories' markers and
//! the processes of the runs' background wrappers (no cmux, ADR-t1433-1);
//! and main's Git history for `conflict_hotspots`. Main's history and the
//! areas are read from Git, not yet from the record of it the supervisor
//! keeps ([`crate::domain::stats::main_log`]).
//!
//! No Git runs in the queue service (ADR-t3102-3 decision 5): its client
//! asks `stats_inputs` ([`stats_inputs`]) what to read, reads it with its
//! own Git ([`prepare_read`]) and sends what it read with `stats` and
//! `kpi` ([`StatsGitReads`]), again while the service finds the queue's
//! events moved past what it read ([`call_read`]). A caller on the DB reads
//! Git itself.

use crate::domain::EventKind;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{
    AgentSignals, CoordinationReads, EventStore, PlanningRecords, ProcessControl, RunFiles, RunLog,
    RunReads, StatusFilter, SupervisorReads, TaskQuery, TaskReads,
};
use crate::domain::{
    GoalStatus, RunId, RunStatus, SupervisorPulse, TaskRun, TaskStatus,
    host_metrics::HostSummary,
    live_alerts::{EVENTS_PER_READ, LiveHistory, LiveInputs},
    stall::{BackgroundTask, IDLE_LOG, StallConfig, background_first_seen},
    stats::{
        ConflictConfig, ConflictConfigReport, History, LiveRun, LiveSnapshot, SlotSnapshot,
        StallConfigReport, Stats, StatsQuery,
        conflicts::{MainHistory, conflicted_files, earliest_conflict},
        outlier_thresholds, stats as aggregate, timestamp_millis, with_areas, with_changes,
        without_cargo_measures, without_rustc,
    },
};

/// The marker the agent's `UserPromptSubmit` hook writes when the session
/// takes an input (ADR-0043 decision 2), in the run directory.
pub const PROMPT_SUBMIT_MARKER: &str = "prompt-submit.json";

/// What refuses the events `stats` or `kpi` read, when they are not the
/// ones what was read with them was read for.
pub type EventsCheck<'a> = dyn Fn(&[crate::domain::RunEvent]) -> Result<()> + 'a;

/// A change of main as [`CompactHistory`] sends it: its path, its old
/// name, each as an index in `names`, and whether it deletes.
pub type SentChange = (u32, Option<u32>, bool);

/// What `stats` reads outside the queue.
pub struct StatsSources<'a> {
    pub files: &'a dyn RunFiles,
    pub signals: &'a dyn AgentSignals,
    /// The `[stall]` of `dagq.toml`; `None` when there is no file.
    pub config_file: &'a dyn Fn() -> Result<Option<StallConfig>>,
    /// The `[conflicts]` of `dagq.toml`; `None` when there is no file.
    pub conflicts_file: &'a dyn Fn() -> Result<Option<ConflictConfig>>,
    /// Main's history since a unix second (see
    /// [`crate::application::RepositoryReads::main_history`]).
    pub history: &'a dyn Fn(i64) -> Result<MainHistory>,
    /// The summary of the host's load between two unix seconds (task
    /// 516); `None` reads none.
    pub host_metrics: Option<&'a dyn Fn(i64, i64) -> HostSummary>,
    /// The `[areas]` of `dagq.toml` and the landed commits' changes, for
    /// the runs' areas (ADR-t980-1).
    pub areas: &'a super::areas::AreaReader,
    /// Refuses the events `stats` aggregates when they are not the ones
    /// `history` and `areas` were read for (the queue service's
    /// [`ServedReads::check`]); `None` checks none.
    pub fresh: Option<&'a EventsCheck<'a>>,
    /// The host's offset from UTC in seconds, east positive: when an ask
    /// counts as opened at night.
    pub utc_offset_secs: i64,
    /// Whether the rules that work for the repository's `dagq.toml` count
    /// `integrate`'s checks
    /// ([`crate::domain::command_rules::EffectiveRules::counts_checks`]):
    /// only then are the counts of what `integrate` repeats shown.
    pub counts_checks: bool,
    /// Whether the repository's profiles record the host's Rust toolchain
    /// at a claim
    /// ([`crate::domain::toolchain::Toolchains::records_rust_toolchain`]):
    /// only then are the runs' `rustc` and the runs per `rustc` shown.
    pub rust_toolchain: bool,
}

/// The unix second main's history is read since for `conflict_hotspots`:
/// a second before the earliest event of `events`, so that the landings
/// of any window before its first conflict are counted too; `None`, none
/// is read, without a conflict.
pub fn history_since(events: &[crate::domain::RunEvent]) -> Option<i64> {
    let earliest = events
        .iter()
        .filter_map(|event| timestamp_millis(&event.created_at))
        .min();
    earliest_conflict(events)
        .and(earliest)
        .map(|millis| millis.div_euclid(1000) - 1)
}

/// Main's history since [`history_since`] of `events`, or why it could not
/// be read; none is read without a conflict.
pub fn conflict_history(
    events: &[crate::domain::RunEvent],
    read: &dyn Fn(i64) -> Result<MainHistory>,
) -> History {
    match history_since(events) {
        None => History::Read(MainHistory::default()),
        Some(since) => read_history(read, since),
    }
}

/// `read` of main's history since `since`, or why it could not be read.
fn read_history(read: &dyn Fn(i64) -> Result<MainHistory>, since: i64) -> History {
    match read(since) {
        Ok(history) => History::Read(history),
        Err(error) => History::Unavailable(format!("{error:#}")),
    }
}

/// What the queue service's client reads with Git before it sends
/// `stats` (with `--ledger` too) or `kpi`, as no Git runs in the service
/// (ADR-t3102-3 decision 5): the answer of `stats_inputs`, made by
/// [`stats_inputs`] of the queue's events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsInputs {
    /// The main checkout of the repository the queue is bound to, as the
    /// service finds it from the binding with no Git run, whose
    /// `dagq.toml` the service reads and whose Git the client reads; none
    /// for a queue bound to none or to one whose main checkout is not
    /// found so.
    pub checkout: Option<PathBuf>,
    /// The commit of every landed run of the queue's events
    /// ([`crate::domain::areas::landed_commits`]), in their order: the
    /// client judges their areas and sends them in this order.
    pub landed: Vec<String>,
    /// The second main's history is read since ([`history_since`]); none
    /// without a conflict, when none is read.
    pub history_since: Option<i64>,
    /// The files the conflict events name, which the client narrows the
    /// history it sends to ([`MainHistory::narrowed_to`]).
    pub conflicted: Vec<String>,
    /// The digest of the rest: what the client sends back with what it
    /// read, so that the service reads it only with events that give the
    /// same inputs ([`StatsGitReads::served`]).
    pub version: String,
}

/// `stats_inputs` of a queue whose bound main checkout is `checkout`, with
/// `events`. No Git runs.
pub fn stats_inputs(checkout: Option<PathBuf>, events: &[crate::domain::RunEvent]) -> StatsInputs {
    let mut inputs = StatsInputs {
        checkout,
        landed: crate::domain::areas::landed_commits(events)
            .into_iter()
            .map(|(_, commit)| commit)
            .collect(),
        history_since: history_since(events),
        conflicted: conflicted_files(events),
        version: String::new(),
    };
    inputs.version = inputs.digest();
    inputs
}

impl StatsInputs {
    /// The SHA-256 of these inputs but the version, in hex.
    fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(&(
            &self.checkout,
            &self.landed,
            self.history_since,
            &self.conflicted,
        ))
        .unwrap_or_default();
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// Why the queue service refuses a `stats` or `kpi` whose Git reads were
/// made from other inputs than the queue's events give now: an event that
/// changes them (a landing, a conflict) came after its `stats_inputs`. The
/// client asks again ([`call_read`]).
pub const STALE: &str =
    "the Git reads sent with it were made for other events than the queue has now";

/// Why the queue service refuses a `stats` or `kpi` sent with no Git
/// reads: its client is older than the service, and reads no Git for it.
pub const NOT_READ: &str = "it is sent with no Git reads, which a dagq older than the queue \
     service sends: update the dagq that sends it";

/// What the queue service's client read with Git in the main checkout
/// [`StatsInputs`] names and sends with `stats` and `kpi`; the service
/// reads these in place of Git, with the events whose inputs have its
/// `inputs` version only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatsGitReads {
    /// The [`StatsInputs::version`] these were read for.
    pub inputs: String,
    /// The areas of the landed commits; none without `[areas]`.
    #[serde(default)]
    pub areas: Option<SentAreas>,
    /// Main's history since [`StatsInputs::history_since`], narrowed to
    /// the conflicted files, or why it could not be read; none when none
    /// is read (no conflict, or a `kpi`).
    #[serde(default)]
    pub history: Option<SentHistory>,
}

/// The areas of the landed commits, each set of areas once: `of` has one
/// item per [`StatsInputs::landed`], in its order, the index in `sets` of
/// its commit's areas, none for a commit Git does not have.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SentAreas {
    pub sets: Vec<Vec<String>>,
    pub of: Vec<Option<u32>>,
}

/// Main's history as the client sends it: read, or why not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SentHistory {
    Read(CompactHistory),
    Unavailable(String),
}

/// A [`MainHistory`] with only what `conflict_hotspots` reads, small enough
/// for a request: each commit's second as the difference from the one
/// before (the first's from 0), the changes of the commits that have any
/// by the commit's index, each as its path, its old name and whether it
/// deletes, and the paths main has now, every name as its index in
/// `names`. The commits' IDs are not sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactHistory {
    pub names: Vec<String>,
    pub at: Vec<i64>,
    pub changes: Vec<(usize, Vec<SentChange>)>,
    pub paths: Vec<u32>,
}

impl CompactHistory {
    fn of(history: &MainHistory) -> Self {
        let mut names: Vec<String> = Vec::new();
        let mut index: HashMap<String, u32> = HashMap::new();
        let mut name = |name: &str| -> u32 {
            *index.entry(name.to_owned()).or_insert_with(|| {
                names.push(name.to_owned());
                u32::try_from(names.len() - 1).unwrap_or(u32::MAX)
            })
        };
        let mut last = 0;
        let at = history
            .commits
            .iter()
            .map(|commit| {
                let delta = commit.at - last;
                last = commit.at;
                delta
            })
            .collect();
        let changes = history
            .commits
            .iter()
            .enumerate()
            .filter(|(_, commit)| !commit.changes.is_empty())
            .map(|(at, commit)| {
                let changes = commit
                    .changes
                    .iter()
                    .map(|change| {
                        (
                            name(&change.path),
                            change.from.as_deref().map(&mut name),
                            change.deleted,
                        )
                    })
                    .collect();
                (at, changes)
            })
            .collect();
        let mut now: Vec<&String> = history.paths.iter().collect();
        now.sort();
        let paths = now.into_iter().map(|path| name(path)).collect();
        Self {
            names,
            at,
            changes,
            paths,
        }
    }

    /// The history this was made of, with no commit IDs; refused when its
    /// names, each counted as often as it is named, come to more than
    /// [`DECODED_BYTES`], or a second is past what a unix millisecond holds.
    fn history(&self) -> Result<MainHistory> {
        use crate::domain::stats::conflicts::{MainChange, MainCommit};
        let len = |index: u32| self.names.get(index as usize).map_or(0, String::len);
        let named = self
            .changes
            .iter()
            .flat_map(|(_, changes)| changes)
            .map(|(path, from, _)| len(*path) + from.map_or(0, len))
            .chain(self.paths.iter().map(|path| len(*path)))
            .fold(0usize, usize::saturating_add);
        within_decoded(named, "main's history")?;
        let name = |index: u32| -> Result<String> {
            self.names
                .get(index as usize)
                .cloned()
                .context("main's history sent names a path it has not")
        };
        let mut at: i64 = 0;
        let mut commits: Vec<MainCommit> = Vec::with_capacity(self.at.len());
        for delta in &self.at {
            at = at
                .checked_add(*delta)
                .filter(|at| at.checked_mul(1000).is_some())
                .context("main's history sent has a commit's second out of range")?;
            commits.push(MainCommit {
                sha: String::new(),
                at,
                changes: Vec::new(),
            });
        }
        for (index, changes) in &self.changes {
            commits
                .get_mut(*index)
                .context("main's history sent names a commit it has not")?
                .changes = changes
                .iter()
                .map(|(path, from, deleted)| {
                    Ok(MainChange {
                        path: name(*path)?,
                        from: from.map(name).transpose()?,
                        deleted: *deleted,
                    })
                })
                .collect::<Result<_>>()?;
        }
        Ok(MainHistory {
            commits,
            paths: self
                .paths
                .iter()
                .map(|path| name(*path))
                .collect::<Result<_>>()?,
        })
    }
}

/// The most bytes of names the queue service makes of the Git reads a
/// `stats` or `kpi` was sent with, each name counted as often as they name
/// it: a request names a name by its index, so that one of
/// [`crate::domain::queue_service::MAX_REQUEST_BYTES`] could name a long
/// one many times over.
pub const DECODED_BYTES: usize = 16 * crate::domain::queue_service::MAX_REQUEST_BYTES;

/// That `bytes` of names, of `what`, are within [`DECODED_BYTES`].
fn within_decoded(bytes: usize, what: &str) -> Result<()> {
    anyhow::ensure!(
        bytes <= DECODED_BYTES,
        "{what} sent name {bytes} bytes, more than the {DECODED_BYTES} the queue service makes"
    );
    Ok(())
}

/// What the queue service reads `stats` and `kpi` with from the
/// [`StatsGitReads`] their client sent ([`StatsGitReads::served`]).
pub struct ServedReads {
    pub areas: super::areas::AreaReader,
    since: Option<i64>,
    history: Option<History>,
    version: String,
    checkout: Option<PathBuf>,
}

impl StatsGitReads {
    /// These reads, for the queue whose inputs are `inputs` now (the
    /// service's [`stats_inputs`]): refused ([`STALE`]) when they were read
    /// for other inputs.
    pub fn served(&self, inputs: &StatsInputs) -> Result<ServedReads> {
        anyhow::ensure!(self.inputs == inputs.version, "{STALE}");
        let areas = match &self.areas {
            None => None,
            Some(areas) => {
                anyhow::ensure!(
                    areas.of.len() == inputs.landed.len(),
                    "the areas sent are of {} landed commits, not the {} the inputs name",
                    areas.of.len(),
                    inputs.landed.len()
                );
                let named = areas
                    .of
                    .iter()
                    .flatten()
                    .map(|set| {
                        areas.sets.get(*set as usize).map_or(0, |set| {
                            set.iter().map(String::len).fold(0, usize::saturating_add)
                        })
                    })
                    .fold(0usize, usize::saturating_add);
                within_decoded(named, "the areas")?;
                let mut judged = HashMap::new();
                for (commit, set) in inputs.landed.iter().zip(&areas.of) {
                    if let Some(set) = set {
                        let set = areas
                            .sets
                            .get(*set as usize)
                            .context("the areas sent name a set they have not")?;
                        judged.insert(commit.clone(), set.clone());
                    }
                }
                Some(judged)
            }
        };
        let history = match &self.history {
            None => None,
            Some(SentHistory::Read(history)) => Some(History::Read(history.history()?)),
            Some(SentHistory::Unavailable(reason)) => Some(History::Unavailable(reason.clone())),
        };
        Ok(ServedReads {
            areas: super::areas::AreaReader::judged(areas),
            since: inputs.history_since,
            history,
            version: inputs.version.clone(),
            checkout: inputs.checkout.clone(),
        })
    }
}

impl ServedReads {
    /// Main's history since `since` as [`StatsSources::history`] reads it:
    /// the one the client read since the same second, or the reason it
    /// could not; refused for another second, or with none read.
    pub fn history(&self, since: i64) -> Result<MainHistory> {
        anyhow::ensure!(
            self.since == Some(since),
            "main's history was read since {:?}, not {since}",
            self.since
        );
        match &self.history {
            Some(History::Read(history)) => Ok(history.clone()),
            Some(History::Unavailable(reason)) => Err(anyhow::Error::msg(reason.clone())),
            None => anyhow::bail!("the queue service's client read no main history"),
        }
    }

    /// That `events`, the ones `stats` or `kpi` aggregate, give the inputs
    /// these were read for ([`STALE`] when not).
    pub fn check(&self, events: &[crate::domain::RunEvent]) -> Result<()> {
        anyhow::ensure!(
            stats_inputs(self.checkout.clone(), events).version == self.version,
            "{STALE}"
        );
        Ok(())
    }
}

/// What the queue service's client reads [`StatsGitReads`] with, outside
/// the service (ADR-t3102-3 decision 5).
pub struct StatsClient<'a> {
    /// The `[areas]` of a checkout's `dagq.toml` and its Git.
    pub areas: &'a dyn Fn(&Path) -> Result<super::areas::AreaReader>,
    /// Main's history of a checkout since a unix second (Git).
    pub history: &'a dyn Fn(&Path, i64) -> Result<MainHistory>,
}

/// Why main's history is not read for a queue bound to no main checkout.
pub const NO_CHECKOUT: &str = "the queue is bound to no repository checkout";

/// How many times a client sends `stats` or `kpi` with Git reads it made
/// again when the service finds them [`STALE`].
pub const ATTEMPTS: usize = 3;

/// What the queue service answers `read` (a `stats` or `kpi`) sent with
/// the Git reads [`prepare_read`] makes, by `call`; made and sent again,
/// up to [`ATTEMPTS`] times in all, while the service finds them
/// [`STALE`]. Any other read is sent as it is.
pub fn call_read(
    read: &super::queue_reads::QueueRead,
    client: &StatsClient<'_>,
    mut call: impl FnMut(&super::queue_reads::QueueRead) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    let mut attempt = 1;
    loop {
        let mut sent = read.clone();
        prepare_read(&mut sent, client, &mut call)?;
        match call(&sent) {
            Err(error) if attempt < ATTEMPTS && format!("{error:#}").contains(STALE) => {
                attempt += 1;
            }
            answered => return answered,
        }
    }
}

/// `read`, a `stats` or `kpi` its client sends to the queue service (a
/// job's among them, [`super::queue_reads::JobRead`]), with the
/// [`StatsGitReads`] `client` reads in the main checkout the
/// [`StatsInputs`] `call` answers to `stats_inputs` (a job's to its own)
/// names: the areas of the landed commits (none without `[areas]` or a
/// main checkout), and for a `stats` main's history since
/// [`StatsInputs::history_since`] narrowed to the conflicted files, or why
/// it was not read ([`NO_CHECKOUT`] without a main checkout, as on the
/// DB). Params larger than a request takes are refused with their size,
/// and nothing is sent. Any other read is left as it is, and nothing is
/// asked.
pub fn prepare_read(
    read: &mut super::queue_reads::QueueRead,
    client: &StatsClient<'_>,
    call: impl FnOnce(&super::queue_reads::QueueRead) -> Result<serde_json::Value>,
) -> Result<()> {
    use super::queue_reads::{JobRead, QueueRead, StatsInputsRead};
    let (inputs_read, stats) = match read {
        QueueRead::Stats(_) => (QueueRead::StatsInputs(StatsInputsRead::default()), true),
        QueueRead::Kpi(_) => (QueueRead::StatsInputs(StatsInputsRead::default()), false),
        QueueRead::Job(JobRead::Stats { .. }) => (QueueRead::Job(JobRead::StatsInputs), true),
        QueueRead::Job(JobRead::Kpi { .. } | JobRead::ObserverKpi { .. }) => {
            (QueueRead::Job(JobRead::StatsInputs), false)
        }
        _ => return Ok(()),
    };
    let inputs: StatsInputs =
        serde_json::from_value(call(&inputs_read)?).context("the inputs of stats do not read")?;
    let checkout = inputs.checkout.as_deref();
    let areas = match checkout {
        Some(checkout) => (client.areas)(checkout)?.commit_areas(&inputs.landed),
        None => None,
    }
    .map(|judged| {
        let mut sent = SentAreas::default();
        let mut index: HashMap<Vec<String>, u32> = HashMap::new();
        sent.of = inputs
            .landed
            .iter()
            .map(|commit| {
                let set = judged.get(commit)?;
                Some(*index.entry(set.clone()).or_insert_with(|| {
                    sent.sets.push(set.clone());
                    u32::try_from(sent.sets.len() - 1).unwrap_or(u32::MAX)
                }))
            })
            .collect();
        sent
    });
    // `kpi` counts no conflict hotspot: main's history is read for `stats`
    // alone.
    let since = inputs.history_since.filter(|_| stats);
    let history = since.map(|since| match checkout {
        Some(checkout) => match read_history(&|since| (client.history)(checkout, since), since) {
            History::Read(history) => {
                SentHistory::Read(CompactHistory::of(&history.narrowed_to(&inputs.conflicted)))
            }
            History::Unavailable(reason) => SentHistory::Unavailable(reason),
        },
        None => SentHistory::Unavailable(NO_CHECKOUT.to_owned()),
    });
    let git = Some(StatsGitReads {
        inputs: inputs.version.clone(),
        areas,
        history,
    });
    match read {
        QueueRead::Stats(stats) => stats.git = git,
        QueueRead::Kpi(kpi) => kpi.git = git,
        QueueRead::Job(
            JobRead::Stats { git: slot, .. }
            | JobRead::Kpi { git: slot, .. }
            | JobRead::ObserverKpi { git: slot },
        ) => *slot = git,
        _ => {}
    }
    fit_request(
        read,
        crate::domain::queue_service::MAX_REQUEST_BYTES
            - crate::domain::telemetry::REQUEST_ENVELOPE_BYTES,
    )
}

/// That `read`'s params fit a request of at most `max` bytes: params
/// larger are refused with their size, and nothing is sent.
pub fn fit_request(read: &super::queue_reads::QueueRead, max: usize) -> Result<()> {
    let (use_case, params) = read.request();
    let size = serde_json::to_vec(&params)?.len();
    anyhow::ensure!(
        size <= max,
        "the request of {} is {size} bytes, larger than the {max} a request to the queue \
         service takes",
        use_case.as_str()
    );
    Ok(())
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
/// The times come from the [`EventStore`]; the other ports are what the
/// judgments of now and the joins still read from the StateStore (the
/// slots, the leases, the unfinished runs, the ready tasks and the tasks'
/// goals, titles, changes and drafts' origins) until the ledger replaces
/// them (ADR-t1662-2 D1).
pub fn stats(
    queue: &(
         impl EventStore
         + PlanningRecords
         + CoordinationReads
         + SupervisorReads
         + RunReads
         + TaskReads
         + ?Sized
     ),
    processes: &dyn ProcessControl,
    now: i64,
    query: &StatsQuery,
    sources: &StatsSources<'_>,
) -> Result<Stats> {
    let inputs = inputs(queue, processes, now, sources)?;
    let mut stats = inputs.aggregate(&inputs.events, now, query);
    finish(queue, &inputs.events, &mut stats, sources)?;
    Ok(stats)
}

/// What [`stats`] aggregates beside the events: the tasks' goals, the
/// slots now and the judgments of now with what they read.
pub(crate) struct Inputs {
    pub events: Vec<crate::domain::RunEvent>,
    goals: HashMap<crate::domain::TaskId, Option<crate::domain::GoalId>>,
    snapshot: SlotSnapshot,
    live: LiveSnapshot,
}

impl Inputs {
    /// `stats`' aggregate of `events` (the EventStore's, or those the
    /// ledger's rows give back) with these inputs.
    pub fn aggregate(
        &self,
        events: &[crate::domain::RunEvent],
        now: i64,
        query: &StatsQuery,
    ) -> Stats {
        aggregate(events, &self.goals, now, self.snapshot, query, &self.live)
    }
}

/// [`stats`]' reads of the queue and of `sources` but the joins.
pub(crate) fn inputs(
    queue: &(
         impl EventStore
         + PlanningRecords
         + CoordinationReads
         + SupervisorReads
         + RunReads
         + TaskReads
         + ?Sized
     ),
    processes: &dyn ProcessControl,
    now: i64,
    sources: &StatsSources<'_>,
) -> Result<Inputs> {
    let events = queue.all_events()?;
    if let Some(fresh) = sources.fresh {
        fresh(&events)?;
    }
    let goals = queue.task_goals()?;
    let snapshot = slot_snapshot(queue, processes, now)?;
    let config = stall_config(&events, sources.config_file)?;
    let (runs, _) = live_runs(&queue.all_runs()?, processes, &events, &sources.live())?;
    let live = LiveSnapshot {
        runs,
        config,
        history: conflict_history(&events, sources.history),
        conflicts: conflict_config((sources.conflicts_file)()?),
        draft_origins: queue.draft_origins()?,
        utc_offset_secs: sources.utc_offset_secs,
    };
    Ok(Inputs {
        events,
        goals,
        snapshot,
        live,
    })
}

/// [`stats`]' joins of its aggregate `stats` of `events`: the titles,
/// changes and areas of the runs, the cargo measures and the host's load.
fn finish(
    queue: &(impl PlanningRecords + ?Sized),
    events: &[crate::domain::RunEvent],
    stats: &mut Stats,
    sources: &StatsSources<'_>,
) -> Result<()> {
    let titles = queue.task_titles()?;
    for run in &mut stats.runs {
        run.title = titles.get(&run.task_id).cloned();
    }
    with_changes(stats, &queue.task_changes()?);
    // Only the listed runs' landings are read from Git.
    let listed: HashSet<&RunId> = stats.runs.iter().map(|run| &run.run_id).collect();
    let landings: Vec<crate::domain::RunEvent> = events
        .iter()
        .filter(|event| {
            event.kind == EventKind::RunIntegrated
                && event
                    .run_id
                    .as_ref()
                    .is_some_and(|run| listed.contains(run))
        })
        .cloned()
        .collect();
    let areas = sources.areas.run_areas(&landings);
    with_areas(stats, areas.as_ref());
    if !sources.counts_checks {
        without_cargo_measures(stats);
    }
    if !sources.rust_toolchain {
        without_rustc(stats);
    }
    if let Some(read) = sources.host_metrics {
        let (from, until) = stats.window_ms;
        stats.host = Some(read(from.div_euclid(1000), until.div_euclid(1000)));
    }
    Ok(())
}

/// What the judgments of now read outside the queue: the run directories'
/// markers through `files` and `signals`, and the `[stall]` of `dagq.toml`
/// (`None` when there is no file).
pub struct LiveSources<'a> {
    pub files: &'a dyn RunFiles,
    pub signals: &'a dyn AgentSignals,
    pub config_file: &'a dyn Fn() -> Result<Option<StallConfig>>,
}

impl StatsSources<'_> {
    fn live(&self) -> LiveSources<'_> {
        LiveSources {
            files: self.files,
            signals: self.signals,
            config_file: self.config_file,
        }
    }
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
        config: stall_config(events, sources.config_file)?,
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
    let (all, events) = carried_events(queue, history)?;
    let (runs, positions) = live_runs(&all, processes, &events, sources)?;
    let outliers = history.outlier_thresholds(&queue.task_goals()?, &runs);
    let inputs = LiveInputs {
        slots: slot_snapshot(queue, processes, now)?,
        config: stall_config(&events, sources.config_file)?,
        runs,
        outliers,
        positions,
    };
    Ok((inputs, events))
}

/// Fold into `history` the events past its cursor, [`EVENTS_PER_READ`] at
/// a time, then read the events of the runs not finished: every run of the
/// queue, and the events the judgments of now read, which judge as the
/// whole history does ([`LiveHistory::events`]). Only the first pass of a
/// process reads every event, a page at a time.
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
        runs.extend(queue.run_events(run.id())?);
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

/// The thresholds of the running alerts: the latest supervisor's, else
/// the file's, else the defaults.
fn stall_config(
    events: &[crate::domain::RunEvent],
    config_file: &dyn Fn() -> Result<Option<StallConfig>>,
) -> Result<StallConfigReport> {
    Ok(match StallConfig::loaded(events) {
        Some(config) => StallConfigReport {
            config,
            source: "supervisor",
        },
        None => match config_file()? {
            Some(config) => StallConfigReport {
                config,
                source: "file",
            },
            None => StallConfigReport {
                config: StallConfig::default(),
                source: "default",
            },
        },
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

    fn landed(id: i64, run: u32, commit: &str) -> RunEvent {
        RunEvent {
            run_id: Some(RunId::new(format!("{run:08x}-c873-4cec-aee3-ee7f07f52e4a")).unwrap()),
            ..event(
                id,
                "run_integrated",
                json!({"commit": commit}),
                "1970-01-01T00:01:40.000Z",
            )
        }
    }

    fn conflict(id: i64, file: &str) -> RunEvent {
        event(
            id,
            "integration_deferred",
            json!({"main": "m", "conflicts": [file]}),
            "1970-01-01T00:03:20.000Z",
        )
    }

    fn whole_history() -> MainHistory {
        use crate::domain::stats::conflicts::{MainChange, MainCommit};
        let change = |path: &str, from: Option<&str>| MainChange {
            path: path.into(),
            from: from.map(str::to_owned),
            deleted: false,
        };
        MainHistory {
            commits: vec![
                MainCommit {
                    sha: "c1".into(),
                    at: 150,
                    changes: vec![change("a", None), change("z", None)],
                },
                MainCommit {
                    sha: "c2".into(),
                    at: 190,
                    changes: vec![change("z", None)],
                },
                MainCommit {
                    sha: "c3".into(),
                    at: 210,
                    changes: vec![change("b2", Some("b"))],
                },
            ],
            paths: ["a", "b2", "z"].map(str::to_owned).into(),
        }
    }

    /// A client that finds `[areas]` (`a` → docs) and reads `history`.
    fn client_of<'a>(
        areas: &'a dyn Fn(&Path) -> Result<super::super::areas::AreaReader>,
        history: &'a dyn Fn(&Path, i64) -> Result<MainHistory>,
    ) -> StatsClient<'a> {
        StatsClient { areas, history }
    }

    fn mapped(_: &Path) -> Result<super::super::areas::AreaReader> {
        use crate::domain::areas::AreaMap;
        Ok(super::super::areas::AreaReader {
            map: Some(AreaMap::new(vec![("docs".into(), vec!["a".into()])]).unwrap()),
            changed: std::sync::Arc::new(|commits: &[String]| {
                Ok(commits
                    .iter()
                    .filter(|commit| *commit != "gone")
                    .map(|commit| {
                        let paths = if commit == "c2" { "z" } else { "a" };
                        (commit.clone(), vec![paths.to_owned()])
                    })
                    .collect())
            }),
            judged: None,
        })
    }

    /// `read` (`stats` or `kpi`) as `prepare_read` makes it from the inputs
    /// of `events` and `client`, and its Git reads as the service reads
    /// them back from the request.
    fn sent(
        read: crate::domain::queue_service::UseCase,
        events: &[RunEvent],
        client: &StatsClient<'_>,
    ) -> (super::super::queue_reads::QueueRead, Option<StatsGitReads>) {
        use super::super::queue_reads::{KpiRead, QueueRead, StatsRead};
        let inputs = stats_inputs(Some(PathBuf::from("/repo")), events);
        let mut read = QueueRead::parse(read, &json!({})).unwrap().unwrap();
        prepare_read(&mut read, client, |asked| {
            assert!(matches!(asked, QueueRead::StatsInputs(_)));
            Ok(serde_json::to_value(&inputs).unwrap())
        })
        .unwrap();
        let (use_case, params) = read.request();
        let git = match QueueRead::parse(use_case, &params).unwrap() {
            Some(QueueRead::Stats(StatsRead { git, .. }) | QueueRead::Kpi(KpiRead { git, .. })) => {
                git
            }
            other => panic!("{other:?}"),
        };
        (read, git)
    }

    /// `stats_inputs` names the landed commits, the second main's history
    /// is read since by [`conflict_history`]'s rule and the conflicted
    /// files, with a version that a landing or a conflict changes and
    /// another event does not.
    #[test]
    fn stats_inputs_name_what_the_client_reads_with_git_and_their_version() {
        let quiet = [landed(1, 1, "c1")];
        let inputs = stats_inputs(Some(PathBuf::from("/repo")), &quiet);
        assert_eq!(inputs.landed, ["c1"]);
        assert_eq!((inputs.history_since, inputs.conflicted.len()), (None, 0));
        let events = [landed(1, 1, "c1"), conflict(2, "b"), conflict(3, "a")];
        let inputs = stats_inputs(Some(PathBuf::from("/repo")), &events);
        assert_eq!(inputs.history_since, Some(99));
        assert_eq!(inputs.history_since, history_since(&events));
        assert_eq!(inputs.conflicted, ["a", "b"]);
        let note = event(4, "note", json!({}), "1970-01-01T00:05:00.000Z");
        let mut more = events.to_vec();
        more.push(note);
        assert_eq!(
            stats_inputs(Some(PathBuf::from("/repo")), &more).version,
            inputs.version
        );
        for changed in [landed(5, 2, "c2"), conflict(5, "c")] {
            let mut more = events.to_vec();
            more.push(changed);
            assert_ne!(
                stats_inputs(Some(PathBuf::from("/repo")), &more).version,
                inputs.version
            );
        }
        assert_ne!(stats_inputs(None, &events).version, inputs.version);
    }

    /// The client of `stats` reads with its Git what `stats_inputs` names
    /// and sends it, and the service reads it back as it would have read
    /// Git: the areas of each landed commit (none for one Git has not),
    /// and main's history narrowed to the conflicted files, which gives
    /// the same hotspots; `kpi` is sent no history. A history the client
    /// could not read is the same `Unavailable`, a queue with no main
    /// checkout gives the DB's reason and no areas, and another read asks
    /// nothing.
    #[test]
    fn a_client_reads_git_for_stats_and_kpi_as_the_service_on_the_db_would() {
        use crate::domain::queue_service::UseCase;
        use crate::domain::stats::{ConflictConfigReport, conflicts::conflict_hotspots};
        let events = [
            landed(1, 1, "c1"),
            landed(2, 2, "c2"),
            landed(3, 3, "gone"),
            conflict(4, "a"),
            conflict(5, "b"),
        ];
        let inputs = stats_inputs(Some(PathBuf::from("/repo")), &events);
        let read = |checkout: &Path, since: i64| {
            assert_eq!((checkout, since), (Path::new("/repo"), 99));
            Ok(whole_history())
        };
        let client = client_of(&mapped, &read);
        let (_, git) = sent(UseCase::Stats, &events, &client);
        let served = git.unwrap().served(&inputs).unwrap();
        let on_db = mapped(Path::new("/repo")).unwrap();
        assert_eq!(served.areas.run_areas(&events), on_db.run_areas(&events));
        assert_eq!(
            served.areas.run_areas(&events).unwrap().len(),
            2,
            "a commit Git has not has no areas"
        );
        let hotspots = |history: History| {
            serde_json::to_value(conflict_hotspots(
                &events,
                crate::domain::EventId::new(0),
                crate::domain::EventId::new(5),
                |_| true,
                &history,
                ConflictConfigReport::default(),
            ))
            .unwrap()
        };
        let read_back = conflict_history(&events, &|since| served.history(since));
        assert_eq!(
            hotspots(read_back),
            hotspots(conflict_history(&events, &|_| Ok(whole_history())))
        );
        assert!(served.history(98).is_err(), "another second");
        let (_, kpi) = sent(UseCase::Kpi, &events, &client);
        let kpi = kpi.unwrap();
        assert_eq!(kpi.history, None, "kpi reads no main history");
        assert!(kpi.areas.is_some());

        let failing = |_: &Path, _: i64| -> Result<MainHistory> { anyhow::bail!("no git") };
        let client = client_of(&mapped, &failing);
        let served = sent(UseCase::Stats, &events, &client)
            .1
            .unwrap()
            .served(&inputs)
            .unwrap();
        assert_eq!(
            conflict_history(&events, &|since| served.history(since)),
            conflict_history(&events, &|_| anyhow::bail!("no git"))
        );

        // No main checkout: no areas, and the DB's reason for the history.
        let unbound = stats_inputs(None, &events);
        let mut read = super::super::queue_reads::QueueRead::parse(UseCase::Stats, &json!({}))
            .unwrap()
            .unwrap();
        prepare_read(&mut read, &client, |_| {
            Ok(serde_json::to_value(&unbound).unwrap())
        })
        .unwrap();
        let super::super::queue_reads::QueueRead::Stats(stats) = &read else {
            unreachable!()
        };
        let served = stats.git.as_ref().unwrap().served(&unbound).unwrap();
        assert_eq!(served.areas.run_areas(&events), None);
        assert_eq!(
            conflict_history(&events, &|since| served.history(since)),
            History::Unavailable(NO_CHECKOUT.into())
        );

        let mut other = super::super::queue_reads::QueueRead::parse(UseCase::List, &json!({}))
            .unwrap()
            .unwrap();
        let before = other.clone();
        prepare_read(&mut other, &client, |_| unreachable!()).unwrap();
        assert_eq!(other, before);
    }

    /// A job's `stats`, `kpi` and observer KPI are sent with the Git reads
    /// made from the inputs its own `stats_inputs` names, as a command
    /// line's are: main's history for `stats` alone, the areas for each.
    #[test]
    fn a_jobs_reads_carry_the_git_reads_made_from_its_own_inputs() {
        use super::super::queue_reads::{JobRead, QueueRead};
        let events = [landed(1, 1, "c1"), conflict(2, "a")];
        let inputs = stats_inputs(Some(PathBuf::from("/repo")), &events);
        let read = |_: &Path, _: i64| Ok(whole_history());
        let client = client_of(&mapped, &read);
        let prepared = |job: JobRead| {
            let mut read = QueueRead::Job(job);
            prepare_read(&mut read, &client, |asked| {
                assert_eq!(asked, &QueueRead::Job(JobRead::StatsInputs));
                Ok(serde_json::to_value(&inputs).unwrap())
            })
            .unwrap();
            match read {
                QueueRead::Job(
                    JobRead::Stats { git, .. }
                    | JobRead::Kpi { git, .. }
                    | JobRead::ObserverKpi { git },
                ) => git.unwrap(),
                other => panic!("{other:?}"),
            }
        };
        let stats = prepared(JobRead::Stats {
            query: StatsQuery::default(),
            git: None,
        });
        assert!(stats.history.is_some());
        stats.served(&inputs).unwrap().check(&events).unwrap();
        for job in [
            JobRead::Kpi {
                query: crate::domain::kpi::KpiQuery::default(),
                git: None,
            },
            JobRead::ObserverKpi { git: None },
        ] {
            let git = prepared(job);
            assert_eq!(git.history, None, "kpi reads no main history");
            assert!(git.areas.is_some());
        }
        let mut inputs_read = QueueRead::Job(JobRead::StatsInputs);
        prepare_read(&mut inputs_read, &client, |_| unreachable!()).unwrap();
        assert_eq!(inputs_read, QueueRead::Job(JobRead::StatsInputs));
    }

    /// Reads made for other inputs than the events give are refused: by
    /// the service before it reads them, and by the check of the events
    /// `stats` and `kpi` aggregate (a landing or a conflict after
    /// `stats_inputs`); the client then makes them again and sends them,
    /// up to [`ATTEMPTS`] times.
    #[test]
    fn reads_of_other_events_are_refused_and_made_again() {
        use crate::domain::queue_service::UseCase;
        let events = [landed(1, 1, "c1"), conflict(2, "a")];
        let inputs = stats_inputs(Some(PathBuf::from("/repo")), &events);
        let read = |_: &Path, _: i64| Ok(whole_history());
        let client = client_of(&mapped, &read);
        let git = sent(UseCase::Stats, &events, &client).1.unwrap();
        let served = git.served(&inputs).unwrap();
        served.check(&events).unwrap();
        for later in [landed(3, 2, "c2"), conflict(3, "b")] {
            let mut moved = events.to_vec();
            moved.push(later);
            let error = served.check(&moved).unwrap_err();
            assert!(format!("{error:#}").contains(STALE), "{error:#}");
            let now = stats_inputs(Some(PathBuf::from("/repo")), &moved);
            assert!(format!("{:#}", git.served(&now).err().unwrap()).contains(STALE));
        }

        let read_first = super::super::queue_reads::QueueRead::parse(UseCase::Stats, &json!({}))
            .unwrap()
            .unwrap();
        let sends = std::cell::Cell::new(0);
        let call = |stale_until: usize| {
            let sends = &sends;
            let inputs = &inputs;
            move |asked: &super::super::queue_reads::QueueRead| {
                if matches!(asked, super::super::queue_reads::QueueRead::StatsInputs(_)) {
                    return Ok(serde_json::to_value(inputs).unwrap());
                }
                sends.set(sends.get() + 1);
                if sends.get() <= stale_until {
                    anyhow::bail!("failed: stats {STALE}")
                }
                Ok(json!({"answered": sends.get()}))
            }
        };
        assert_eq!(
            call_read(&read_first, &client, call(1)).unwrap(),
            json!({"answered": 2})
        );
        sends.set(0);
        let error = call_read(&read_first, &client, call(usize::MAX)).unwrap_err();
        assert!(format!("{error:#}").contains(STALE));
        assert_eq!(sends.get(), ATTEMPTS);
    }

    /// Reads whose plain form (every commit's ID and areas, the whole
    /// history) is far past what a request takes are sent within it, and
    /// read back to the same areas and the same hotspots; params past it
    /// even so are refused with their size, and nothing is sent.
    #[test]
    fn reads_past_a_request_in_their_plain_form_are_sent_within_it_and_read_the_same() {
        use crate::domain::queue_service::{MAX_REQUEST_BYTES, UseCase};
        use crate::domain::stats::conflicts::{MainChange, MainCommit};
        use crate::domain::stats::{ConflictConfigReport, conflicts::conflict_hotspots};
        const COMMITS: u32 = 60_000;
        let mut events: Vec<RunEvent> = (0..COMMITS)
            .map(|n| landed(i64::from(n) + 1, n, &format!("{n:040x}")))
            .collect();
        events.push(conflict(i64::from(COMMITS) + 1, "a"));
        let history = MainHistory {
            commits: (0..COMMITS)
                .map(|n| MainCommit {
                    sha: format!("{n:040x}"),
                    at: 100 + i64::from(n) * 7,
                    changes: vec![
                        MainChange {
                            path: if n % 9 == 0 { "a" } else { "src/other/file.rs" }.into(),
                            from: None,
                            deleted: false,
                        },
                        MainChange {
                            path: format!("docs/plans/{n}.md"),
                            from: None,
                            deleted: false,
                        },
                    ],
                })
                .collect(),
            paths: (0..COMMITS)
                .map(|n| format!("docs/plans/{n}.md"))
                .chain(["a".to_owned()])
                .collect(),
        };
        let plain: std::collections::BTreeMap<String, Vec<String>> = (0..COMMITS)
            .map(|n| (format!("{n:040x}"), vec!["docs".to_owned()]))
            .collect();
        let plain_history: Vec<serde_json::Value> = history
            .commits
            .iter()
            .map(|commit| {
                let changes: Vec<serde_json::Value> = commit
                    .changes
                    .iter()
                    .map(|change| json!({"path": change.path, "from": change.from, "deleted": change.deleted}))
                    .collect();
                json!({"sha": commit.sha, "at": commit.at, "changes": changes})
            })
            .collect();
        assert!(
            serde_json::to_vec(&(&plain, &plain_history, &history.paths))
                .unwrap()
                .len()
                > 4 * MAX_REQUEST_BYTES,
            "the plain form is past a request"
        );
        let all_docs = |_: &Path| {
            use crate::domain::areas::AreaMap;
            Ok(super::super::areas::AreaReader {
                map: Some(AreaMap::new(vec![("docs".into(), vec!["*".into()])]).unwrap()),
                changed: std::sync::Arc::new(|commits: &[String]| {
                    Ok(commits
                        .iter()
                        .map(|commit| (commit.clone(), vec!["x".to_owned()]))
                        .collect())
                }),
                judged: None,
            })
        };
        let whole = history.clone();
        let read = move |_: &Path, _: i64| Ok(whole.clone());
        let client = client_of(&all_docs, &read);
        let (sent_read, git) = sent(UseCase::Stats, &events, &client);
        assert!(serde_json::to_vec(&sent_read.request().1).unwrap().len() < MAX_REQUEST_BYTES / 2);
        let inputs = stats_inputs(Some(PathBuf::from("/repo")), &events);
        let served = git.unwrap().served(&inputs).unwrap();
        assert_eq!(
            served.areas.run_areas(&events),
            all_docs(Path::new("/repo")).unwrap().run_areas(&events)
        );
        let hotspots = |history: History| {
            serde_json::to_value(conflict_hotspots(
                &events,
                crate::domain::EventId::new(0),
                crate::domain::EventId::new(i64::from(COMMITS) + 1),
                |_| true,
                &history,
                ConflictConfigReport::default(),
            ))
            .unwrap()
        };
        assert_eq!(
            hotspots(conflict_history(&events, &|since| served.history(since))),
            hotspots(History::Read(history))
        );
        let error = fit_request(&sent_read, 100).unwrap_err();
        assert!(format!("{error:#}").contains("stats"), "{error:#}");
    }

    /// Git reads that name a long name over and over, so that the service
    /// would make far more than the request holds, are refused, as are
    /// seconds past what a unix millisecond holds; the service makes none
    /// of them.
    #[test]
    fn reads_naming_more_than_the_service_makes_are_refused() {
        let events = [landed(1, 1, "c1"), conflict(2, "a")];
        let inputs = stats_inputs(Some(PathBuf::from("/repo")), &events);
        let long = "x".repeat(DECODED_BYTES / 4);
        let refused = |git: StatsGitReads| format!("{:#}", git.served(&inputs).err().unwrap());
        let history = |history: CompactHistory| StatsGitReads {
            inputs: inputs.version.clone(),
            areas: None,
            history: Some(SentHistory::Read(history)),
        };
        let named = refused(history(CompactHistory {
            names: vec![long.clone()],
            at: vec![0],
            changes: vec![(0, vec![(0, Some(0), false); 3])],
            paths: Vec::new(),
        }));
        assert!(named.contains("more than"), "{named}");
        let past = refused(history(CompactHistory {
            at: vec![i64::MAX / 1000 + 1],
            ..CompactHistory::default()
        }));
        assert!(past.contains("out of range"), "{past}");
        let overflow = refused(history(CompactHistory {
            at: vec![i64::MAX / 2000, i64::MAX],
            ..CompactHistory::default()
        }));
        assert!(overflow.contains("out of range"), "{overflow}");
        let areas = refused(StatsGitReads {
            inputs: inputs.version.clone(),
            areas: Some(SentAreas {
                sets: vec![vec![long.clone(); 5]],
                of: vec![Some(0)],
            }),
            history: None,
        });
        assert!(areas.contains("more than"), "{areas}");
        let within = StatsGitReads {
            inputs: inputs.version.clone(),
            areas: Some(SentAreas {
                sets: vec![vec![long]],
                of: vec![Some(0)],
            }),
            history: None,
        };
        assert!(within.served(&inputs).is_ok());
    }
}
