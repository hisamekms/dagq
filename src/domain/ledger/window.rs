//! The one read of the ledger by a window (ADR-t1662-1 decisions 12 and
//! 18, ADR-t1662-2 D1, docs/design/measurement.md "共通の読み取り関数"):
//! [`WindowRows`] takes the rows of every kind as they are read and `stats`'
//! window (`--since` / `--until`, an event id or a time, counted to the
//! window's end) and `--goal`, `--change`, `--area`, and returns the
//! [`LedgerWindow`] the statistics are made of. A row is matched by its range of input ids
//! (`inputs_from` to `inputs_through`) and its times (`started_at`,
//! `ended_at`; one not ended reaches the window's end); a bound that is an
//! event id is matched by the ids, never crossed by a time. The filters
//! choose among the rows of a task, a goal or a run; the folds that carry
//! state into a window (the attributes, the findings, main's history, the
//! judgments of now, the reverts and CI's failures after the landings)
//! read the records of every row read up to the window's end whatever the
//! filters ("A read from a time" below says which a read may leave out);
//! the reverts and failures are given for the runs the filters chose
//! ([`LedgerWindow::post_landing`]). Each read says why it has no value instead of a
//! 0 or an empty list: [`FoldedJudgments`]' states, [`MainMissing`],
//! [`attributes::Folded::BeforeBaseline`],
//! [`findings::Folded::BeforeBaseline`], and an Execution's context of
//! none (not recorded) apart from one with `context_reason` (not
//! measured). A node row (the host's load over an hour) is taken by its
//! times alone, whatever the filters: from `--since`'s time (every one
//! before the end without it, so that `kpi`'s periods and `report` read
//! the host's load of any span, [`LedgerWindow::host_between`]), while
//! `stats`' `host` sums the window's own seconds ([`LedgerWindow::host`]).
//! The store holds no row of a run on its way (its row is written once
//! its life ended); [`runs_on_their_way`] names those its task rows not
//! ended give, and their rows are folded at the read by the same fold
//! ([`run_on_its_way`]) before [`WindowRows`] takes them.
//!
//! # A read from a time
//!
//! [`WindowQuery::read_from_ms`] leaves out of the store's read the rows the
//! periods of `kpi` from that time need nothing of ([`RowsFrom`]); without it
//! every row is read. The queue rows are always read, whole: most of what
//! the folds that carry state read is theirs. A run, session or task row is
//! left out when it ended before the time and each of its records stands
//! before it, in time and in id ([`RowsFrom::leaves_out`]); a row with no
//! time, a legacy one and a node row that reaches the time are read. A time
//! does not follow an id (a clock turned back has no limit), so neither is
//! read from the other. By time: what counts by when it happened, as an
//! Execution does, finds nothing in the periods among the rows left out. By
//! id: every record left out stands at or before a queue record of the time
//! or before it, so a period's first event id is the same with them or without.
//! What looks back past the time still comes, so that the answer is that of
//! every row:
//!
//! - A task row and every run it names come with a row of that task or a
//!   record of it from the time on ([`WindowRows::rows_wanted`]): a run's
//!   retries, a task's first `ready`.
//! - A run row comes when it holds a slot, an ask or a session to this day
//!   ([`WindowRows::take_earlier`]).
//! - The records of the task and session rows left out are among
//!   [`LedgerWindow::events`] still (a store gives them without their rows,
//!   and they are few beside the run rows' records): what a plan review
//!   predicted, a proposal's planner and judge, the routes of a planner.
//! - Of the run rows left out, what is read past their end
//!   ([`EARLIER_RUN_KINDS`]) comes the same way, and their claims
//!   ([`Earlier::claims`]), first Executions ([`Earlier::executions`]) and
//!   what the landings' attempts read ([`Earlier::landings`]) apart.
//!
//! All of this is read of one state of the rows
//! ([`crate::application::LedgerStore::ledger_read_snapshot`]): a row that
//! the builder moves between two of its reads would be in neither.
//!
//! # The edges of a window
//!
//! A window's edges are taken of the records of the rows read: the latest
//! event (`upto`), each cursor's id and time, the last event an event id
//! `--until` holds and its time, the first time of a window without
//! `--since` (`host_from_ms`) and, for a read from a time, the queue's last
//! record of the time or before it (`queue_before`), whatever the records
//! are (those past R of the rows held, a run's on its way, the omitted
//! kinds' up to C), and `stats` takes its own of the same records. The
//! queue's events of the kinds a coverage holds in its (C, S] have no row
//! ([`omitted`]); of the same snapshot as the rows (its coverage read after
//! the rows each edge is taken with, the newest of the pages of a read
//! whose pages are of more than one state:
//! [`crate::application::LedgerWindowRead::omitted_coverage`]), the read
//! takes the few of them each edge stands on among the records
//! ([`WindowRows::take_omitted_edges`]), and those before the time among the
//! queue's records ([`WindowRows::take_omitted_before`]), so that every
//! edge is the one of the rows that held them: the larger latest, the
//! smaller first time, a cursor's event itself. No other kind's edge is
//! read, and no run's, task's or session's event of those kinds (it comes
//! with its row's records alone). Of each kind: the latest is its L; a
//! cursor's event is read by its key, and the last at or before an event
//! id `--until` from the last point at or before it (K at most); the last
//! of a time at or before t
//! (a time cursor, the window's end, `queue_before`) is read from the last
//! point of an M at or before t, its T the answer so far, through the
//! events between the points whose T rises (K each), up to a timed one past
//! t + D or S, as M never falls and no timed one after another falls behind
//! M by more than D; the first time of ids (a, b] alike from the point
//! before the first whose T is past a, up to a timed one past the least
//! time read and D, or b, or S. An event of no time is the latest and no
//! time's answer. So the edges read the spans of K events that hold the
//! timed omitted events within D of their times, and a few seeks of the
//! points, however long the history around the window.
//!
//! The bounds a reader takes inside the window past these edges (`kpi`'s
//! periods and comparisons, the times of its event id cursors, the first
//! time of `stats`' page) are declared with the query
//! ([`WindowQuery::bounds`]) and answered by the same reads, a span of K
//! or of D each ([`bounds`]).
//!
//! # Where each value comes from
//!
//! The rows' kinds are `run` and `task` ([`super::fold`]), `session`
//! ([`super::session`]), `queue` ([`super::queue`]) and `node`
//! ([`super::node`], whose items its doc lists); "records" is the
//! events a row owns ([`super::owner`]), which [`LedgerWindow::events`]
//! gives back for today's functions. A window holds them once, in its own
//! list, none on its rows. "Queue rows" below are the store's: a window keeps
//! none of them, so what it gives of one comes of its records alone
//! ([`WindowRows`]). The time each is recorded, and what
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
//! baseline (`基準より前`); the reverts and failures after a landing are
//! apart from a 0 before their record (`記録より前`), with no record, and
//! after an import that could not read a line
//! ([`post_landing::Coverage`]).
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
//! | `sessions` (`by_kind`, `by_route`, per run and goal) | session rows (the spans of no run) and run rows' `sessions` (a run on its way's folded at the read); their tokens and `context` from the rows' `executions` ([`LedgerWindow::executions`]) | the spans overlapping the window, the Executions that ended in it |
//! | `execution_tokens` | every run and session row's Executions, whatever rows the window chose, so that `recorded_from` and `coverage` are the queue's ([`LedgerWindow::execution_tokens`]; each one's end, a cut's time; actor × provider × model as `stats` sums them) | ended in the window's times and of its filters |
//! | `planner_routes` | session rows of `runtime_planner` (and their records), with their `executions`; the `planner_question` asks of a task from its task row's records, a task not ended too | the cursors' window |
//! | `jobs` | session rows of the jobs of no run and run rows' `jobs` (kind, provider, model, mode, verdict, time) | ended in the window |
//! | `claim_holds`, `landing_holds`, `claim_deferrals` | queue `claim_hold`, `landing_hold`, `claim_deferral` rows | started in the window; the one open at its end |
//! | `updates` | queue event rows of `update_*` | the cursors' window |
//! | `landing_utilization` | queue `landing_slot` rows and run rows' records (the waits) | overlapping the window |
//! | `host` (`samples`, `first`, `last`, `metrics`: each column's `min`, `mean`, `median`, `max`, `p90`; `cpu_secs`: `covered_secs`, `max_gap_secs`, `total`, `by_kind`) | node rows of `host`: their `detail.samples` summarized whole by [`crate::domain::host_metrics::summarize`] ([`super::node::host_summary`]) | the window's seconds, from `--since`'s time (else its first event's) to its end, both inclusive ([`LedgerWindow::host`]) |
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
//! | `host` (as `stats`' `host`) | node rows of `host`: `detail.samples` summarized whole ([`super::node::host_summary`]) over each period's seconds after its start to its end, so two periods never share a sample, and over each `--compare` window's the same way ([`LedgerWindow::host_between`]) |
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
//! | `host` (each period's `host`, `cpu_per_landing`, `load_per_core` and `health.disk` in the JSON; the HTML's section of the host's CPU per landing and load over the cores) | as `kpi` above: node rows of `host`, each period's seconds ([`LedgerWindow::host_between`]) |

