//! The wiring of 観測と分析 (docs/design/architecture.md): `status`,
//! `doctor`, `stats`, `kpi`, `forecast`, `report`, the queue's reads
//! (`read_queue`, also the queue service's), `observe`,
//! `throughput-review`, `events`, `timeline`, `watch` and `ci failures`,
//! and the ports of the supervisor's reports, forecasts and CI watch. Each
//! entry builds the adapters and the settings it reads and calls the use
//! case; what is decided is in `application` and `domain`.

use super::{
    OneShot, bound_checkout, bound_main_checkout, goal_tags, open_queue_watch, provider_checks,
    queue_service_view, task_changes, worker_adapters,
};
use crate::domain::{EventFilter, EventId, EventKind};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::{
    application::{
        AgentProvider, Verifier, health,
        stats::{self as statistics, StatsSources},
    },
    domain::{RunId, SessionRole, TaskId, stats::StatsQuery},
    infrastructure::{
        adapters::{
            ClaudeCode, Cmux, GitRepository, SystemProcesses, VERIFICATION_TIMEOUT, executable,
            is_dagq_source,
        },
        clock,
        codex::Codex,
        run_env::{ShellVerifier, load_conflict_config, load_kpi_settings, load_stall_config},
        run_files::LocalRunFiles,
        sqlite::{ReadOnlyQueue, SqliteQueue},
        transcripts::ClaudeTranscripts,
    },
};

/// How the supervisor watches the CI ([`SuperviseOptions::ci_watch`](super::SuperviseOptions::ci_watch)).
#[derive(Debug, Clone)]
pub struct CiWatchOptions {
    /// The GitHub CLI: `gh` on the supervisor's PATH, or a path (tests
    /// give a fake).
    pub program: String,
    /// The time between checks instead of `[ci_watch] interval_secs`;
    /// tests shorten it.
    pub interval: Option<Duration>,
}

impl Default for CiWatchOptions {
    fn default() -> Self {
        Self {
            program: crate::infrastructure::ci_watch::GH.to_owned(),
            interval: None,
        }
    }
}

/// Whether the inbox was opened with its guardrail settings
/// ([`crate::application::inbox_guardrail::judge`]), by the newest
/// `inbox_opened`.
fn inbox_guardrail(queue: &SqliteQueue) -> Result<Value> {
    let opened = queue.latest_event_of(EventKind::InboxOpened.as_str())?;
    Ok(crate::application::inbox_guardrail::judge(
        opened.as_ref().map(|event| &event.payload),
    ))
}

/// `status`: see [`status_for`], with all of the attention.
pub fn status(db: &Path) -> Result<Value> {
    status_for(db, None)
}

/// The host's `[update]` for the queue at `db`: its `host.toml` over the
/// host-wide one.
fn host_update(db: &Path) -> crate::infrastructure::release_update::HostUpdate {
    crate::infrastructure::release_update::load_host_update(
        db.parent().unwrap_or(Path::new(".")),
        crate::infrastructure::kpi_config::host_wide_file().as_deref(),
    )
}

/// `status --role` on the system clock: see [`OneShot::status_for`].
pub fn status_for(db: &Path, role: Option<SessionRole>) -> Result<Value> {
    OneShot::system().status_for(db, role)
}

/// `doctor` on the system clock: see [`OneShot::doctor`].
pub fn doctor(db: &Path, full: bool) -> Result<Value> {
    OneShot::system().doctor(db, full, None)
}

fn add_sccache_diagnostics(
    queue: &SqliteQueue,
    db: &Path,
    stats: bool,
    output: &mut Value,
) -> Result<()> {
    let verifier = bound_checkout(queue)?.map(|checkout| ShellVerifier {
        checkout,
        db: db.to_path_buf(),
        user_config: None,
        verification_timeout: VERIFICATION_TIMEOUT,
    });
    let server = crate::infrastructure::sccache::SystemSccache::new(
        db.parent().unwrap_or(Path::new(".")),
        crate::infrastructure::sccache::START_TIMEOUT,
    );
    crate::application::sccache::add_diagnostics(
        queue,
        &server,
        verifier.as_ref().map(|verifier| verifier as &dyn Verifier),
        db.parent().unwrap_or(Path::new(".")),
        stats,
        output,
    )
}

/// The programs the `[run.env]` of the main checkout of the repository the
/// queue is bound to names, resolved on this process's PATH (ADR-0049
/// decision 9).
fn doctor_run_env(queue: &SqliteQueue, db: &Path) -> Result<crate::domain::run_env::RunEnvCheck> {
    // A queue no supervisor or `up` bound has no repository to read, and a
    // repository without a main checkout (`repository` says why) no
    // `dagq.toml`.
    let Some(checkout) = bound_checkout(queue)? else {
        return Ok(crate::domain::run_env::RunEnvCheck::default());
    };
    ShellVerifier {
        checkout,
        db: db.to_path_buf(),
        user_config: None,
        verification_timeout: VERIFICATION_TIMEOUT,
    }
    .run_env_programs(None)
}

/// `doctor`'s `roles` ([`health::roles`]) from the `[roles.*]` of the
/// `dagq.toml` of the bound main checkout.
fn doctor_roles(queue: &SqliteQueue) -> Value {
    health::roles(match bound_checkout(queue) {
        Ok(Some(checkout)) => crate::infrastructure::run_env::load_role_models(&checkout),
        Ok(None) => Ok(Default::default()),
        Err(error) => Err(error),
    })
}

