//! The one read of the ledger by a window (ADR-t1662-1 decisions 12 and
//! 18, ADR-t1662-2 D1, docs/design/measurement.md "共通の読み取り関数"):
//! [`select`] takes the rows of every kind and `stats`' window (`--since`
//! / `--until`, an event id or a time, counted to the window's end) and
//! `--goal`, `--change`, `--area`, and returns the [`LedgerWindow`] the
//! statistics are made of. A row is matched by its range of input ids
//! (`inputs_from` to `inputs_through`) and its times (`started_at`,
//! `ended_at`; one not ended reaches the window's end). The filters
//! choose among the rows of a task, a goal or a run; the folds that carry
//! state into a window (the attributes, the findings, main's history, the
//! judgments of now) read every row's records up to the window's end
//! whatever the filters. Each read says why it has no value instead of a
//! 0 or an empty list: [`FoldedJudgments`]' states, [`MainMissing`],
//! [`attributes::Folded::BeforeBaseline`],
//! [`findings::Folded::BeforeBaseline`], and an Execution's context of
//! none (not recorded) apart from one with `context_reason` (not
//! measured). A node row (the host's load over an hour) is taken by its
//! times alone, whatever the filters, as `stats` reads the host's load
//! over the window's times ([`LedgerWindow::host`]).
//!
//! # Where each value comes from
//!
//! The rows' kinds are `run` and `task` ([`super::fold`]), `session`
//! ([`super::session`]), `queue` ([`super::queue`]) and `node`
//! ([`super::node`], whose items its doc lists); "records" is the
//! events a row owns ([`super::owner`]), which [`LedgerWindow::events`]
//! gives back for today's functions. The time each is recorded, and what
//! is missing when, follow measurement.md "欠測と集約の契約": the judgments
//! of now are taken from the one stream with the latest successful
//! observation within the grace (streams never added up) and are missing
//! (`観測が無い`) past the grace after the last successful observation for
//! the supervisor's stop, the observation's failure, a gap in the record,
//! a stale reach or a window before the baseline; main's history is
//! missing (`記録が無い`) past the grace after its reach
//! (`recorded_through`, when the supervisor last found the history to the
//! head it read recorded, not a commit's time) for Git that could not be
//! read, the supervisor's stop, a stale reach or a window before the
//! record; the attributes and the findings are missing before their
//! baseline (`基準より前`).
//!
//! ## The inputs of today's use cases
//!
//! One line per input of `stats()`, `kpi()`, `report::make` and
//! `report::diagram`: its lasting source, when that is recorded, what a
//! window without it is (the rules above) and the row and item it is
//! folded into.
//!
//! | input | lasting source | recorded | missing | row and item |
//! | --- | --- | --- | --- | --- |
//! | `EventStore::all_events` (`stats`, `kpi`) | the events | as they happen | none: each event is some row's | every row's records ([`LedgerWindow::events`]) |
//! | `PlanningRecords::task_goals` (`stats`, `kpi`) | the attribute records ([`attributes`]) | in the transaction of the change; a baseline for what had none | `基準より前` before the baseline | task rows' records; queue event rows of a goal's ([`LedgerWindow::attributes`]) |
//! | `PlanningRecords::task_titles` (`stats`) | the attribute records ([`attributes`]) | in the transaction of the change; a baseline for what had none | `基準より前` before the baseline | task rows' records |
//! | `PlanningRecords::task_changes` (`stats`, `kpi`) | the attribute records ([`attributes`]) | in the transaction of the change; a baseline for what had none | `基準より前` before the baseline | task rows' records |
//! | `PlanningRecords::draft_origins` (`stats`, `kpi`) | the attribute records ([`attributes`]) | in the transaction of the change; a baseline for what had none | `基準より前` before the baseline | task rows' records |
//! | `SupervisorReads::supervisors` (the slots of the `SlotSnapshot`) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `ProcessControl::alive` (whether a supervisor lives, for the slots) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `CoordinationReads::run_leases` (the slots a lease holds) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `RunReads::active_runs` (the runs holding a slot) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `RunLog::run_events` (whether a run waits outside the slots) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `TaskReads::list` (the ready tasks) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `TaskReads::list_goals` (the ready tasks of draft goals) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `TaskReads::candidates` (the candidates) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `RunReads::all_runs` (the unfinished runs, their order, status and workspace, the `LiveSnapshot`) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `RunFiles` `idle.json` (when the session went idle, its background tasks) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `RunFiles` the idle log (when a background task was first listed) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `RunFiles` `prompt-submit.json` (the last input) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `RunFiles` the receipt (when it was written) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `AgentSignals::idle_hook` (the marker read) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `ProcessControl::start_identity` (`background_alive` of a wrapper) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | the session's workspace (`workspace_id`, `session_workspace`) | the judgments of now's inputs ([`crate::domain::live_alerts`]): the stream's baseline, the input changed and its version, the reach | each supervisor's pass (60 s), its reach every 300 s | `観測が無い` with its reason past the grace after the stream's last successful observation | queue `live_alert_stream` rows, per supervisor; judged at the window's end ([`LedgerWindow::judgments`]) |
//! | `StatsSources::history` (Git: `MainHistory`'s landings, renames, deletions and paths) | main's history ([`crate::domain::stats::main_log`]) | each supervisor's pass, its reach (`recorded_through`, when it found the history to the head it read recorded) every 600 s | `記録が無い` with its reason past the grace after the reach | queue event rows of `main_*` ([`LedgerWindow::main_history`], [`LedgerWindow::main_covers`]) |
//! | `AreaReader::changed` (Git: the landed commits' files) | main's history ([`crate::domain::stats::main_log`]) | each supervisor's pass, its reach (`recorded_through`, when it found the history to the head it read recorded) every 600 s | `記録が無い` with its reason past the grace after the reach | queue event rows of `main_*` ([`LedgerWindow::main_history`], [`LedgerWindow::main_covers`]) (`MainLog::changed`) |
//! | `QueueRecords::findings` (`report`'s `findings_open` and lines) | the finding records ([`findings`]) | in the transaction of the change; a baseline for what had none | `基準より前` before the baseline | queue `finding` rows ([`LedgerWindow::findings`]) |
//! | `TaskReads::graph_input` (`report::diagram`: priorities and who set them, goals' priorities, goal dependencies and verdicts, candidates) | the attribute records ([`attributes`]) | in the transaction of the change; a baseline for what had none | `基準より前` before the baseline | task rows' records; queue event rows of a goal's ([`LedgerWindow::attributes`]) |
//! | `TaskReads::list_goals` (`report::diagram`'s goal titles) | the attribute records ([`attributes`]) | in the transaction of the change; a baseline for what had none | `基準より前` before the baseline | queue event rows of a goal's |
//! | `SupervisorReads::supervisors` (`kpi`'s heartbeats) | `supervisor_alive`, `supervisor_started`, `supervisor_stopped` | every 300 s, at a start and a stop | a supervisor past its evidence of life is ended ([`crate::domain::supervisor_life`]) | queue rows of `supervisor_*` (an interval's end or an event) |
//! | `StatsSources::host_metrics` (`infrastructure::host_metrics::summary` of the host's CSV files) | the host's samples (NodeSampleStore) | the host's sampling, each `--host-metrics-interval`; a CSV row the store lacks at the next import | no sample in the window's seconds: a summary of none; past `--host-metrics-retention-days` the store and the node rows keep none | node rows of `host`: `detail.samples` ([`super::node`]), the window's seconds ([`LedgerWindow::host`]) |
//! | `kpi`'s `HostReader`, `ReportSetup::host_metrics` (`infrastructure::host_metrics::summary` of the host's CSV files) | the host's samples (NodeSampleStore) | the host's sampling, each `--host-metrics-interval`; a CSV row the store lacks at the next import | no sample in the period's seconds: a summary of none; past `--host-metrics-retention-days` the store and the node rows keep none | node rows of `host`: `detail.samples` ([`super::node`]), each period's seconds after its start to its end ([`LedgerWindow::host_between`]) |
//! | `StatsSources::config_file` (`[stall]` of `dagq.toml`) | a setting, not an observation | read with each call | — | none; the `[stall]` a supervisor used is `stall_config_loaded`, a queue event row |
//! | `StatsSources::conflicts_file` (`[conflicts]`) | a setting, not an observation | read with each call | — | none: the reader takes it beside the window |
//! | `AreaReader::map` (`[areas]`) | a setting, not an observation | read with each call | — | [`WindowQuery::areas`] |
//! | `StatsSources::utc_offset_secs`, `kpi::Host::utc_offset_secs` | a setting, not an observation | read with each call | — | none: the reader takes it beside the window |
//! | `StatsSources::counts_checks`, `kpi::Host::rust_toolchain` | a setting, not an observation | read with each call | — | none: the reader takes it beside the window |
//! | `kpi::Host::cores` | a setting, not an observation | read with each call | — | none: the reader takes it beside the window |
//! | `KpiConfig` (`[kpi]`) | a setting, not an observation | read with each call | — | none: the reader takes it beside the window |
//! | `ReportSetup` (root, retention, build, the diagram's renderer) | a setting, not an observation | read with each call | — | none: the reader takes it beside the window |
//! | `now` | the call | — | — | [`WindowQuery::now_ms`] |
//! | `--since`, `--until`, `--goal`, `--change`, `--area` | the call | — | — | [`WindowQuery`] |
//!
//! ## `stats --full`
//!
//! | field | row and item | window |
//! | --- | --- | --- |
//! | `runs`, `goals`, `changes`, `areas`, `e2e`, `overall`, `versions`, `load_bands`, `trial_groups`, `escalations` | run rows: intervals and totals, `legacy`, attributes and `after_claim` ([`super::read::read`]), its sessions, jobs and Executions, and records; task rows' attributes; areas by main's history | the run's finish in the cursors' window |
//! | `alerts` (slots), `running_alerts`, `workspace_check` | queue `live_alert_stream` rows ([`LedgerWindow::judgments`]) and the run rows' records (nudges, asks) | the window's end |
//! | `alerts` (others), `backend_failures`, `reason_codes`, `duplicate_cancels`, `cancel_reasons`, `review_reasons`, `stall_thresholds`, `landing_rechecks`, `landing_waits`, `claim_waits`, `verification_commands`, `verification_failures`, `failed_tests`, `waiting`, `worker_routes`, `asks`, `provider_switches`, `follow_up_categories`, `worker_question_topics`, `recommendations` | run rows' and task rows' records; queue event rows (an ask or a repair of no run, a backend's failure) | the cursors' window |
//! | `draft_flow`, `auto_repairs` | task rows' records, a task's not ended among them (a draft's `task_created` and its leaving `draft`, an ask of no run); run rows' records (`follow_up_registered`, the landings, an ask or a repair of a run); queue event rows (a repair of no task) | the cursors' window |
//! | `stall_config` | queue event rows of `stall_config_loaded`, else the settings | the window's end |
//! | `conflict_hotspots` | run rows' records (the conflicts) and queue rows of `main_*` ([`LedgerWindow::main_history`]) | the cursors' window; `記録が無い` with its reason |
//! | `sessions` (`by_kind`, `by_route`, per run and goal) | session rows (the spans of no run) and run rows' `sessions`; their tokens and `context` from the rows' `executions` ([`LedgerWindow::executions`]) | the spans overlapping the window, the Executions that ended in it |
//! | `execution_tokens` | every run and session row's Executions, whatever rows the window chose, so that `recorded_from` and `coverage` are the queue's ([`LedgerWindow::execution_tokens`]; each one's end, a cut's time; actor × provider × model as `stats` sums them) | ended in the window's times and of its filters |
//! | `planner_routes` | session rows of `runtime_planner` (and their records), with their `executions`; the `planner_question` asks of a task from its task row's records, a task not ended too | the cursors' window |
//! | `jobs` | session rows of the jobs of no run and run rows' `jobs` (kind, provider, model, mode, verdict, time) | ended in the window |
//! | `claim_holds`, `landing_holds`, `claim_deferrals` | queue `claim_hold`, `landing_hold`, `claim_deferral` rows | started in the window; the one open at its end |
//! | `updates` | queue event rows of `update_*` | the cursors' window |
//! | `landing_utilization` | queue `landing_slot` rows and run rows' records (the waits) | overlapping the window |
//! | `host` (`samples`, `first`, `last`, `metrics`: each column's `min`, `mean`, `median`, `max`, `p90`; `cpu_secs`: `covered_secs`, `max_gap_secs`, `total`, `by_kind`) | node rows of `host`: their `detail.samples` summarized whole by [`crate::domain::host_metrics::summarize`] ([`super::node::host_summary`]); today `infrastructure::host_metrics::summary` of the CSV files | the window's seconds, from `--since`'s time (else its first event's) to its end, both inclusive ([`LedgerWindow::host`]) |
//! | `next_cursor`, `window_ms` | the window ([`LedgerWindow`]) | — |
//!
//! ## `kpi`
//!
//! Each period's values are `stats` of its window (above) and these:
//!
//! | metric | row and item |
//! | --- | --- |
//! | per run (`landings`, `lead_time`, `phase.*`, `land_phase.*`, by axis and `--by` key) | run rows (intervals, attributes, `legacy`), task rows' attributes, areas by main's history |
//! | `asks_per_landing`, `attentions_per_landing`, `ask_wait`, `ask_apply_wait`, `ask_seen_wait`, `ask.worker_question_wait`, `backend_failures_per_run`, `auto_repairs`, `max_load_avg`, `verify_command.*`, `review.sendback_rate`, `plan.*`, `forecast.*` | run, task and queue event rows' records |
//! | `slot_usage`, `landing_utilization`, `landing_utilization.peak`, `landing_attempt`, `landing_queue_depth` | run rows' intervals and holds, queue `landing_slot` rows |
//! | `candidates` | queue `live_alert_stream` rows (the slots' inputs) |
//! | `findings_open`, `finding_resolve_time` | queue `finding` rows |
//! | `improvement_proposals` | queue `finding` rows' records (the links to a proposal and their `by`) and task rows' records (`task_submitted` with its `owner_origin`, `task_reopened`, `task_status_changed`, `proposal_withdrawn`) up to the period's end |
//! | `drafts_per_landing`, `draft_backlog` | task rows' records (a task not ended too), task attributes |
//! | `session_open.*`, `session_active.*`, `session_active_ratio.*` | session rows and run rows' `sessions` |
//! | `job.count.*`, `job.failed_rate.*`, `job.secs.*`, `job.verdict.*` | session rows of jobs and run rows' `jobs` |
//! | `tokens`, `tokens_per_landing` (their `details.tokens`, `token_coverage`) | each period's [`LedgerWindow::execution_tokens`]: every run and session row's Executions, summed by the rule of the tokens' aggregation (each one's end, actor × provider × model), the records' start the queue's |
//! | `supervisors` (their lives and heartbeats) | queue rows of `supervisor_*` |
//! | `host` (as `stats`' `host`) | node rows of `host`: `detail.samples` summarized whole ([`super::node::host_summary`]) over each period's seconds after its start to its end, so two periods never share a sample, and over each `--compare` window's the same way ([`LedgerWindow::host_between`]); today `kpi`'s `HostReader` over `infrastructure::host_metrics::summary` |
//! | `cpu_per_landing`, `cpu_per_landing.<kind>` | the period's `host`: `cpu_secs.total` and `cpu_secs.by_kind` (each sample's `cpu_total` and `cpu_<kind>` for the seconds since the one before, the first for the shortest gap but never before the period, a gap up to `max_gap_secs`) over the run rows' landings |
//! | `load_per_core` | the period's `host`: `metrics.load1` (`median`, `p90`, `max` of the samples' `load1`) over `kpi::Host::cores` |
//! | `health.disk` | the period's `host`: `metrics.disk_free_bytes` and `metrics.disk_free_pct` (`min`, `median` of the samples') ([`crate::domain::host_metrics::DiskFree::of`]) |
//! | `health.routes`, `health.planner_routes` | as `worker_routes` and `planner_routes` |
//!
//! ## `report`
//!
//! | section | row and item |
//! | --- | --- |
//! | `report` (period, build, time) | the window and the settings |
//! | `kpi` | as `kpi` above |
//! | `findings_open`, `findings` (every field of a line, larger impact first) | queue `finding` rows' values ([`LedgerWindow::findings`], [`findings::Findings::unsettled`]) |
//! | `diagram` | task rows' attributes and queue event rows of the goals' (titles, priorities, dependencies, verdicts, candidates) |
//! | `host` (each period's `host`, `cpu_per_landing`, `load_per_core` and `health.disk` in the JSON; the HTML's section of the host's CPU per landing and load over the cores) | as `kpi` above: node rows of `host`, each period's seconds ([`LedgerWindow::host_between`]); today `ReportSetup::host_metrics` over `infrastructure::host_metrics::summary` |