use std::collections::{BTreeSet, HashMap};
#[cfg(test)]
use std::convert::Infallible;

use omitted::OmittedEdges;

use super::{
    LedgerKind, LedgerRow, WindowEnd, attributes, findings, node, post_landing,
    read::{LedgerValues, read},
    session::row_executions,
};
use crate::domain::{
    EventId, EventKind, GoalId, RunEvent, RunId, TaskChange, TaskId,
    areas::{AreaMap, run_areas},
    host_metrics::HostSummary,
    live_alerts::{self, FoldedJudgments},
    run::{AskRef, AtOf, PayloadAt, StatusOf, restore_payload},
    stats::{
        Cursor, History,
        context::{self, ContextRecord},
        executions::{self, Execution, ExecutionStats},
        landing_utilization,
        ledger_times::LedgerTimes,
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
    /// Read no row the periods from this time need nothing of (unix ms;
    /// module doc): none reads every row. Apart from `since`, which is the
    /// window `stats` answers of and moves its answer.
    pub read_from_ms: Option<i64>,
    /// The bounds the reader takes inside the window past its edges
    /// ([`bounds`]): each period of `kpi` and of its comparison (the last
    /// event at or before its start and end), the time of each event id
    /// cursor of `kpi` (`--since`, `--until`, `--at`, `--compare`), its
    /// first time without `--since`, and the first time of the page of
    /// `stats` without `--full` and `--since`. The read answers each of
    /// the same snapshot as the rows, the omitted queue events of (C, S]
    /// too, and the reader takes the window's answers
    /// ([`LedgerWindow::answers`]), not those it would work out again of
    /// [`LedgerWindow::events`], which miss the omitted events when a clock
    /// turned back. A reader that takes a new bound inside a window
    /// declares it here with the others: one not declared has no answer.
    /// None declared reads none.
    pub bounds: bounds::Declared,
}

/// Which rows a read from a time leaves out of the store's read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowsFrom {
    /// A run, session or task row that ended before this (unix ms); a node
    /// row (its end exclusive, [`node::covers`]) that holds no sample of its
    /// second on.
    pub ended_before_ms: i64,
    /// A run, session or task row whose inputs stop at this event or before
    /// it, a record of the queue's of the time or before it: a row counts the
    /// records that arrive after its end, by their ids. Taken among the
    /// queue's records ([`WindowRows::rows_from`]), so no row but the ledger's
    /// is read for it.
    pub inputs_through: EventId,
}