/// `doctor`'s `agents`: see [`crate::application::review::check_agents`],
/// read from the landing branch of the checkout `queue` is bound to; none
/// for a queue bound to none or a landing branch without agents, and
/// `error` when the checkout or the file cannot be read.
fn doctor_agents(queue: &SqliteQueue) -> Result<Option<serde_json::Value>> {
    let Some(checkout) = bound_main_checkout(queue)? else {
        return Ok(None);
    };
    let checked = checkout
        .and_then(|checkout| GitRepository::inspect(&checkout))
        .and_then(|repository| {
            crate::application::review::check_agents(&repository, &|text| {
                Ok(crate::infrastructure::run_env::parse_config(text)?.review_subagents)
            })
        });
    Ok(match checked {
        Ok(value) => value,
        Err(error) => Some(serde_json::json!({"error": format!("{error:#}")})),
    })
}

/// The landing branch and push of the repository the queue is bound to,
/// as `up`'s preflight resolves them (ADR-t615-1), or `error` with why
/// they do not resolve; `None` for a queue bound to no checkout.
fn doctor_repository(queue: &SqliteQueue) -> Result<Option<serde_json::Value>> {
    let Some(checkout) = bound_main_checkout(queue)? else {
        return Ok(None);
    };
    let resolved = checkout
        .and_then(|checkout| GitRepository::inspect(&checkout))
        .and_then(|repository| repository.resolve_repository_settings());
    Ok(Some(match resolved {
        // A configured push remote that is missing keeps the fields.
        Ok(settings) => {
            let mut value = serde_json::to_value(&settings)?;
            if let Err(error) = settings.push.check() {
                value["error"] = format!("{error:#}").into();
            }
            value
        }
        Err(error) => serde_json::json!({"error": format!("{error:#}")}),
    }))
}

/// What the KPIs and their reports of the queue at `db` are made with at
/// `now`: the `[kpi]` of `checkout`'s `dagq.toml` with the host's
/// `host.toml` (the queue's, over the host-wide one) over it, the host's
/// time zone and cores, the `[report]` retention of `host.toml`, the
/// reports' root (`out`, else `<queue dir>/reports/`), and the host's d2
/// and TALA on this process's PATH for the dependency diagram.
fn report_setup(
    db: &Path,
    checkout: Option<&Path>,
    out: Option<&Path>,
    host_wide: Option<&Path>,
    now: i64,
) -> Result<crate::application::report::ReportSetup> {
    use crate::infrastructure::{kpi_config::load_host_kpi, report_config::load_host_report};
    let repository = match checkout {
        Some(checkout) => load_kpi_settings(checkout)?,
        None => None,
    };
    let queue_dir = db.parent().unwrap_or(Path::new("."));
    let host = load_host_kpi(queue_dir, host_wide)?;
    Ok(crate::application::report::ReportSetup {
        root: out.map_or_else(|| queue_dir.join(REPORTS_DIR), Path::to_path_buf),
        host: crate::application::kpi::Host {
            utc_offset_secs: clock::local_utc_offset(now),
            cores: clock::logical_cores(),
            dagq_source: checkout.is_some_and(is_dagq_source),
        },
        config: crate::domain::kpi::KpiConfig::merge(repository.as_ref(), host.as_ref()),
        keep: load_host_report(queue_dir, host_wide)?,
        build: crate::VERSION.to_owned(),
        diagram: d2_renderer(std::env::var_os("PATH")),
        areas: area_reader(checkout)?,
        host_metrics: Some(host_metrics_reader(
            queue_dir.join(crate::domain::host_metrics::HOST_DIR),
        )),
    })
}

/// The `[areas]` of `checkout`'s `dagq.toml` and its Git for the landed
/// commits' changes (ADR-t980-1); no checkout or no `[areas]` gives no run
/// areas.
fn area_reader(checkout: Option<&Path>) -> Result<crate::application::areas::AreaReader> {
    use crate::application::areas::AreaReader;
    let Some(checkout) = checkout else {
        return Ok(AreaReader::none());
    };
    let map = crate::infrastructure::run_env::load_area_map(checkout)?;
    let checkout = checkout.to_path_buf();
    Ok(AreaReader {
        map,
        changed: Arc::new(move |commits: &[String]| {
            GitRepository::inspect(&checkout)?.landed_changes(commits)
        }),
    })
}

/// Summarizes the host's load records under `dir` (task 872).
fn host_metrics_reader(dir: PathBuf) -> crate::application::report::HostMetrics {
    crate::application::report::HostMetrics(std::sync::Arc::new(move |from, until| {
        crate::infrastructure::host_metrics::summary(&dir, from, until)
    }))
}

/// Draws the reports' dependency diagram with d2 and TALA on `path_var`
/// (ADR-0077 decisions 4 and 5).
fn d2_renderer(
    path_var: Option<std::ffi::OsString>,
) -> crate::application::report::DiagramRenderer {
    crate::application::report::DiagramRenderer(Arc::new(move |source| {
        crate::infrastructure::d2::render_svg(
            source,
            path_var.as_deref(),
            crate::infrastructure::d2::RENDER_TIMEOUT,
        )
    }))
}

/// `[kpi]`'s `max_improvement_proposals` of `checkout`'s `dagq.toml`, or
/// its default (ADR-0051 decision 25; the host's settings do not change
/// it).
pub(super) fn max_improvement_proposals(checkout: &Path) -> Result<usize> {
    Ok(load_kpi_settings(checkout)?
        .and_then(|settings| settings.max_improvement_proposals)
        .unwrap_or(crate::domain::kpi::config::DEFAULT_MAX_IMPROVEMENT_PROPOSALS))
}