use std::collections::{BTreeSet, HashMap};

use super::{
    LedgerKind, LedgerRow, WindowEnd, attributes, findings, node,
    read::{LedgerValues, read},
    session::row_executions,
};
use crate::domain::{
    EventId, GoalId, RunEvent, TaskChange, TaskId,
    areas::{AreaMap, run_areas},
    host_metrics::HostSummary,
    live_alerts::{self, FoldedJudgments},
    stats::{
        Cursor, History,
        context::{self, ContextRecord},
        executions::{self, Execution, ExecutionStats},
        main_log::{MainLog, MainMissing, fold_main_log},
        timestamp_millis,
    },
};

/// What a window of the ledger is asked for: `stats`' cursors and filters,
/// the time without `--until` (`now_ms`), and the `[areas]` map an
/// `--area` is matched by (a setting the reader takes beside the rows).
#[derive(Debug, Clone, Default)]
pub struct WindowQuery {
    pub since: Option<Cursor>,
    pub until: Option<Cursor>,
    pub goal_id: Option<GoalId>,
    pub change: Option<TaskChange>,
    pub area: Option<String>,
    pub areas: Option<AreaMap>,
    pub now_ms: i64,
}

/// The rows of a window and what is folded of them.
#[derive(Debug, Clone)]
pub struct LedgerWindow {
    /// The window by event id: `after < id <= upto`.
    pub after: EventId,
    pub upto: EventId,
    /// The window by time (unix milliseconds): from `--since`'s (none
    /// without it) to `--until`'s, else `now_ms`.
    pub start_ms: Option<i64>,
    pub end_ms: i64,
    /// Where the folds stop: at `upto` always, and at `end_ms` too unless
    /// `--until` is an event (an event of its time with a later id is past
    /// it; the event itself is in it, whatever its milliseconds).
    pub end: WindowEnd,
    /// Whether `--since` is an event: the window starts after it by id.
    since_event: bool,
    /// Where the host's load is read from (unix milliseconds), as `stats`
    /// reads it: `--since`'s time, else the first event of the window,
    /// else its end.
    host_from_ms: i64,
    /// The rows in the window the filters chose, by their first input.
    pub rows: Vec<LedgerRow>,
    /// Every row's records, the window and the filters aside: what the
    /// folds that carry state read, each to [`Self::end`] (a baseline's
    /// group is taken only with its mark, which may come after the
    /// window).
    records: Vec<RunEvent>,
    /// The filters, as the counts of the tokens read them.
    filters: Filters,
}