/// The form of the time a record writes and [`RowsFrom::time_text`] has, as a
/// SQL `LIKE` pattern: a text of this form orders as its time. One in another
/// form, which [`crate::domain::stats::timestamp_millis`] may still read, is not
/// taken as before anything.
pub const RECORD_TIME_FORM: &str = "____-__-__T__:__:__.___Z";

impl RowsFrom {
    /// A node row that ends at this (unix ms) or before is left out.
    pub const fn node_ended_by_ms(&self) -> i64 {
        self.ended_before_ms.div_euclid(1000) * 1000
    }

    /// The time, as a record writes its time.
    pub fn time_text(&self) -> String {
        crate::domain::marks::utc_text(self.ended_before_ms)
    }

    /// Whether `record` stands before the time ([`Self::time_text`]) in all
    /// it tells of when it happened: when it was recorded, and the time an
    /// Execution of it counts by ([`executions::executions`]).
    fn before_the_time(record: &RunEvent, time: &str) -> bool {
        let in_form = |text: &str| {
            text.len() == RECORD_TIME_FORM.len()
                && text
                    .bytes()
                    .zip(RECORD_TIME_FORM.bytes())
                    .all(|(of, form)| form == b'_' || of.eq_ignore_ascii_case(&form))
        };
        let before = |text: &str| in_form(text) && text < time;
        before(&record.created_at)
            && match restore_payload::<AtOf>(&record.payload).at() {
                PayloadAt::None => true,
                PayloadAt::Text(at) => before(&at),
                PayloadAt::Other => false,
            }
    }

    /// Whether the read leaves `row` out, as a store's condition on the row
    /// does.
    pub fn leaves_out(&self, row: &LedgerRow) -> bool {
        match row.kind {
            LedgerKind::Queue => false,
            LedgerKind::Node => row
                .ended_at
                .is_some_and(|end| end <= self.node_ended_by_ms()),
            LedgerKind::Run | LedgerKind::Task | LedgerKind::Session => {
                let time = self.time_text();
                !row.is_legacy()
                    && row.started_at.is_some()
                    && row.ended_at.is_some_and(|end| end < self.ended_before_ms)
                    && row.inputs_through.events <= self.inputs_through
                    && row
                        .records
                        .iter()
                        .all(|record| Self::before_the_time(record, &time))
            }
        }
    }
}

/// The kinds a read from a time still picks from the run rows it leaves out
/// ([`LedgerStore`](crate::application::LedgerStore)'s `ledger_earlier`),
/// with those [`ends_run_occupancy`] reads. `run_claimed` for
/// [`Earlier::claims`]. A follow-up's registration names its draft on the
/// source run, the plan's changes on a task count on a forecast's samples
/// after their snapshot however old, a proposal's tasks and how they ended
/// count on it whenever that was: each is read by its payload and its task,
/// so these go among the window's records without their run
/// ([`WindowRows::take_earlier`]), which would stand for a run with no claim.
/// The asks say which runs may still hold one open: one with no answer is
/// read with its run, closed or not, as the alert of an unanswered ask reads it.
/// So do the sessions: one with no close counts as open in every period.
pub const EARLIER_RUN_KINDS: [EventKind; 20] = [
    EventKind::RunClaimed,
    EventKind::RunIntegrated,
    EventKind::RunSatisfiedClosed,
    EventKind::AskOpened,
    EventKind::AskAnswered,
    EventKind::SessionOpened,
    EventKind::SessionClosed,
    EventKind::FollowUpRegistered,
    EventKind::TaskPriorityChanged,
    EventKind::DependencyAdded,
    EventKind::DependencyRemoved,
    EventKind::GoalDependencyAdded,
    EventKind::GoalDependencyRemoved,
    EventKind::TaskSubmitted,
    EventKind::TaskReopened,
    EventKind::TaskStatusChanged,
    EventKind::GoalSubmitted,
    EventKind::ProposalWithdrawn,
    EventKind::PlanDecided,
    EventKind::ProposalSettled,
];

/// The `status` values with which an event ends its run's occupancy of a slot.
pub const RUN_END_STATUSES: [&str; 3] = ["succeeded", "failed", "interrupted"];

/// Whether `event` ends its run's occupancy of a slot in `kpi`
/// ([`crate::domain::kpi`]'s `slot_usage`): a run with no such event holds
/// its slot up to now, so a read leaves none of those out.
pub fn ends_run_occupancy(event: &RunEvent) -> bool {
    event.kind == EventKind::RunIntegrated
        || event.kind == EventKind::RunSatisfiedClosed
        || restore_payload::<StatusOf>(&event.payload)
            .status()
            .is_some_and(|status| RUN_END_STATUSES.contains(&status))
}

/// What a store gives of the rows a read from a time leaves out, and what
/// the window keeps of it apart from its rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Earlier {
    /// The claims of the runs left out, ascending id: a claim among
    /// [`LedgerWindow::events`] would stand for a run the read does not hold.
    pub claims: Vec<RunEvent>,
    /// The first Execution of each actor and provider of the run rows left
    /// out, by their entries.
    pub executions: Vec<executions::FirstExecution>,
    /// What the landings' attempts and waits read of the runs left out
    /// ([`landing_utilization::reads`]), ascending id and with their runs: an
    /// attempt starts after the one before it, of any run, and ends with the
    /// next start ([`landing_utilization::Attempts::add`]).
    pub landings: Vec<RunEvent>,
}