/// The KPI reports' directory in the queue's (ADR-0051 decision 20).
pub const REPORTS_DIR: &str = "reports";

/// `[ci_watch]` of the checkout `queue` is bound to (ADR-t1920-1); none
/// for a queue bound to none or a file without the table.
fn bound_ci_watch(queue: &SqliteQueue) -> Result<Option<crate::domain::ci_watch::CiWatchConfig>> {
    match bound_checkout(queue)? {
        Some(checkout) => crate::infrastructure::run_env::load_ci_watch(&checkout),
        None => Ok(None),
    }
}

/// `ci failures [--task ID]` on the queue at `db`: see
/// [`crate::application::ci_watch::known_failures`]. Reads only.
pub fn ci_failures(db: &Path, task: Option<TaskId>) -> Result<Value> {
    let queue = SqliteQueue::open_read_only(db)?;
    let config = bound_ci_watch(&queue)?;
    let branch = config.as_ref().and_then(|config| config.branch.clone());
    crate::application::ci_watch::known_failures(&queue, config.as_ref(), branch.as_deref(), task)
}

/// `doctor`'s `ci_watch` (ADR-t1920-1): the table, the `gh` this PATH
/// resolves, whether it is logged in, the repository, and the supervisor's
/// last `ci_watch_unavailable` / `ci_watch_available`; none without the
/// table, `error` when the file cannot be read.
fn doctor_ci_watch(queue: &SqliteQueue) -> Result<Option<Value>> {
    let Some(checkout) = bound_checkout(queue)? else {
        return Ok(None);
    };
    crate::application::ci_watch::doctor(
        queue,
        crate::infrastructure::run_env::load_ci_watch(&checkout),
        |config| {
            crate::infrastructure::ci_watch::doctor_view(
                &checkout,
                config,
                std::env::var_os("PATH").as_deref(),
            )
        },
    )
}

/// `stats` on the system clock: see [`OneShot::stats`].
pub fn stats(db: &Path, query: &StatsQuery) -> Result<Value> {
    OneShot::system().stats(db, query)
}

/// The near-term dependency diagram of `graph --format d2|svg`
/// (ADR-0077): its d2 source
/// ([`crate::application::queue_reads::planning::graph_diagram`]), or the SVG the
/// host's d2 draws from it, and the tasks it shows.
pub fn graph_diagram(
    queue: &SqliteQueue,
    goal_id: Option<crate::domain::GoalId>,
    format: &str,
) -> Result<(String, Vec<TaskId>)> {
    let (source, tasks) = crate::application::queue_reads::planning::graph_diagram(queue, goal_id)?;
    let text = if format == "svg" {
        render_svg(&source)?
    } else {
        source
    };
    Ok((text, tasks))
}

/// The SVG the host's d2 on `PATH` draws from `source`.
fn render_svg(source: &str) -> Result<String> {
    crate::infrastructure::d2::render_svg(
        source,
        std::env::var_os("PATH").as_deref(),
        crate::infrastructure::d2::RENDER_TIMEOUT,
    )
}

/// A read of the queue (`list`, `events`, `stats`, `kpi`, `goal show`,
/// ...) as the command line prints it, and as the queue service answers
/// its read use case ([`QueueRead`], ADR-t1233-5 decision 1): both call
/// this, so a read gives the same JSON either way. The read itself is
/// [`crate::application::queue_reads::answer`]; this passes it the queue
/// and the host's reads. None calls cmux (ADR-t1433-1): `stats`'
/// `workspace_mismatch` is judged by the runs' background wrappers.
///
/// [`QueueRead`]: crate::application::queue_reads::QueueRead
pub fn read_queue(
    queue: &mut SqliteQueue,
    db: &Path,
    one_shot: &OneShot,
    read: &crate::application::queue_reads::QueueRead,
) -> Result<Value> {
    crate::application::queue_reads::answer(queue, &HostReads { db, one_shot }, read)
}

/// How the queue service answers a read use case: [`read_queue`] with the
/// user's `config.toml` for the language, as the command line reads it.
pub fn service_reads() -> crate::infrastructure::queue_service::ServiceReads {
    std::sync::Arc::new(|queue, db, read| {
        let one_shot = OneShot {
            user_config: crate::infrastructure::language::user_config_file(),
            ..OneShot::new(queue.generators().clone())
        };
        read_queue(queue, db, &one_shot, read)
    })
}

/// The host's side of a read of the queue at `db`.
struct HostReads<'a> {
    db: &'a Path,
    one_shot: &'a OneShot,
}

impl crate::application::queue_reads::PlanningSources<SqliteQueue> for HostReads<'_> {
    fn changes(&self, queue: &SqliteQueue) -> Result<Option<crate::domain::ChangeSet>> {
        task_changes(queue)
    }
    fn goal_tags(&self, queue: &SqliteQueue) -> Result<Option<crate::domain::TagSet>> {
        goal_tags(queue)
    }
    fn claim_holds(&self, queue: &SqliteQueue) -> Result<Vec<Value>> {
        health::claim_holds(queue, &SystemProcesses, queue.generators().clock.as_ref())
    }
    fn render_svg(&self, source: &str) -> Result<String> {
        render_svg(source)
    }
    fn raw_stdout(&self, text: String) -> Value {
        json!({ crate::view::RAW_STDOUT: text })
    }
    fn goal_view(&self, detail: &crate::domain::GoalDetail) -> Value {
        crate::view::goal_detail(detail)
    }
}