/// The events of `rows`' records, ascending id, each once.
fn records(rows: &[LedgerRow]) -> Vec<RunEvent> {
    let mut events: Vec<RunEvent> = rows
        .iter()
        .flat_map(|row| row.records.iter().cloned())
        .collect();
    events.sort_by_key(|event| event.id);
    events.dedup_by_key(|event| event.id);
    events
}

/// The time a cursor stands for among `events`: its own, or its event's.
fn cursor_ms(cursor: Cursor, events: &[RunEvent]) -> Option<i64> {
    match cursor {
        Cursor::Time(millis) => Some(millis),
        Cursor::Event(id) => events
            .iter()
            .find(|event| event.id == id)
            .and_then(|event| timestamp_millis(&event.created_at)),
    }
}

/// The window `query` asks of `rows` (every kind's): the cursors and the
/// times resolved as `stats` resolves them (a `--since` past `--until`
/// leaves nothing),
/// the rows with an input by the window's end that have not ended before
/// its start, then those of the filters.
pub fn select(rows: Vec<LedgerRow>, query: &WindowQuery) -> LedgerWindow {
    let all = records(&rows);
    let since = query.since.map(|cursor| cursor.event_id(&all));
    let until = query.until.map(|cursor| cursor.event_id(&all));
    let latest = all.last().map_or(EventId::new(0), |event| event.id);
    let upto = until.map_or(latest, |until| {
        let capped = until.min(latest);
        since.map_or(capped, |since| capped.max(since.min(latest)))
    });
    let after = since.unwrap_or(EventId::new(0));
    // As `stats` times its window: to `--until`'s time (not past now), else
    // the time of the event it stops at, else now; from `--since`'s, not
    // past the end.
    let stops_at = all
        .iter()
        .find(|event| until.is_some() && event.id == upto)
        .and_then(|event| timestamp_millis(&event.created_at));
    let end_ms = match query.until {
        Some(Cursor::Time(millis)) => millis.min(query.now_ms),
        _ => stops_at.unwrap_or(query.now_ms),
    };
    let start_ms = query
        .since
        .and_then(|cursor| cursor_ms(cursor, &all))
        .map(|start| start.min(end_ms));
    let end = WindowEnd {
        upto: Some(upto),
        ms: (!matches!(query.until, Some(Cursor::Event(_)))).then_some(end_ms),
    };
    let host_from_ms = start_ms
        .or_else(|| {
            all.iter()
                .filter(|event| event.id > after && event.id <= upto)
                .filter_map(|event| timestamp_millis(&event.created_at))
                .min()
        })
        .unwrap_or(end_ms)
        .min(end_ms);
    let mut window = LedgerWindow {
        after,
        upto,
        start_ms,
        end_ms,
        end,
        since_event: matches!(query.since, Some(Cursor::Event(_))),
        host_from_ms,
        rows: Vec::new(),
        records: all,
        filters: Filters::default(),
    };
    let filters = Filters::of(&window, query);
    let mut chosen: Vec<LedgerRow> = rows
        .into_iter()
        .filter(|row| window.overlaps(row) && filters.keeps(row))
        .collect();
    chosen.sort_by_key(|row| (row.inputs_from, row.kind));
    window.rows = chosen;
    window.filters = filters;
    window
}