/// What a store reads of the rows a read from a time leaves out, whole: not
/// the rows, which hold far more than their records.
#[derive(Debug, Default)]
pub struct EarlierRecords {
    /// Every record of the task and session rows, and those of
    /// [`EARLIER_RUN_KINDS`], [`RUN_END_STATUSES`] and
    /// [`landing_utilization::reads`] of the run rows.
    pub records: Vec<RunEvent>,
    pub executions: Vec<executions::FirstExecution>,
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
    /// The rows in the window the filters chose, by their first input, as
    /// [`WindowRows`] leaves them: of no queue row and no records.
    pub rows: Vec<LedgerRow>,
    /// The records of every row read, each once and only here, the window
    /// and the filters aside: what the
    /// folds that carry state read, each to [`Self::end`] (a baseline's
    /// group is taken only with its mark, which may come after the
    /// window).
    records: Vec<RunEvent>,
    /// Which of [`Self::records`] are up to [`Self::end`].
    held: Held,
    /// The filters, as the counts of the tokens read them.
    filters: Filters,
    /// What the rows a read from a time left out still give.
    earlier: Earlier,
    /// The answers of the bounds the query declared ([`bounds`]).
    answers: Option<bounds::Answers>,
}

/// The runs the task row `row` names ([`super::fold::TaskLife::runs`]),
/// in the order it names them; none for another kind.
pub fn runs_named(row: &LedgerRow) -> Vec<String> {
    if row.kind != LedgerKind::Task {
        return Vec::new();
    }
    row.detail
        .get("runs")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// The runs on their way among `rows`: those a task row not ended names
/// ([`super::fold::TaskLife::runs`]) with no run row among `rows`, as the
/// builder writes a run's row only once its life ended. Each once, in
/// the order the task rows name them.
pub fn runs_on_their_way(rows: &[LedgerRow]) -> Vec<RunId> {
    let written: BTreeSet<&str> = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Run)
        .map(|row| row.key.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    rows.iter()
        .filter(|row| row.ended_at.is_none())
        .flat_map(runs_named)
        .filter(|run| !written.contains(run.as_str()) && seen.insert(run.clone()))
        .filter_map(|run| RunId::new(&run).ok())
        .collect()
}

/// The row of the run `run` on its way, folded at `now_ms` from its
/// `events` by the one fold the builder writes rows with
/// ([`super::run_row`]): not settled, its intervals up to now. None
/// without events.
pub fn run_on_its_way(run: &RunId, events: &[RunEvent], now_ms: i64) -> Option<LedgerRow> {
    (!events.is_empty()).then(|| super::run_row(run.as_str(), events, &[], now_ms, ON_ITS_WAY))
}

/// The `built_at` of a row [`run_on_its_way`] folds at a read, which no
/// store holds.
pub const ON_ITS_WAY: &str = "folded at the read";

/// Where the window `query` asks stands among records, as [`LedgerWindow`]
/// gives it: by id, its start and end, and by time, its start and end.
pub(super) struct Positions {
    pub after: EventId,
    pub upto: EventId,
    pub start_ms: Option<i64>,
    pub end_ms: i64,
}

impl Positions {
    /// Of `records` (any order), `time_of` the time of the record of an id
    /// (none when none is, or of no time).
    pub(super) fn of<'a>(
        records: impl Iterator<Item = &'a RunEvent> + Clone,
        query: &WindowQuery,
        time_of: impl Fn(EventId) -> Option<i64>,
    ) -> Self {
        // As [`Cursor::event_id`] of the records.
        let id_of = |cursor: Cursor| match cursor {
            Cursor::Event(id) => id,
            Cursor::Time(millis) => records
                .clone()
                .filter(|event| timestamp_millis(&event.created_at).is_some_and(|at| at <= millis))
                .map(|event| event.id)
                .max()
                .unwrap_or(EventId::new(0)),
        };
        let since = query.since.map(id_of);
        let until = query.until.map(id_of);
        let latest = records
            .clone()
            .map(|event| event.id)
            .max()
            .unwrap_or(EventId::new(0));
        let upto = until.map_or(latest, |until| {
            let capped = until.min(latest);
            since.map_or(capped, |since| capped.max(since.min(latest)))
        });
        // As `stats` times its window: to `--until`'s time (not past now), else
        // the time of the event it stops at, else now; from `--since`'s, not
        // past the end.
        let stops_at = until.and_then(|_| time_of(upto));
        let end_ms = match query.until {
            Some(Cursor::Time(millis)) => millis.min(query.now_ms),
            _ => stops_at.unwrap_or(query.now_ms),
        };
        let start_ms = query
            .since
            .and_then(|cursor| match cursor {
                Cursor::Time(millis) => Some(millis),
                Cursor::Event(id) => time_of(id),
            })
            .map(|start| start.min(end_ms));
        Self {
            after: since.unwrap_or(EventId::new(0)),
            upto,
            start_ms,
            end_ms,
        }
    }
}

/// How many rows [`WindowRows::take_pages`] asks for at a time: what a read
/// holds of a row's text and values beyond what the window keeps of it.
pub const ROWS_PER_PAGE: usize = 50;

/// The room [`WindowRows::new`] leaves past the events it is told of, for
/// those recorded while the read goes on (about an hour of a queue's), so
/// that they do not double the list of all the records.
const EVENTS_DURING_A_READ: usize = 1024;

/// The goals the records of the rows that are neither a run's nor a task's
/// name, by each row's kind and key: what [`Filters::goals_of`] reads once
/// the records have left the rows.
type RecordGoals = HashMap<(LedgerKind, String), BTreeSet<GoalId>>;

/// The goals `row`'s records name, for [`RecordGoals`].
fn record_goals(row: &LedgerRow) -> BTreeSet<GoalId> {
    row.records
        .iter()
        .filter_map(|event| event.goal_id)
        .collect()
}