impl crate::application::queue_reads::ObservationSources<SqliteQueue> for HostReads<'_> {
    fn status(&self, queue: &SqliteQueue, role: Option<SessionRole>) -> Result<Value> {
        self.one_shot.status_of(self.db, queue, role)
    }
    fn stats(&self, queue: &SqliteQueue, query: &StatsQuery) -> Result<Value> {
        self.one_shot.stats_of(queue, self.db, query)
    }
    fn kpi(&self, queue: &SqliteQueue, query: &crate::domain::kpi::KpiQuery) -> Result<Value> {
        self.one_shot.kpi_of(queue, self.db, query)
    }
    fn forecast(
        &self,
        queue: &SqliteQueue,
        query: &crate::application::forecast::ForecastQuery,
    ) -> Result<Value> {
        self.one_shot.forecast_of(queue, self.db, query)
    }
    fn improvements(&self, queue: &SqliteQueue) -> Result<Value> {
        self.one_shot.improvements_of(queue)
    }
    fn observe_input(
        &self,
        read: &crate::application::queue_reads::ObserveInputRead,
    ) -> Result<Value> {
        crate::infrastructure::observer::read_input(
            self.db,
            &read.observation,
            read.section.as_deref(),
            read.offset,
            read.limit,
        )
    }
    fn clock<'q>(&self, queue: &'q SqliteQueue) -> &'q dyn crate::application::Clock {
        queue.generators().clock.as_ref()
    }
}

/// `observe`: one observation of the queue at `db`
/// ([`crate::application::observer::observe`]) with the repository's reads,
/// and the local host; no cmux (ADR-t1433-1). `signals` must belong to the
/// provider that starts it.
pub fn observe(
    db: &Path,
    provider: &dyn AgentProvider,
    signals: Option<&dyn crate::application::AgentSignals>,
    options: &crate::application::observer::ObserveOptions,
) -> Result<Value> {
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let mut queue = SqliteQueue::open(&db)?;
    let generators = queue.generators().clone();
    let sources = ObserverReads {
        one_shot: OneShot::new(generators.clone()),
    };
    crate::application::observer::observe(
        &mut queue,
        &db,
        provider,
        signals,
        options,
        &crate::application::observer::ObserverEnvironment {
            sources: &sources,
            host: &crate::infrastructure::observer::LocalObserver,
            generators: &generators,
        },
    )
}

/// The observer's reads that [`OneShot`] gives on the opened queue.
struct ObserverReads {
    one_shot: OneShot,
}

impl crate::application::observer::ObserverSources<SqliteQueue> for ObserverReads {
    fn stats(&self, queue: &SqliteQueue, db: &Path, query: &StatsQuery) -> Result<Value> {
        self.one_shot.stats_of(queue, db, query)
    }
    fn kpi(&self, queue: &SqliteQueue, db: &Path) -> Result<Value> {
        self.one_shot.observer_kpi(queue, db)
    }
    fn improvements(&self, queue: &SqliteQueue) -> Result<Value> {
        self.one_shot.improvements_of(queue)
    }
    fn checkout(&self, queue: &SqliteQueue) -> Result<Option<PathBuf>> {
        bound_checkout(queue)
    }
}

/// `throughput-review`: one review of the queue at `db`
/// ([`crate::application::throughput_review::review`]) with the
/// repository's reads and the local host, on `provider`, the provider of
/// its launch.
pub fn throughput_review(
    db: &Path,
    provider: &dyn AgentProvider,
    options: &crate::application::throughput_review::ReviewOptions,
) -> Result<Value> {
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let mut queue = SqliteQueue::open(&db)?;
    let generators = queue.generators().clone();
    let sources = ThroughputReviewReads {
        one_shot: OneShot::new(generators.clone()),
    };
    crate::application::throughput_review::review(
        &mut queue,
        &db,
        provider,
        options,
        &crate::application::throughput_review::ThroughputReviewEnvironment {
            sources: &sources,
            host: &crate::infrastructure::throughput_review::LocalThroughputReview,
            generators: &generators,
            // Claude Code's reading of the walls in a job's output, which
            // starts nothing (task 438, ADR-t1857-1).
            signals: Some(&crate::infrastructure::adapters::ClaudeCode {
                executable: PathBuf::from("claude"),
            }),
        },
    )
}

/// What the throughput review starts with when no supervisor routed it:
/// the launch of `[roles.throughput_review]` of the queue's bound
/// checkout's `dagq.toml` (the provider's default without one).
pub fn throughput_review_launch(db: &Path) -> Result<crate::domain::actor_model::ActorLaunch> {
    let queue = SqliteQueue::open(
        &db.canonicalize()
            .context("queue must already be initialized")?,
    )?;
    let checkout = bound_checkout(&queue)?;
    Ok(crate::infrastructure::throughput_review::review_launch(
        checkout.as_deref(),
    ))
}

/// What the observer starts with when no supervisor routed it (a command
/// typed by hand): `[roles.observer]` of the bound checkout's `dagq.toml`.
pub fn observer_launch(db: &Path) -> Result<crate::domain::actor_model::ActorLaunch> {
    let queue = SqliteQueue::open(
        &db.canonicalize()
            .context("queue must already be initialized")?,
    )?;
    let checkout = bound_checkout(&queue)?;
    Ok(crate::infrastructure::observer::observer_launch(
        checkout.as_deref(),
    ))
}