/// The filters of a window, resolved on its attributes and areas.
#[derive(Debug, Clone, Default)]
struct Filters {
    goal_id: Option<GoalId>,
    change: Option<TaskChange>,
    area: Option<String>,
    goals: HashMap<TaskId, Option<GoalId>>,
    changes: HashMap<TaskId, Option<TaskChange>>,
    /// The areas of each landed run and, through its runs, each task.
    run_areas: HashMap<String, BTreeSet<String>>,
    task_areas: HashMap<TaskId, BTreeSet<String>>,
}

impl Filters {
    fn of(window: &LedgerWindow, query: &WindowQuery) -> Self {
        let mut filters = Self {
            goal_id: query.goal_id,
            change: query.change.clone(),
            area: query.area.clone(),
            ..Self::default()
        };
        if query.goal_id.is_none() && query.change.is_none() && query.area.is_none() {
            return filters;
        }
        if let attributes::Folded::Attributes(known) = window.attributes() {
            filters.goals = known.task_goals();
            filters.changes = known.task_changes();
        }
        if let Some(map) = &query.areas {
            let events = window.events();
            let changed = window.main_log().changed;
            for (run, areas) in run_areas(map, &events, &changed) {
                let task = events
                    .iter()
                    .find(|event| event.run_id.as_ref() == Some(&run))
                    .and_then(|event| event.task_id);
                if let Some(task) = task {
                    filters
                        .task_areas
                        .entry(task)
                        .or_default()
                        .extend(areas.iter().cloned());
                }
                filters
                    .run_areas
                    .insert(run.as_str().to_owned(), areas.into_iter().collect());
            }
        }
        filters
    }