/// The rows of a window as they are read, each folded to what the window
/// answers with as it comes, so that a read holds its rows once and a page
/// of them at most in full: a row's records move to the window's, a queue
/// row is dropped (nothing of a window reads one but its records) and a run
/// row loses its steps ([`super::steps::DETAIL_KEY`], which only what
/// rebuilds a row reads). A row's records are read for its goals before
/// they move, under a filter of a goal only.
#[derive(Debug)]
pub struct WindowRows {
    picks_goals: bool,
    records: Vec<RunEvent>,
    rows: Vec<LedgerRow>,
    goals: RecordGoals,
    earlier: Earlier,
    /// The records [`Self::take_earlier`] puts among the window's. After the
    /// rows' when the window is made: a record of a row taken too counts as
    /// the row holds it.
    earlier_records: Vec<RunEvent>,
    /// [`WindowQuery::read_from_ms`] and, for it, what the rows taken so far
    /// make of it.
    from: Option<ReadFrom>,
    /// The records as text, of a read that holds them so
    /// ([`payload_text`]); none of the runtime's reads.
    text: Option<payload_text::TextRecords>,
}

/// What a read from a time keeps track of as it takes the rows.
#[derive(Debug, Default)]
struct ReadFrom {
    ms: i64,
    /// The latest of the queue's records of the time or before it: a
    /// period's first event id is at or past it, whatever else is read.
    queue_before: EventId,
    /// The rows taken or asked for, and those their rows and records name.
    asked: BTreeSet<(LedgerKind, String)>,
    wanted: BTreeSet<(LedgerKind, String)>,
}

impl ReadFrom {
    /// Note what `row` names and how far the queue's records reach before
    /// the time.
    fn note(&mut self, row: &LedgerRow) {
        self.asked.insert((row.kind, row.key.clone()));
        if row.kind == LedgerKind::Node {
            return;
        }
        // A queue row's task is only that of the wait it stands for.
        if row.kind != LedgerKind::Queue
            && let Some(task) = row.task_id
        {
            self.wanted.insert((LedgerKind::Task, task.to_string()));
        }
        for event in &row.records {
            let at = timestamp_millis(&event.created_at);
            if row.kind == LedgerKind::Queue && at.is_some_and(|at| at <= self.ms) {
                self.queue_before = self.queue_before.max(event.id);
            }
            if let Some(task) = event.task_id
                && at.is_none_or(|at| at >= self.ms)
            {
                self.wanted.insert((LedgerKind::Task, task.to_string()));
            }
        }
        if row.kind == LedgerKind::Task {
            let named = row
                .detail
                .get("runs")
                .or_else(|| row.legacy.as_ref()?.get("runs"))
                .and_then(serde_json::Value::as_array);
            for run in named.into_iter().flatten().filter_map(|run| run.as_str()) {
                self.wanted.insert((LedgerKind::Run, run.to_owned()));
            }
        }
    }
}

impl WindowRows {
    /// A read for `query`, with room for `events` records and those recorded
    /// during it (more are taken too, by growing the list).
    pub fn new(query: &WindowQuery, events: usize) -> Self {
        Self {
            picks_goals: query.goal_id.is_some(),
            records: Vec::with_capacity(events.saturating_add(EVENTS_DURING_A_READ)),
            rows: Vec::new(),
            goals: RecordGoals::new(),
            earlier: Earlier::default(),
            earlier_records: Vec::new(),
            from: query.read_from_ms.map(|ms| ReadFrom {
                ms,
                ..ReadFrom::default()
            }),
            text: None,
        }
    }

    /// Take as the queue's records of the time or before it the omitted
    /// events `edges` reads ([`OmittedEdges::at_or_before`]), once the
    /// queue's rows are taken: they are the queue's records with no row.
    pub fn take_omitted_before<E>(&mut self, edges: &OmittedEdges<'_, E>) -> Result<(), E> {
        if let Some(from) = &mut self.from
            && let Some(id) = edges.at_or_before(from.ms)?
        {
            from.queue_before = from.queue_before.max(id);
        }
        Ok(())
    }

    /// Take among the window's records the omitted events its edges stand
    /// on ([`OmittedEdges::events_for`]), once every row is taken: the rows'
    /// records do not hold them, and a window's edges and `stats`' are read
    /// of its records. A record a row holds too counts once.
    pub fn take_omitted_edges<E>(
        &mut self,
        query: &WindowQuery,
        edges: &OmittedEdges<'_, E>,
    ) -> Result<(), E> {
        let mut events = edges.events_for(query, &[&self.records, &self.earlier_records])?;
        if !events.is_empty() {
            debug_assert!(self.text.is_none(), "a read of the payloads as text");
            self.earlier_records.append(&mut events);
        }
        Ok(())
    }

    /// Which rows a read from a time leaves out, once the queue's rows are
    /// taken: the inputs left out stop at the queue's last record of the time
    /// or before it, so with none of them no row is left out by its inputs.
    pub fn rows_from(&self) -> Option<RowsFrom> {
        self.from.as_ref().map(|from| RowsFrom {
            ended_before_ms: from.ms,
            inputs_through: from.queue_before,
        })
    }

    /// The rows not taken yet of the tasks the rows taken name and of the
    /// runs those tasks name, each asked for once: take those the store has
    /// and ask again until none is left.
    pub fn rows_wanted(&mut self) -> Vec<(LedgerKind, String)> {
        let Some(from) = &mut self.from else {
            return Vec::new();
        };
        let wanted: Vec<(LedgerKind, String)> = std::mem::take(&mut from.wanted)
            .into_iter()
            .filter(|row| !from.asked.contains(row))
            .collect();
        from.asked.extend(wanted.iter().cloned());
        wanted
    }