/// The throughput review's reads that [`OneShot`] gives on the opened
/// queue.
struct ThroughputReviewReads {
    one_shot: OneShot,
}

impl crate::application::throughput_review::ThroughputReviewSources<SqliteQueue>
    for ThroughputReviewReads
{
    fn stats(&self, queue: &SqliteQueue, db: &Path, query: &StatsQuery) -> Result<Value> {
        self.one_shot.stats_of(queue, db, query)
    }
    fn kpi(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        query: &crate::domain::kpi::KpiQuery,
    ) -> Result<Value> {
        self.one_shot.kpi_of(queue, db, query)
    }
    fn checkout(&self, queue: &SqliteQueue) -> Result<Option<PathBuf>> {
        bound_checkout(queue)
    }
    fn record_finding(
        &self,
        queue: &mut SqliteQueue,
        finding: crate::domain::NewFinding,
    ) -> Result<i64> {
        Ok(queue.record_finding(finding)?.finding.id.as_i64())
    }
}

/// `events --after`: the events after `after`, oldest first, in compact
/// form with no filter.
pub fn events(db: &Path, after: EventId, limit: usize, all: bool) -> Result<Value> {
    events_matching(
        db,
        &crate::application::watch::EventsQuery {
            after,
            limit,
            all,
            full: false,
            filter: EventFilter::default(),
        },
    )
}

/// `events` with its filters and `--full` on the queue at `db`.
pub fn events_matching(db: &Path, query: &crate::application::watch::EventsQuery) -> Result<Value> {
    crate::application::watch::events_in(&SqliteQueue::open_read_only(db)?, query)
}

/// `timeline RUN` on the queue at `db`.
pub fn timeline(db: &Path, run: &RunId, gap_secs: i64, full: bool) -> Result<Value> {
    timeline_in(&SqliteQueue::open_read_only(db)?, run, gap_secs, full)
}

/// [`timeline`] on a queue the caller already opened, so a command opens
/// it once.
pub fn timeline_in(queue: &SqliteQueue, run: &RunId, gap_secs: i64, full: bool) -> Result<Value> {
    crate::application::watch::timeline_in(
        queue,
        run,
        gap_secs,
        full,
        queue.generators().clock.as_ref(),
    )
}

/// `watch` on the queue at `db` ([`crate::application::watch::watch`]); a
/// `watch --role inbox` keeps its record in a file under the queue's
/// directory, never in the queue (ADR-t906-1). It notifies nobody: only
/// the command line's watch in the inbox's session runs cmux.
pub fn watch(db: &Path, options: &crate::application::watch::WatchOptions) -> Result<Value> {
    watch_in(db, &open_queue_watch(db)?, options, None)
}

/// `watch` using the live read-only queue already opened by the CLI.
/// Callers must use [`open_queue_watch`], never a migrated snapshot.
pub fn watch_in(
    db: &Path,
    queue: &SqliteQueue,
    options: &crate::application::watch::WatchOptions,
    cmux: Option<&Path>,
) -> Result<Value> {
    use crate::infrastructure::inbox_watchers;
    ensure!(
        queue.is_read_only()?,
        "watch requires a live read-only queue connection; use open_queue_watch"
    );
    let clock = queue.generators().clock.clone();
    let mut record = (options.role == Some(SessionRole::Inbox))
        .then(|| {
            let now = clock.system_time();
            inbox_watchers::WatcherFile::start(
                &inbox_watchers::dir(db),
                std::process::id(),
                clock.now(),
                now.duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as i64),
                options
                    .timeout
                    .map(|timeout| i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX / 4)),
                i64::try_from(options.interval.as_secs()).unwrap_or(i64::MAX / 4),
            )
            .inspect_err(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "the inbox watcher's record could not be written: {error:#}");
            })
            .ok()
        })
        .flatten();
    // The inbox's watch tells the person of a new ask through the cmux of
    // the inbox's session (ADR-t1433-1 decision 2); a cmux not found tells
    // nobody, and the watch goes on.
    let inbox_cmux = (options.role == Some(SessionRole::Inbox))
        .then(|| cmux.and_then(|cmux| executable(cmux).ok()))
        .flatten()
        .map(|executable| Cmux { executable });
    let notifier = inbox_cmux
        .as_ref()
        .map(|cmux| crate::application::watch::InboxNotifier {
            queue,
            backend: cmux,
            // A queue bound to no repository is named after the working
            // directory, as the other commands name it.
            checkout: crate::infrastructure::adapters::naming_checkout(
                queue
                    .repository_binding()
                    .ok()
                    .flatten()
                    .map(PathBuf::from)
                    .as_deref(),
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            ),
        });
    crate::application::watch::watch(
        queue,
        clock.as_ref(),
        &SystemProcesses,
        record
            .as_mut()
            .map(|record| record as &mut dyn crate::application::watch::WatchRecord),
        notifier
            .as_ref()
            .map(|notifier| notifier as &dyn crate::application::watch::AskNotifier),
        options,
    )
}

impl OneShot {
    /// `status --role`: see [`health::status`], measured to these
    /// generators' now.
    pub fn status_for(&self, db: &Path, role: Option<SessionRole>) -> Result<Value> {
        self.status_of(db, &self.open_read_only(db)?, role)
    }