    /// The goals a row is of: its task's, else those its records name (a
    /// goal review's, a plan review's goals).
    fn goals_of(&self, row: &LedgerRow) -> BTreeSet<GoalId> {
        let mut goals: BTreeSet<GoalId> = BTreeSet::new();
        if let Some(goal) = row
            .task_id
            .and_then(|task| self.goals.get(&task).copied().flatten())
        {
            goals.insert(goal);
        }
        if row.kind != LedgerKind::Run && row.kind != LedgerKind::Task {
            goals.extend(row.records.iter().filter_map(|event| event.goal_id));
            goals.extend(
                row.detail["items"]["goal_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_i64)
                    .map(GoalId::new),
            );
        }
        goals
    }

    /// Whether an Execution counts under the filters, as `stats` counts
    /// its `execution_tokens` under `--goal`: by its task's goal, else (a
    /// goal review's) its own goal; by its task's change, by its run's
    /// areas (else its task's).
    fn counts(&self, execution: &Execution) -> bool {
        if let Some(goal) = self.goal_id {
            let of = match (execution.task_id, execution.goal_id) {
                (None, of) => of,
                (Some(task), _) => self.goals.get(&task).copied().flatten(),
            };
            if of != Some(goal) {
                return false;
            }
        }
        if let Some(change) = &self.change
            && execution
                .task_id
                .and_then(|task| self.changes.get(&task).cloned().flatten())
                .as_ref()
                != Some(change)
        {
            return false;
        }
        if let Some(area) = &self.area {
            let areas = execution
                .run_id
                .as_ref()
                .and_then(|run| self.run_areas.get(run.as_str()))
                .or_else(|| {
                    execution
                        .task_id
                        .and_then(|task| self.task_areas.get(&task))
                });
            if !areas.is_some_and(|areas| areas.contains(area)) {
                return false;
            }
        }
        true
    }

    fn keeps(&self, row: &LedgerRow) -> bool {
        // The host's load is the queue's, whatever is filtered.
        if row.kind == LedgerKind::Node {
            return true;
        }
        if let Some(goal) = self.goal_id
            && !self.goals_of(row).contains(&goal)
        {
            return false;
        }
        if let Some(change) = &self.change {
            let of_task = row
                .task_id
                .and_then(|task| self.changes.get(&task).cloned().flatten());
            if of_task.as_ref() != Some(change) {
                return false;
            }
        }
        if let Some(area) = &self.area {
            let areas = match row.kind {
                LedgerKind::Run => self.run_areas.get(&row.key),
                _ => row.task_id.and_then(|task| self.task_areas.get(&task)),
            };
            if !areas.is_some_and(|areas| areas.contains(area)) {
                return false;
            }
        }
        true
    }
}

impl LedgerWindow {
    /// Whether `row` falls in the window: it has begun by the window's end
    /// (its first input at or before `upto`, never by its time alone, so a
    /// row of the time of an event `--until` with a later id is past it)
    /// and has not ended before the window's start: an input after
    /// `after`, still open (a legacy row keeps no end and falls in by its
    /// inputs only), or ended after the start's time (at it too for a time
    /// `--since`; an event `--since` leaves out what ended at its time,
    /// which is that event's or before it).
    /// A node row falls in by its hour alone: it may hold a sample of the
    /// seconds the host's load is read over ([`Self::host`]).
    fn overlaps(&self, row: &LedgerRow) -> bool {
        if row.kind == LedgerKind::Node {
            let (from, until) = self.host_secs();
            return row
                .started_at
                .zip(row.ended_at)
                .is_some_and(|(start, end)| node::covers(start, end, from, until));
        }
        let begun = row.inputs_from <= self.upto;
        let input_after = row.inputs_through.events > self.after;
        let open = row.ended_at.is_none() && !row.is_legacy();
        let ended_after = match (row.ended_at, self.start_ms) {
            (Some(end), Some(from)) if self.since_event => end > from,
            (Some(end), Some(from)) => end >= from,
            (Some(_), None) => true,
            (None, _) => false,
        };
        begun && (input_after || open || ended_after)
    }