    /// Take what the rows left out still give (module doc), in any order:
    /// a record of a row taken too counts once, and a run that holds a slot
    /// (a claim with no end), an ask or a session to this day is wanted whole.
    /// A close the rows left out do not give may be among the rows read: that
    /// run is read whole too, as its session may count in the periods.
    pub fn take_earlier(&mut self, earlier: EarlierRecords) {
        self.earlier.executions = earlier.executions;
        let mut claimed = BTreeSet::new();
        let mut ended = BTreeSet::new();
        let mut asks: HashMap<(RunId, Option<String>), bool> = HashMap::new();
        // Paired as `stats` pairs an ask's events.
        let ask = |event: &RunEvent, run: &RunId| {
            (run.clone(), restore_payload::<AskRef>(&event.payload).key())
        };
        let closed: BTreeSet<EventId> = earlier
            .records
            .iter()
            .filter(|event| event.kind == EventKind::SessionClosed)
            .filter_map(|event| crate::domain::sessions::SpanRef::of(event).opened_event_id)
            .collect();
        let mut open_sessions = BTreeSet::new();
        for mut event in earlier.records {
            let Some(run) = event.run_id.clone() else {
                self.earlier_records.push(event);
                continue;
            };
            if landing_utilization::reads(&event) {
                self.earlier.landings.push(event.clone());
            }
            if ends_run_occupancy(&event) {
                ended.insert(run.clone());
            }
            match event.kind.known() {
                Some(EventKind::RunClaimed) => {
                    claimed.insert(run);
                    self.earlier.claims.push(event);
                }
                Some(EventKind::AskOpened) => {
                    asks.entry(ask(&event, &run)).or_insert(true);
                }
                Some(EventKind::AskAnswered) => {
                    asks.insert(ask(&event, &run), false);
                }
                Some(EventKind::SessionOpened) => {
                    if !closed.contains(&event.id) {
                        open_sessions.insert(run);
                    }
                }
                Some(
                    EventKind::RunIntegrated
                    | EventKind::RunSatisfiedClosed
                    | EventKind::SessionClosed,
                ) => {}
                Some(kind) if EARLIER_RUN_KINDS.contains(&kind) => {
                    event.run_id = None;
                    self.earlier_records.push(event);
                }
                _ => {}
            }
        }
        if let Some(from) = &mut self.from {
            let open_asks = asks
                .into_iter()
                .filter(|(_, open)| *open)
                .map(|((run, _), _)| run);
            from.wanted.extend(
                claimed
                    .into_iter()
                    .filter(|run| !ended.contains(run))
                    .chain(open_asks)
                    .chain(open_sessions)
                    .map(|run| (LedgerKind::Run, run.as_str().to_owned())),
            );
        }
        if let Some(text) = &mut self.text {
            text.take_earlier(&mut self.earlier_records);
        }
    }

    /// Take `row`, folded to what the window keeps of it.
    pub fn take(&mut self, mut row: LedgerRow) {
        if let Some(from) = &mut self.from {
            from.note(&row);
        }
        if self.picks_goals && row.kind == LedgerKind::Session {
            self.goals
                .insert((row.kind, row.key.clone()), record_goals(&row));
        }
        self.records.append(&mut row.records);
        row.records = Vec::new();
        if let Some(text) = &mut self.text {
            text.take(&mut self.records);
        }
        match row.kind {
            LedgerKind::Queue => return,
            LedgerKind::Run => {
                if let Some(detail) = row.detail.as_object_mut() {
                    detail.remove(super::steps::DETAIL_KEY);
                }
            }
            _ => {}
        }
        self.rows.push(row);
    }

    /// Take a row asked for by its key, if the store has it.
    pub fn take_row(&mut self, row: Option<LedgerRow>) {
        if let Some(row) = row {
            self.take(row);
        }
    }

    /// Take every row `page` gives, asked for [`ROWS_PER_PAGE`] at a time
    /// after the last key it gave (`None`: from the first), by key: a page
    /// may hold fewer with more after it, so only an empty page ends them.
    pub fn take_pages<E>(
        &mut self,
        mut page: impl FnMut(Option<&str>, usize) -> Result<Vec<LedgerRow>, E>,
    ) -> Result<(), E> {
        let mut after: Option<String> = None;
        loop {
            let rows = page(after.as_deref(), ROWS_PER_PAGE)?;
            let Some(last) = rows.last() else {
                return Ok(());
            };
            after = Some(last.key.clone());
            for row in rows {
                self.take(row);
            }
        }
    }

    /// The runs on their way among the rows taken ([`runs_on_their_way`]).
    pub fn runs_on_their_way(&self) -> Vec<RunId> {
        runs_on_their_way(&self.rows)
    }

    /// The window `query` asks of the rows taken: the cursors and the times
    /// resolved as `stats` resolves them (a `--since` past `--until` leaves
    /// nothing), the rows with an input by the window's end that have not
    /// ended before its start, then those of the filters.
    pub fn into_window(mut self, query: &WindowQuery) -> LedgerWindow {
        debug_assert!(self.text.is_none(), "a read of the payloads as text");
        self.rows.shrink_to_fit();
        self.records.append(&mut self.earlier_records);
        self.earlier_records = Vec::new();
        self.records.shrink_to_fit();
        self.records.sort_by_key(|event| event.id);
        self.records.dedup_by_key(|event| event.id);
        let earlier = settled(self.earlier, |id| {
            self.records
                .binary_search_by_key(&id, |read| read.id)
                .is_ok()
        });
        let mut window = of_records(self.records, self.rows, &self.goals, query);
        window.earlier = earlier;
        window
    }
}

/// `earlier` in id order, each once, of none of the records `read` holds.
fn settled(mut earlier: Earlier, read: impl Fn(EventId) -> bool) -> Earlier {
    for events in [&mut earlier.claims, &mut earlier.landings] {
        events.sort_by_key(|event| event.id);
        events.dedup_by_key(|event| event.id);
        events.retain(|event| !read(event.id));
        events.shrink_to_fit();
    }
    earlier
}

/// The window `query` asks of `rows` (every kind's), read all at once: the
/// tests' way to [`WindowRows`], which the runtime reads by pages
/// ([`crate::application::LedgerStore::ledger_window`]).
#[cfg(test)]
pub(crate) fn select(rows: Vec<LedgerRow>, query: &WindowQuery) -> LedgerWindow {
    select_with(rows, query, &OmittedEdges::<Infallible>::none()).unwrap()
}