    /// [`Self::status_for`] on a queue the caller already opened, so a
    /// command opens it once.
    /// For the inbox and a planner it adds the `language` their prompt
    /// and the `SessionStart` hook carry (ADR-t616-2). `release_update` is
    /// the release check of the host's `[update]` (ADR-t618-1).
    pub fn status_of(
        &self,
        db: &Path,
        queue: &SqliteQueue,
        role: Option<SessionRole>,
    ) -> Result<Value> {
        // A bound repository whose main checkout is not found reads as
        // unknown.
        let ci_watch = match bound_main_checkout(queue) {
            Ok(Some(Ok(checkout))) => crate::infrastructure::run_env::load_ci_watch(&checkout),
            Ok(Some(Err(error))) | Err(error) => Err(error),
            Ok(None) => Ok(None),
        };
        let (ci_named_jobs, ci_enabled) = crate::application::ci_watch::status_reading(&ci_watch);
        let mut status = health::status(
            queue,
            &SystemProcesses,
            &*self.generators.clock,
            role,
            ci_named_jobs,
        )?;
        add_sccache_diagnostics(queue, db, false, &mut status)?;
        let live_builds = crate::application::release_update::live_builds(&status["supervisors"]);
        status["release_update"] = crate::application::release_update::status(
            queue,
            &host_update(db).config,
            &live_builds,
        )?;
        status["ci"] = crate::application::ci_watch::status(queue, ci_enabled)?;
        let sections = health::status_sections(role);
        if sections.language {
            status["language"] = serde_json::to_value(self.language_report(queue)?)?;
        }
        if sections.inbox {
            status["inbox_guardrail"] = inbox_guardrail(queue)?;
            status["inbox_watcher"] = self.inbox_watcher(db);
            status["queue_service"] = queue_service_view(db, Some(queue));
        }
        Ok(status)
    }

    /// Whether the inbox has a watcher now (ADR-t906-1), from the records
    /// the inbox's watches left under the queue's directory and the
    /// processes under their pids (task 927).
    fn inbox_watcher(&self, db: &Path) -> Value {
        crate::application::inbox_watcher::judge_with(
            &crate::infrastructure::inbox_watchers::read(
                &crate::infrastructure::inbox_watchers::dir(db),
            ),
            &SystemProcesses,
            self.generators.clock.now(),
        )
        .to_json()
    }

    /// The language of the checkout `queue` is bound to over the user's
    /// (ADR-t616-2), with where it came from and any mistake.
    fn language_report(
        &self,
        queue: &SqliteQueue,
    ) -> Result<crate::infrastructure::language::LanguageReport> {
        Ok(crate::infrastructure::language::language_report(
            bound_checkout(queue)?.as_deref(),
            self.user_config.as_deref(),
        ))
    }

    /// `doctor`: see [`health::doctor`], with the queue's `schema` as
    /// `migrate --check` reports it. The schema is reported even for a
    /// queue that needs `migrate` or refuses this binary (ADR-0045 decision
    /// 5); a queue that refuses it has no `supervisors` or `runs`, and
    /// `error` says why. `common_dir`, when given, is the repository the
    /// queue must be bound to, checked either way.
    pub fn doctor(&self, db: &Path, full: bool, common_dir: Option<&str>) -> Result<Value> {
        let (schema, queue) = SqliteQueue::inspect_read_only(db)?;
        let mut service_view = None;
        let mut report = match queue {
            ReadOnlyQueue::Refused { binding, error } => {
                if let Some(common_dir) = common_dir {
                    binding.assert_repository(common_dir)?;
                }
                health::refused(self.generators.clock.now(), &error)
            }
            ReadOnlyQueue::Readable(queue) => {
                let queue = queue.with_generators(self.generators.clone());
                if let Some(common_dir) = common_dir {
                    queue.assert_repository(common_dir)?;
                }
                service_view = Some(queue_service_view(db, Some(&queue)));
                let run_env = doctor_run_env(&queue, db).map_err(|error| format!("{error:#}"));
                let mut report = health::doctor(
                    &queue,
                    &SystemProcesses,
                    &LocalRunFiles,
                    &*self.generators.clock,
                    full,
                    run_env,
                )?;
                if let Some(repository) = doctor_repository(&queue)? {
                    report["repository"] = repository;
                }
                add_sccache_diagnostics(&queue, db, true, &mut report)?;
                report["roles"] = doctor_roles(&queue);
                // The means the CI watch reads GitHub with (ADR-t1920-1).
                if let Some(ci_watch) = doctor_ci_watch(&queue)? {
                    report["ci_watch"] = ci_watch;
                }
                // The agents the landing branch's dagq.toml names and
                // their definitions (ADR-t1728-1).
                if let Some(agents) = doctor_agents(&queue)? {
                    report["agents"] = agents;
                }
                // Whether the inbox was opened with its guardrail settings
                // (ADR-t2159-1 decision 5).
                report["inbox_guardrail"] = inbox_guardrail(&queue)?;
                report["language"] = serde_json::to_value(self.language_report(&queue)?)?;
                report
            }
        };
        report["schema"] = serde_json::to_value(schema)?;
        // The inbox's watcher (ADR-t906-1).
        report["inbox_watcher"] = self.inbox_watcher(db);
        // The queue service (ADR-t1233-4): whether it answers, and its
        // attention from a queue that can be read.
        report["queue_service"] = service_view.unwrap_or_else(|| queue_service_view(db, None));
        // The host's `[update]`, with what was taken as its default
        // (ADR-t618-1 decision 3).
        report["release_update"] = serde_json::to_value(host_update(db))?;
        // What `graph --format svg` draws with (ADR-0077 decision 4).
        report["d2"] = serde_json::to_value(crate::infrastructure::d2::Tools::on(
            std::env::var_os("PATH").as_deref(),
        ))?;
        // The worker providers `claude` and `codex` resolve to on this PATH
        // and the modes this binary runs them in (ADR-t813-2); what each
        // supervisor fixed is on its entry in `supervisors`.
        let (claude, transcripts, codex) = (
            ClaudeCode {
                executable: PathBuf::from("claude"),
            },
            ClaudeTranscripts::from_env(),
            Codex::new(PathBuf::from("codex")),
        );
        report["providers"] = serde_json::to_value(provider_checks(
            Path::new("claude"),
            Path::new("codex"),
            &worker_adapters(&claude, &transcripts, Some(&codex)),
        ))?;
        Ok(report)
    }