    /// The window's rows of `kind`.
    pub fn of(&self, kind: LedgerKind) -> impl Iterator<Item = &LedgerRow> {
        self.rows.iter().filter(move |row| row.kind == kind)
    }

    /// The window's run and task rows as [`read`] reads each.
    pub fn values(&self) -> Vec<LedgerValues> {
        self.rows
            .iter()
            .filter(|row| matches!(row.kind, LedgerKind::Run | LedgerKind::Task))
            .map(read)
            .collect()
    }

    /// The events up to the window's end ([`Self::end`]), ascending id:
    /// every row's records, whatever the filters, so that today's functions
    /// of `stats`, `kpi` and `report` read from the rows what they read of
    /// the EventStore.
    pub fn events(&self) -> Vec<RunEvent> {
        self.records
            .iter()
            .filter(|event| self.end.holds(event))
            .cloned()
            .collect()
    }

    /// The window's end in unix seconds.
    pub const fn end_secs(&self) -> i64 {
        self.end_ms.div_euclid(1000)
    }

    /// The unix seconds the host's load is read over, both inclusive.
    const fn host_secs(&self) -> (i64, i64) {
        (self.host_from_ms.div_euclid(1000), self.end_secs())
    }

    /// The host's load over the window as `stats`' `host`: the samples of
    /// its node rows from `--since`'s time (else its first event's) to its
    /// end, summarized as `infrastructure::host_metrics::summary`
    /// summarizes the same samples.
    pub fn host(&self) -> HostSummary {
        let (from, until) = self.host_secs();
        node::host_summary(self.of(LedgerKind::Node), from, until)
    }