/// [`select`] with the omitted events of `edges` its edges stand on.
#[cfg(test)]
pub(crate) fn select_with<E>(
    rows: Vec<LedgerRow>,
    query: &WindowQuery,
    edges: &OmittedEdges<'_, E>,
) -> Result<LedgerWindow, E> {
    let events = rows.iter().map(|row| row.records.len()).sum();
    let mut read = WindowRows::new(query, events);
    for row in rows {
        read.take(row);
    }
    read.take_omitted_edges(query, edges)?;
    let mut window = read.into_window(query);
    bounds::answer(&mut window, query, edges)?;
    Ok(window)
}

/// The window `query` asks of `rows` read as a store reads them: what
/// [`crate::application::LedgerStore::ledger_window`] does, of rows in memory,
/// with [`RowsFrom::leaves_out`] for the store's condition.
#[cfg(test)]
pub(crate) fn select_from_time(rows: Vec<LedgerRow>, query: &WindowQuery) -> LedgerWindow {
    select_from_time_with(rows, query, &OmittedEdges::<Infallible>::none()).unwrap()
}

/// [`select_from_time`] with the omitted events of `edges`: among the
/// queue's records before the time, and those its edges stand on.
#[cfg(test)]
pub(crate) fn select_from_time_with<E>(
    rows: Vec<LedgerRow>,
    query: &WindowQuery,
    edges: &OmittedEdges<'_, E>,
) -> Result<LedgerWindow, E> {
    let mut read = WindowRows::new(query, 0);
    let (queue, others): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|row| row.kind == LedgerKind::Queue);
    for row in queue {
        read.take(row);
    }
    read.take_omitted_before(edges)?;
    let Some(from) = read.rows_from() else {
        for row in others {
            read.take(row);
        }
        read.take_omitted_edges(query, edges)?;
        let mut window = read.into_window(query);
        bounds::answer(&mut window, query, edges)?;
        return Ok(window);
    };
    let (out, kept): (Vec<_>, Vec<_>) = others.into_iter().partition(|row| from.leaves_out(row));
    for row in kept {
        read.take(row);
    }
    let mut earlier = EarlierRecords::default();
    for row in out.iter().filter(|row| row.kind != LedgerKind::Node) {
        if row.kind == LedgerKind::Run {
            earlier.executions.extend(
                row_executions(row).iter().filter_map(|e| {
                    executions::FirstExecution::of(e.actor, e.provider.clone(), e.at)
                }),
            );
            earlier.records.extend(
                row.records
                    .iter()
                    .filter(|event| {
                        event
                            .kind
                            .known()
                            .is_some_and(|kind| EARLIER_RUN_KINDS.contains(&kind))
                            || ends_run_occupancy(event)
                            || landing_utilization::reads(event)
                    })
                    .cloned(),
            );
        } else {
            earlier.records.extend(row.records.iter().cloned());
        }
    }
    read.take_earlier(earlier);
    loop {
        let wanted = read.rows_wanted();
        if wanted.is_empty() {
            read.take_omitted_edges(query, edges)?;
            let mut window = read.into_window(query);
            bounds::answer(&mut window, query, edges)?;
            return Ok(window);
        }
        for (kind, key) in wanted {
            read.take_row(
                out.iter()
                    .find(|row| row.kind == kind && row.key == key)
                    .cloned(),
            );
        }
    }
}

/// The window [`select`] gives, in the form that keeps all it is given:
/// every row with its records and its steps, the queue's too, and a copy of
/// the records in the window's. What the tests hold the folded window's
/// answers to.
#[cfg(test)]
pub(crate) fn select_holding_every_row(rows: Vec<LedgerRow>, query: &WindowQuery) -> LedgerWindow {
    let mut all: Vec<RunEvent> = rows
        .iter()
        .flat_map(|row| row.records.iter().cloned())
        .collect();
    all.sort_by_key(|event| event.id);
    all.dedup_by_key(|event| event.id);
    let goals = rows
        .iter()
        .filter(|row| !matches!(row.kind, LedgerKind::Run | LedgerKind::Task))
        .map(|row| ((row.kind, row.key.clone()), record_goals(row)))
        .collect();
    let mut window = of_records(all, rows, &goals, query);
    bounds::answer(&mut window, query, &OmittedEdges::<Infallible>::none()).unwrap();
    window
}

/// The window `query` asks of `rows`, `all` their records (ascending id) and
/// `goals` the goals those records name.
fn of_records(
    all: Vec<RunEvent>,
    rows: Vec<LedgerRow>,
    goals: &RecordGoals,
    query: &WindowQuery,
) -> LedgerWindow {
    let Positions {
        after,
        upto,
        start_ms,
        end_ms,
    } = Positions::of(all.iter(), query, |id| {
        all.binary_search_by_key(&id, |event| event.id)
            .ok()
            .and_then(|at| timestamp_millis(&all[at].created_at))
    });
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
        held: Held::of(&all, end),
        records: all,
        filters: Filters::default(),
        earlier: Earlier::default(),
        answers: None,
    };
    let filters = Filters::of(&window, query);
    let mut chosen: Vec<LedgerRow> = rows
        .into_iter()
        .filter(|row| window.overlaps(row) && filters.keeps(row, goals))
        .collect();
    chosen.sort_by_key(|row| (row.inputs_from, row.kind));
    window.rows = chosen;
    window.filters = filters;
    window
}

/// The records up to a window's end, for [`LedgerWindow::events`] to lend.
/// Every read of `stats`, `kpi` and `report` folds them, some more than
/// once, so they are worked out once, and the payloads are not copied for
/// them unless the slice they must be has no other way to be contiguous.
#[derive(Debug, Clone)]
enum Held {
    /// The first of the records: all of them when the end drops none.
    First(usize),
    /// A record past the end stands before one within it (a later id of an
    /// earlier time), so they are no run of the records: one copy of them,
    /// held with the window.
    Picked(Vec<RunEvent>),
}