    /// `stats`: see [`statistics::stats`], measured to these generators'
    /// now. The run directories are read from disk as Claude Code writes
    /// them, `[stall]` and `[conflicts]` from the `dagq.toml` of the main
    /// checkout of the repository the queue is bound to, main's history
    /// from that checkout, and the workspaces from
    /// `workspaces` (`None`: `workspace_mismatch` is not judged).
    pub fn stats(&self, db: &Path, query: &StatsQuery) -> Result<Value> {
        self.stats_of(&self.open_read_only(db)?, db, query)
    }

    /// [`Self::stats`] on `queue`, the queue at `db` the caller already
    /// opened, so a command opens it once.
    pub fn stats_of(&self, queue: &SqliteQueue, db: &Path, query: &StatsQuery) -> Result<Value> {
        let now = self.generators.clock.now();
        let checkout = bound_checkout(queue)?;
        let config_file = || match &checkout {
            Some(checkout) => load_stall_config(checkout),
            None => Ok(None),
        };
        let conflicts_file = || match &checkout {
            Some(checkout) => load_conflict_config(checkout),
            None => Ok(None),
        };
        let history = |since| match &checkout {
            Some(checkout) => GitRepository::inspect(checkout)?.main_history(since),
            None => anyhow::bail!("the queue is bound to no repository checkout"),
        };
        let signals = ClaudeCode {
            executable: PathBuf::from("claude"),
        };
        let host_dir = db
            .parent()
            .unwrap_or(Path::new("."))
            .join(crate::domain::host_metrics::HOST_DIR);
        let host_metrics =
            |from, until| crate::infrastructure::host_metrics::summary(&host_dir, from, until);
        let sources = StatsSources {
            files: &LocalRunFiles,
            signals: &signals,
            config_file: &config_file,
            conflicts_file: &conflicts_file,
            history: &history,
            host_metrics: Some(&host_metrics),
            areas: &area_reader(checkout.as_deref())?,
            utc_offset_secs: clock::local_utc_offset(now),
            dagq_source: checkout.as_deref().is_some_and(is_dagq_source),
        };
        Ok(serde_json::to_value(statistics::stats(
            queue,
            &SystemProcesses,
            now,
            query,
            &sources,
        )?)?)
    }

    /// `kpi` (ADR-0051 decision 9): see [`crate::domain::kpi::kpi`],
    /// measured to these generators' now in the host's time zone, judged by
    /// the `[kpi]` of the main checkout's `dagq.toml` with the host's
    /// `host.toml` (the queue's, over the host-wide one) over it.
    pub fn kpi_of(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        query: &crate::domain::kpi::KpiQuery,
    ) -> Result<Value> {
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let setup = report_setup(
            db,
            bound_checkout(queue)?.as_deref(),
            None,
            host_wide.as_deref(),
            now,
        )?;
        Ok(serde_json::to_value(crate::application::kpi::kpi(
            queue,
            now,
            setup.host,
            &setup.config,
            &setup.areas,
            query,
            setup
                .host_metrics
                .as_ref()
                .map(|read| crate::domain::kpi::HostReader(read.0.as_ref())),
        )?)?)
    }

    /// `forecast` (ADR-0070 decision 2): see
    /// [`crate::application::forecast::forecast`], at these generators' now,
    /// with the `min_samples` of the `[kpi]` settings `kpi` judges by.
    pub fn forecast_of(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        query: &crate::application::forecast::ForecastQuery,
    ) -> Result<Value> {
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let setup = report_setup(
            db,
            bound_checkout(queue)?.as_deref(),
            None,
            host_wide.as_deref(),
            now,
        )?;
        Ok(serde_json::to_value(
            crate::application::forecast::forecast(
                queue,
                &SystemProcesses,
                now,
                setup.config.min_samples,
                query,
            )?,
        )?)
    }

    /// What the observer reads of the KPIs (ADR-0051 decision 24): `kpi`
    /// of the last 7 days and the last 4 weeks as [`Self::kpi_of`] judges
    /// them, with the open breaches' events
    /// ([`crate::domain::kpi::observe::observer_input`]).
    pub fn observer_kpi(&self, queue: &SqliteQueue, db: &Path) -> Result<serde_json::Value> {
        use crate::domain::kpi::{KpiQuery, Period, observe};
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let setup = report_setup(
            db,
            bound_checkout(queue)?.as_deref(),
            None,
            host_wide.as_deref(),
            now,
        )?;
        let of = |period, last| {
            crate::application::kpi::kpi(
                queue,
                now,
                setup.host,
                &setup.config,
                &setup.areas,
                &KpiQuery {
                    period,
                    last,
                    ..KpiQuery::default()
                },
                None,
            )
        };
        let (day, week) = (of(Period::Day, 7)?, of(Period::Week, 4)?);
        Ok(observe::observer_input(
            &day,
            &week,
            &queue.kpi_breach_events_open()?,
        ))
    }