    /// The host's load over a span of the window as `kpi`'s periods (and
    /// their `--compare` windows) read it: the seconds after `start_ms` up
    /// to `end_ms` (`kpi::HostReader::between`), so that two adjacent
    /// spans never share a sample. Only the window's node rows are read: a
    /// span outside it reads none.
    pub fn host_between(&self, start_ms: i64, end_ms: i64) -> HostSummary {
        node::host_summary(
            self.of(LedgerKind::Node),
            start_ms.div_euclid(1000) + 1,
            end_ms.div_euclid(1000),
        )
    }

    /// The judgments of now at the window's end (`alerts`' slots,
    /// `running_alerts`, `workspace_check`) and how they were observed:
    /// the inputs of the stream chosen by its latest successful
    /// observation judged at the window's end, or why none was.
    pub fn judgments(&self) -> FoldedJudgments {
        live_alerts::fold_within(
            &self.records,
            |event| self.end.holds(event),
            self.end_secs(),
        )
    }

    /// Main's history as its record gives it at the window's end: the
    /// commits, the paths and the reach recorded by then, a later
    /// recovery or reach not among them.
    pub fn main_log(&self) -> MainLog {
        fold_main_log(&self.events())
    }

    /// Main's history for the window, as `conflict_hotspots` spans it, or
    /// why it has none (`記録が無い`).
    pub fn main_history(&self) -> History {
        let events = self.events();
        fold_main_log(&events).history_for(&events, self.after, self.upto)
    }