impl Held {
    fn of(records: &[RunEvent], end: WindowEnd) -> Self {
        let first = records.iter().take_while(|event| end.holds(event)).count();
        if records[first..].iter().any(|event| end.holds(event)) {
            Self::Picked(
                records
                    .iter()
                    .filter(|event| end.holds(event))
                    .cloned()
                    .collect(),
            )
        } else {
            Self::First(first)
        }
    }
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
            for (run, areas) in run_areas(map, events, &changed) {
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

    /// The goals a row is of: its task's, else those its records named as
    /// they were read (`of_records`; a goal review's, a plan review's goals).
    fn goals_of(&self, row: &LedgerRow, of_records: &RecordGoals) -> BTreeSet<GoalId> {
        let mut goals: BTreeSet<GoalId> = BTreeSet::new();
        if let Some(goal) = row
            .task_id
            .and_then(|task| self.goals.get(&task).copied().flatten())
        {
            goals.insert(goal);
        }
        if row.kind != LedgerKind::Run && row.kind != LedgerKind::Task {
            if let Some(named) = of_records.get(&(row.kind, row.key.clone())) {
                goals.extend(named.iter().copied());
            }
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

    fn keeps(&self, row: &LedgerRow, goals: &RecordGoals) -> bool {
        // The host's load is the queue's, whatever is filtered.
        if row.kind == LedgerKind::Node {
            return true;
        }
        if let Some(goal) = self.goal_id
            && !self.goals_of(row, goals).contains(&goal)
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

#[cfg(test)]
impl LedgerWindow {
    /// Where the host's load is read from (unix milliseconds): the first
    /// time of a window without `--since`.
    pub(crate) const fn host_from_ms(&self) -> i64 {
        self.host_from_ms
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
            // From `--since`'s time only: without it every node row up to
            // the end, as `kpi`'s periods and `report` read the host's
            // load of any span ([`Self::host_between`]); `stats`' `host`
            // still sums only its own seconds ([`Self::host`]).
            let from = self.start_ms.map_or(0, |start| start.div_euclid(1000));
            let until = self.end_secs();
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

    /// The window's runs' times under today's names ([`LedgerTimes`]):
    /// each run row's by the one read ([`super::read::times`]), a legacy
    /// row's frozen values as they are. A run before the switch with no
    /// row of its own, which a task row names (or one on its way folded at
    /// the read, whose values would be made again of the events), has none
    /// and is in `missing`.
    pub fn run_times(&self) -> LedgerTimes {
        let mut times = LedgerTimes::default();
        for row in self.of(LedgerKind::Run) {
            let (Ok(run), Some(task)) = (RunId::new(row.key.clone()), row.task_id) else {
                continue;
            };
            if row.is_legacy() && row.built_at == ON_ITS_WAY {
                times.missing.insert(run, task);
            } else {
                times.runs.insert(run, super::read::times(row));
            }
        }
        // A run with a row outside the window still owns records.
        let recorded: BTreeSet<&RunId> = self
            .records
            .iter()
            .filter_map(|event| event.run_id.as_ref())
            .collect();
        for row in self.of(LedgerKind::Task).filter(|row| row.is_legacy()) {
            let Some(task) = row.task_id else { continue };
            let named = row
                .detail
                .get("runs")
                .or_else(|| row.legacy.as_ref()?.get("runs"))
                .and_then(serde_json::Value::as_array);
            for run in named.into_iter().flatten() {
                if let Some(run) = run.as_str().and_then(|run| RunId::new(run).ok())
                    && !times.runs.contains_key(&run)
                    && !recorded.contains(&run)
                {
                    times.missing.insert(run, task);
                }
            }
        }
        times
    }

    /// The events up to the window's end ([`Self::end`]), ascending id:
    /// every row's records, whatever the filters, so that today's functions
    /// of `stats`, `kpi` and `report` read from the rows what they read of
    /// the EventStore. Lent, however often it is asked: a read holds no
    /// more copies of the payloads for it ([`Held`]).
    pub fn events(&self) -> &[RunEvent] {
        match &self.held {
            Held::First(count) => &self.records[..*count],
            Held::Picked(events) => events,
        }
    }

    /// What the rows a read from a time left out still give.
    pub const fn earlier(&self) -> &Earlier {
        &self.earlier
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
        fold_main_log(self.events())
    }

    /// Main's history for the window, as `conflict_hotspots` spans it, or
    /// why it has none (`記録が無い`).
    pub fn main_history(&self) -> History {
        let events = self.events();
        fold_main_log(events).history_for(events, self.after, self.upto)
    }

    /// Whether main's history covers the window's times.
    pub fn main_covers(&self) -> Result<(), MainMissing> {
        let events = self.events();
        fold_main_log(events).covers(events, self.start_ms, Some(self.end_ms))
    }

    /// The reverts and CI's failures after the landings, matched to their
    /// runs as the records give them at the window's end: every row's,
    /// whatever the filters ([`post_landing::fold`]).
    pub fn post_landing_book(&self) -> post_landing::PostLandingBook {
        post_landing::fold(self.events(), self.end_ms)
    }

    /// What befell each of the window's runs (its chosen run rows, by the
    /// filters) after its landing, read at the window's end.
    pub fn post_landing(&self) -> Vec<post_landing::PostLanding> {
        let book = self.post_landing_book();
        self.of(LedgerKind::Run)
            .filter_map(|row| RunId::new(row.key.clone()).ok())
            .map(|run| book.of_run(&run))
            .collect()
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
        executions::window_after(
            &all,
            &self.earlier.executions,
            self.start_ms,
            self.end_ms,
            |execution| self.filters.counts(execution),
        )
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

pub mod bounds;
#[doc(hidden)]
pub mod omitted;
pub mod payload_text;

#[cfg(test)]
mod tests;