    /// [`crate::application::kpi::improvements`] against `[kpi]`'s
    /// `max_improvement_proposals` of the main checkout's `dagq.toml`.
    pub fn improvements_of(&self, queue: &SqliteQueue) -> Result<serde_json::Value> {
        let limit = match bound_checkout(queue)? {
            Some(checkout) => max_improvement_proposals(&checkout)?,
            None => crate::domain::kpi::config::DEFAULT_MAX_IMPROVEMENT_PROPOSALS,
        };
        crate::application::kpi::improvements(queue, limit)
    }

    /// `report` (ADR-0051 decision 21): the KPI report of the `period` that
    /// holds `at` (now without; today's and this week's are partial),
    /// made as the supervisor makes it and written under `out` (the
    /// queue's `reports/` without), or with `print` only returned.
    pub fn report_of(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        period: crate::domain::kpi::Period,
        at: Option<crate::domain::stats::Cursor>,
        out: Option<&Path>,
        print: bool,
    ) -> Result<Value> {
        use crate::application::report;
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let setup = report_setup(
            db,
            bound_checkout(queue)?.as_deref(),
            out,
            host_wide.as_deref(),
            now,
        )?;
        let made = report::make(queue, &setup, now, period, at)?;
        if print {
            return Ok(serde_json::to_value(made)?);
        }
        Ok(serde_json::to_value(report::write(
            &LocalRunFiles,
            &setup,
            &made,
            period,
            now,
        )?)?)
    }
}

/// The daily KPI reports' port of the supervisor (ADR-0051 decision 20):
/// made with [`report_setup`] of the queue at `db` and `checkout` at each
/// pass, its diagram drawn on `diagram_path` when given, and pushed as the
/// queue's `host.toml` (over `host_wide`) says.
pub(super) fn report_port(
    db: &Path,
    checkout: &Path,
    host_wide: Option<PathBuf>,
    diagram_path: Option<std::ffi::OsString>,
    push_retry: [Duration; 2],
) -> crate::application::supervise::ReportPort {
    let (db, checkout) = (db.to_path_buf(), checkout.to_path_buf());
    let name = checkout.file_name().map_or_else(
        || "dagq".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let push_target = crate::application::push::PushTarget {
        name,
        queue: db.display().to_string(),
    };
    let (setup_db, setup_wide) = (db.clone(), host_wide.clone());
    let queue_dir = db.parent().unwrap_or(Path::new(".")).to_path_buf();
    crate::application::supervise::ReportPort {
        utc_offset: clock::local_utc_offset,
        setup: Arc::new(move |now| {
            let mut setup =
                report_setup(&setup_db, Some(&checkout), None, setup_wide.as_deref(), now)?;
            if let Some(path) = &diagram_path {
                setup.diagram = d2_renderer(Some(path.clone()));
            }
            Ok(setup)
        }),
        push_config: Arc::new(move || {
            crate::infrastructure::push::load_host_push(&queue_dir, host_wide.as_deref())
        }),
        run_push: crate::infrastructure::push::run_push,
        push_retry,
        push_target,
    }
}

/// The forecast snapshots' port of the supervisor (ADR-0070 decision 3):
/// the `min_samples` of the `[kpi]` [`report_setup`] reads, looked for
/// every `check`.
pub(super) fn forecast_port(
    db: &Path,
    checkout: &Path,
    host_wide: Option<PathBuf>,
    check: Duration,
) -> crate::application::supervise::ForecastPort {
    let (db, checkout) = (db.to_path_buf(), checkout.to_path_buf());
    crate::application::supervise::ForecastPort {
        utc_offset: clock::local_utc_offset,
        min_samples: Arc::new(move |now| {
            report_setup(&db, Some(&checkout), None, host_wide.as_deref(), now)
                .map(|setup| setup.config.min_samples)
        }),
        check,
    }
}

/// The CI watch's port of the supervisor: `[ci_watch]` of `checkout`'s
/// `dagq.toml` read again each pass, its checks through `settings`'
/// `gh` in the main checkout (ADR-t1920-1).
pub(super) fn ci_watch_port(
    db: &Path,
    checkout: &Path,
    settings: &CiWatchOptions,
) -> crate::application::supervise::CiWatchPort {
    let file_checkout = checkout.to_path_buf();
    let file = Arc::new(move || crate::infrastructure::run_env::load_ci_watch(&file_checkout))
        as crate::application::supervise::CiWatchFile;
    let checkout = checkout.to_path_buf();
    let queue_dir = db.parent().unwrap_or(Path::new(".")).to_path_buf();
    let program = settings.program.clone();
    let source = Arc::new(
        move |config: &crate::domain::ci_watch::CiWatchConfig, branch: &str| {
            let remote = crate::infrastructure::run_env::load_repository_config(&checkout)?
                .remote()
                .to_owned();
            Ok(Arc::new(crate::infrastructure::ci_watch::GhSource::new(
                &program,
                std::env::var_os("PATH"),
                &checkout,
                &remote,
                config.clone(),
                branch,
                &queue_dir,
            ))
                as Arc<dyn crate::application::ci_watch::CiSource>)
        },
    ) as crate::application::supervise::CiSourceMaker;
    crate::application::supervise::CiWatchPort {
        file,
        source,
        interval: settings.interval,
    }
}