    /// Whether main's history covers the window's times.
    pub fn main_covers(&self) -> Result<(), MainMissing> {
        let events = self.events();
        fold_main_log(&events).covers(&events, self.start_ms, Some(self.end_ms))
    }

    /// The tasks' and goals' attributes at the window's end.
    pub fn attributes(&self) -> attributes::Folded {
        attributes::fold_within(&self.records, self.end)
    }

    /// The findings at the window's end.
    pub fn findings(&self) -> findings::Folded {
        findings::fold_within(&self.records, self.end)
    }

    /// The tokens of the window as `stats`' `execution_tokens` counts
    /// them: every row's Executions, whatever the rows chosen, so that
    /// when the records start (`recorded_from`, `coverage`, per actor and
    /// provider) is the queue's; counted when they ended in the window's
    /// times and pass its filters ([`executions::window`]).
    pub fn execution_tokens(&self) -> ExecutionStats {
        let all = executions::executions(&self.records);
        executions::window(&all, self.start_ms, self.end_ms, |execution| {
            self.filters.counts(execution)
        })
    }

    /// The Executions of the window's chosen run and session rows (their
    /// `executions`), by when each ended: what each row holds, not what
    /// the window counts ([`Self::execution_tokens`]).
    pub fn executions(&self) -> Vec<Execution> {
        let mut executions: Vec<Execution> = self
            .rows
            .iter()
            .filter(|row| matches!(row.kind, LedgerKind::Run | LedgerKind::Session))
            .flat_map(row_executions)
            .collect();
        executions.sort_by_key(|execution| execution.at);
        executions
    }

    /// The Executions' context as `stats`' `runs[].context` and
    /// `sessions.by_kind.<kind>.<provider>.context` count it.
    pub fn context_records(&self) -> Vec<ContextRecord> {
        context::records(&self.executions())
    }
}

/// The window of `events` (ascending id) through the ledger: every row
/// [`super::build`] makes of them, selected up to `until_secs` (a time
/// cursor), or past every event without one. What the tests of the folds
/// read the folds through, so that each of them holds of the rows too.
#[cfg(test)]
pub(crate) fn through_ledger(events: &[RunEvent], until_secs: Option<i64>) -> LedgerWindow {
    let last_ms = events
        .iter()
        .filter_map(|event| timestamp_millis(&event.created_at))
        .max()
        .unwrap_or(0);
    let now_ms = until_secs.map_or((last_ms.div_euclid(1000) + 1) * 1000, |secs| secs * 1000);
    let rows = super::build(events, now_ms, "2030-01-01T00:00:00.000Z");
    let query = WindowQuery {
        until: until_secs.map(|secs| Cursor::Time(secs * 1000)),
        now_ms,
        ..WindowQuery::default()
    };
    select(rows, &query)
}

#[cfg(test)]
mod tests;
