//! The wiring of 観測と分析 (docs/design/architecture.md): `status`,
//! `doctor`, `stats`, `kpi`, `forecast`, `report`, the queue's reads
//! (`read_queue`, also the queue service's), `observe`,
//! `throughput-review`, `events`, `timeline`, `watch` and `ci failures`,
//! and the ports of the supervisor's reports, forecasts and CI watch. Each
//! entry builds the adapters and the settings it reads and calls the use
//! case; what is decided is in `application` and `domain`.

use super::{
    OneShot, bound_checkout, bound_main_checkout, goal_tags, open_queue_watch, provider_checks,
    queue_service_view, task_changes, task_title_prefixes, worker_adapters,
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
        AgentProvider, Verifier, health, lifecycle,
        stats::{self as statistics, StatsGitReads, StatsSources},
    },
    domain::{RunId, SessionRole, TaskId, stats::StatsQuery},
    infrastructure::{
        adapters::{ClaudeCode, GitRepository, SystemProcesses, VERIFICATION_TIMEOUT},
        clock,
        codex::Codex,
        run_env::{ShellVerifier, load_conflict_config, load_kpi_settings, load_stall_config},
        run_files::LocalRunFiles,
        sqlite::{ReadOnlyQueue, SqliteQueue},
        transcripts::ClaudeTranscripts,
    },
};

/// How the supervisor builds the ledger ([`SuperviseOptions::ledger`](super::SuperviseOptions::ledger)).
#[derive(Debug, Clone)]
pub struct LedgerOptions {
    /// The time between passes; tests shorten it.
    pub interval: Duration,
}

impl Default for LedgerOptions {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(crate::domain::ledger::builder::INTERVAL_SECS),
        }
    }
}

/// How the supervisor names the environment of its runs and measures
/// their resources ([`SuperviseOptions::environment`](super::SuperviseOptions::environment)).
#[derive(Debug, Clone)]
pub struct EnvironmentOptions {
    /// The time between the measures of the runs' resources; tests
    /// shorten it.
    pub interval: Duration,
}

impl Default for EnvironmentOptions {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(crate::application::supervise::RESOURCES_INTERVAL_SECS),
        }
    }
}

/// The supervisor's measure of the runs' resources in `environment`, in
/// the queue opened by `store`, with the processes it starts for its runs
/// in other trees (`children`).
pub(super) fn resources_port(
    settings: &EnvironmentOptions,
    environment: Arc<dyn crate::application::ExecutionEnvironment>,
    store: Arc<dyn crate::application::QueueOpener<dyn crate::application::ResourceQueue + Send>>,
    children: crate::domain::environment::RunChildren,
) -> crate::application::supervise::ResourcesPort {
    crate::application::supervise::ResourcesPort {
        interval: settings.interval,
        environment,
        store,
        children,
    }
}

/// The supervisor's builder of the ledger in the queue's stores (opened
/// by `store`), with the node rows kept as the host's samples are
/// (`retention_days`, none when they are not recorded), and the runs'
/// command steps classified by the rules that work for the `dagq.toml` of
/// `checkout` against `profiles`, their literals digested under the key of
/// the queue at `db` ([`ledger_classifier`]).
pub(super) fn ledger_port(
    settings: &LedgerOptions,
    retention_days: Option<u32>,
    store: Arc<dyn crate::application::QueueOpener<dyn crate::application::LedgerQueue + Send>>,
    (checkout, db, profiles): (
        &Path,
        &Path,
        &'static crate::domain::toolchain::ProfileRegistry,
    ),
) -> crate::application::supervise::LedgerPort {
    let (checkout, db) = (checkout.to_path_buf(), db.to_path_buf());
    crate::application::supervise::LedgerPort {
        interval: settings.interval,
        retention_days,
        utc_offset: clock::local_utc_offset,
        store,
        classifier: Arc::new(move || ledger_classifier(&checkout, &db, profiles)),
    }
}

/// The rules that classify the runs' command steps now
/// ([`crate::domain::command_rules::effective_rules`]): the `dagq.toml` of
/// `checkout` as the readers outside the landing read it (no profile and
/// no project rule when it cannot be read), the shapes' words matched
/// under the key of the queue at `db`; `None` without the key, whose steps
/// keep no shape.
fn ledger_classifier(
    checkout: &Path,
    db: &Path,
    profiles: &crate::domain::toolchain::ProfileRegistry,
) -> Option<crate::domain::command_rules::CommandClassifier> {
    use crate::application::TelemetrySinks;
    let key = crate::infrastructure::queue_service::SystemServiceAccess.digest_key(db)?;
    let toolchains = crate::infrastructure::run_env::toolchains_in_use(checkout, profiles);
    Some(crate::domain::command_rules::CommandClassifier::new(
        crate::domain::command_rules::effective_rules(&toolchains),
        crate::domain::command_shape::Subcommands::of(&toolchains).keyed(Some(key)),
    ))
}

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

/// The LaunchAgents of the queue at `location`, its own and any other
/// label's whose `--db` is its database, with what `launchd` has under
/// each ([`lifecycle::queue_agents`]).
pub fn queue_agents(
    location: &crate::infrastructure::location::QueueLocation,
    launchd: &dyn crate::application::LaunchAgent,
) -> Result<Vec<lifecycle::QueueAgent>> {
    lifecycle::queue_agents(
        launchd,
        &LocalRunFiles,
        &location.label,
        &location.launch_agent,
        &location.db,
    )
}

/// [`queue_agents`] of the queue at `db` under this user's launchd. One
/// that cannot be read is logged and counts as none: `doctor` and
/// `status` go on judging the registrations.
fn launchd_queue_agents(db: &Path) -> Vec<lifecycle::QueueAgent> {
    let launchd = crate::infrastructure::launchd::Launchctl {
        // SAFETY: getuid has no preconditions and cannot fail.
        uid: unsafe { libc::getuid() },
    };
    queue_agents(
        &crate::infrastructure::location::QueueLocation::explicit(db),
        &launchd,
    )
    .unwrap_or_else(|error| {
        tracing::warn!(error = %format_args!("{error:#}"), "the queue's LaunchAgents could not be read: {error:#}");
        Vec::new()
    })
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
        profiles: &crate::domain::toolchain::BUILTIN,
        children: crate::domain::environment::RunChildren::default(),
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
        profiles: &crate::domain::toolchain::BUILTIN,
        children: crate::domain::environment::RunChildren::default(),
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

/// `doctor`'s and `status`'s `toolchains` (ADR-t2268-1): the profiles the
/// `dagq.toml` of the bound main checkout declares, resolved against
/// `profiles`, with the paths each applies to (none while it declares
/// none), or `error` with the mistake; none for a queue bound to no
/// checkout.
fn bound_toolchains(
    queue: &SqliteQueue,
    profiles: &crate::domain::toolchain::ProfileRegistry,
) -> Result<Option<Value>> {
    let Some(checkout) = bound_checkout(queue)? else {
        return Ok(None);
    };
    Ok(Some(crate::domain::toolchain::view(
        crate::infrastructure::run_env::load_toolchains(&checkout, profiles)
            .map_err(|error| format!("{error:#}")),
    )))
}

/// `doctor`'s `command_rules`: the rules that classify the commands for
/// the `dagq.toml` of the bound main checkout, resolved against
/// `profiles`, each with where it comes from, which a run's work breakdown
/// classifies by too ([`crate::domain::command_rules::doctor_view`]); or
/// `error` with the file's mistake; none for a queue bound to no checkout.
fn bound_command_rules(
    queue: &SqliteQueue,
    profiles: &crate::domain::toolchain::ProfileRegistry,
) -> Result<Option<Value>> {
    let Some(checkout) = bound_checkout(queue)? else {
        return Ok(None);
    };
    Ok(Some(
        match crate::infrastructure::run_env::load_toolchains(&checkout, profiles) {
            Ok(toolchains) => crate::domain::command_rules::doctor_view(&toolchains),
            Err(error) => json!({"error": format!("{error:#}")}),
        },
    ))
}

/// `runtime` of `[roles.worker]` and `[roles.verification]` of the
/// `dagq.toml` of the bound main checkout (goal 38).
fn runtimes_of(queue: &SqliteQueue) -> Result<crate::domain::container_runtime::RuntimeConfig> {
    crate::infrastructure::run_env::BoundRuntimes::new(bound_checkout(queue)).read()
}

/// `doctor`'s `agents`: see [`crate::application::review::check_agents`],
/// read from the landing branch of the checkout `queue` is bound to: the
/// project's agents (`agents`), the built-ins (`builtins`: the whole
/// review `builtin:acceptance` first, then each built-in agent, each with
/// whether it runs and does by default, its globs and whether `dagq.toml`
/// wrote them, its provider and its rules file), the enabled built-ins
/// that judge without the project's rules because the landing branch has
/// no rules file for them (`without_rules`), and the mistakes (`errors`);
/// none for a queue bound to none, and `error` alone when the checkout or
/// `dagq.toml` cannot be read or parsed (an unknown `builtin:` name, a
/// project's agent under that prefix, an `enabled` that is no boolean, or
/// `builtin:acceptance` turned off or given `paths` or `provider` among
/// them), which also keeps every review from passing.
fn doctor_agents(queue: &SqliteQueue) -> Result<Option<serde_json::Value>> {
    let Some(checkout) = bound_main_checkout(queue)? else {
        return Ok(None);
    };
    let checked = checkout
        .and_then(|checkout| GitRepository::inspect(&checkout))
        .and_then(|repository| {
            crate::application::review::check_agents(&repository, &|text| {
                Ok(crate::infrastructure::run_env::parse_config(text)?.review_agents())
            })
        });
    Ok(Some(match checked {
        Ok(value) => value,
        Err(error) => serde_json::json!({"error": format!("{error:#}")}),
    }))
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

/// Whether the host's Rust toolchain is recorded and shown for the queue
/// bound to `checkout`, as the profiles of its `dagq.toml` resolved against
/// `profiles` say
/// ([`crate::domain::toolchain::Toolchains::records_rust_toolchain`]); a
/// queue bound to none records it not. `kpi`'s `toolchain` axis and
/// `stats`' `rustc` read it alike.
fn records_rust_toolchain(
    checkout: Option<&Path>,
    profiles: &crate::domain::toolchain::ProfileRegistry,
) -> bool {
    checkout.is_some_and(|checkout| {
        crate::infrastructure::run_env::toolchains_in_use(checkout, profiles)
            .records_rust_toolchain()
    })
}

/// Whether the rules that work for the `dagq.toml` of `checkout`, its
/// profiles resolved against `profiles`, count `integrate`'s checks
/// ([`crate::domain::command_rules::EffectiveRules::counts_checks`]): only
/// then does `stats` show the counts of what `integrate` repeats. None is
/// counted without a checkout, or with a `dagq.toml` that cannot be read.
fn counts_checks(
    checkout: Option<&Path>,
    profiles: &crate::domain::toolchain::ProfileRegistry,
) -> bool {
    checkout.is_some_and(|checkout| {
        crate::domain::command_rules::effective_rules(
            &crate::infrastructure::run_env::toolchains_in_use(checkout, profiles),
        )
        .counts_checks()
    })
}

/// What the KPIs and their reports of the queue at `db` are made with at
/// `now`: the `[kpi]` of `checkout`'s `dagq.toml` with the host's
/// `host.toml` (the queue's, over the host-wide one) over it, the host's
/// time zone and cores, the `[report]` retention of `host.toml`, the
/// reports' root (`out`, else `<queue dir>/reports/`), and the host's d2
/// and TALA on this process's PATH for the dependency diagram; and whether
/// the `toolchain` axis has strata, as the profiles of `checkout`'s
/// `dagq.toml` resolved against `profiles` say
/// ([`crate::domain::toolchain::Toolchains::records_rust_toolchain`]).
fn report_setup(
    db: &Path,
    checkout: Option<&Path>,
    out: Option<&Path>,
    host_wide: Option<&Path>,
    now: i64,
    profiles: &crate::domain::toolchain::ProfileRegistry,
) -> Result<crate::application::report::ReportSetup> {
    report_setup_with(
        db,
        checkout,
        out,
        host_wide,
        now,
        profiles,
        area_reader(checkout)?,
    )
}

/// [`report_setup`] with the landed runs' areas read by `areas`.
fn report_setup_with(
    db: &Path,
    checkout: Option<&Path>,
    out: Option<&Path>,
    host_wide: Option<&Path>,
    now: i64,
    profiles: &crate::domain::toolchain::ProfileRegistry,
    areas: crate::application::areas::AreaReader,
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
            rust_toolchain: records_rust_toolchain(checkout, profiles),
        },
        config: crate::domain::kpi::KpiConfig::merge(repository.as_ref(), host.as_ref()),
        keep: load_host_report(queue_dir, host_wide)?,
        build: crate::VERSION.to_owned(),
        diagram: d2_renderer(std::env::var_os("PATH")),
        areas,
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
        judged: None,
    })
}

/// What `stats` and `kpi` of a queue read in Git's place: the main
/// checkout whose `dagq.toml` they read, and what the queue service's
/// client read with Git there (none on the DB, which reads Git in the
/// checkout itself).
struct GitPart {
    checkout: Option<PathBuf>,
    served: Option<statistics::ServedReads>,
}

impl GitPart {
    /// On the DB (`git` none): the bound main checkout, found with Git. In
    /// the queue service: the one [`served_checkout`] finds with no Git
    /// run, never one the client names, and `git` as the service reads it
    /// with the inputs the queue's events give now
    /// ([`statistics::StatsGitReads::served`]).
    fn of(queue: &SqliteQueue, git: Option<&StatsGitReads>) -> Result<Self> {
        let Some(git) = git else {
            return Ok(Self {
                checkout: bound_checkout(queue)?,
                served: None,
            });
        };
        let checkout = served_checkout(queue)?;
        let inputs = statistics::stats_inputs(checkout.clone(), &queue.all_events()?);
        Ok(Self {
            served: Some(git.served(&inputs)?),
            checkout,
        })
    }

    /// The landed commits' areas: the client's, else read with Git.
    fn areas(&self) -> Result<crate::application::areas::AreaReader> {
        match &self.served {
            Some(served) => Ok(served.areas.clone()),
            None => area_reader(self.checkout.as_deref()),
        }
    }

    /// What refuses the events read when the client's reads are not of
    /// them; none on the DB.
    fn fresh(&self) -> Option<impl Fn(&[crate::domain::RunEvent]) -> Result<()> + '_> {
        self.served
            .as_ref()
            .map(|served| move |events: &[crate::domain::RunEvent]| served.check(events))
    }
}

/// The main checkout of the repository `queue` is bound to as the queue
/// service finds it, from the binding alone and with no Git run
/// ([`crate::infrastructure::adapters::main_checkout_from_files`]); none for
/// a queue bound to none.
fn served_checkout(queue: &SqliteQueue) -> Result<Option<PathBuf>> {
    Ok(queue.repository_binding()?.and_then(|common_dir| {
        crate::infrastructure::adapters::main_checkout_from_files(Path::new(&common_dir))
    }))
}

/// What the queue service answers `read`, a `stats` or `kpi` its client
/// sends, by `call`, sent with what the client read with the host's Git in
/// the main checkout the service's `stats_inputs` names
/// ([`crate::application::stats::call_read`]). The one place a client
/// reads Git for them.
pub fn call_stats_read(
    read: &crate::application::queue_reads::QueueRead,
    call: impl FnMut(&crate::application::queue_reads::QueueRead) -> Result<Value>,
) -> Result<Value> {
    statistics::call_read(
        read,
        &statistics::StatsClient {
            areas: &|checkout| area_reader(Some(checkout)),
            history: &|checkout, since| GitRepository::inspect(checkout)?.main_history(since),
        },
        call,
    )
}

/// Summarizes the host's load records under `dir` (task 872).
fn host_metrics_reader(dir: PathBuf) -> crate::application::report::HostMetrics {
    crate::application::report::HostMetrics(std::sync::Arc::new(move |from, until| {
        crate::infrastructure::host_metrics::summary(&dir, from, until)
    }))
}

/// `report`'s params as the queue service's client sends them
/// ([`crate::application::report::prepare_request`]), from what `call`
/// answers (the service, or `run_command` on the DB), with the host's Git
/// for the main checkout and the landed commits' changes, and the host's d2
/// and TALA for the diagram.
pub(super) fn prepare_report(
    params: &mut crate::application::queue_commands::ReportParams,
    call: impl FnMut(&crate::application::queue_commands::QueueCommand) -> Result<Value>,
) -> Result<()> {
    crate::application::report::prepare_request(
        params,
        &crate::application::report::ReportClient {
            main_checkout: &crate::infrastructure::adapters::main_checkout_of,
            areas: &|checkout| area_reader(Some(checkout)),
            diagram: &d2_renderer(std::env::var_os("PATH")),
        },
        call,
    )
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
    ci_failures_of(&SqliteQueue::open_read_only(db)?, task)
}

/// `ci failures [--task ID]` on `queue`, as the command line and the queue
/// service answer it.
fn ci_failures_of(queue: &SqliteQueue, task: Option<TaskId>) -> Result<Value> {
    let config = bound_ci_watch(queue)?;
    let branch = config.as_ref().and_then(|config| config.branch.clone());
    crate::application::ci_watch::known_failures(queue, config.as_ref(), branch.as_deref(), task)
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

/// `status`'s, `stats`' and `doctor`'s `recurring` ([`health::recurring`])
/// of the `dagq.toml` of the bound main checkout and the queue's history;
/// none for a queue bound to no checkout.
fn recurring_view(queue: &SqliteQueue) -> Result<Option<Value>> {
    recurring_view_of(queue, bound_checkout(queue)?.as_deref())
}

/// [`recurring_view`] of the `dagq.toml` of `checkout`, the bound main
/// checkout.
fn recurring_view_of(queue: &SqliteQueue, checkout: Option<&Path>) -> Result<Option<Value>> {
    let Some(checkout) = checkout else {
        return Ok(None);
    };
    health::recurring(
        crate::infrastructure::run_env::load_recurring(checkout),
        |items| queue.recurring_schedule(items),
    )
}

/// `stats` on the system clock: see [`OneShot::stats`].
pub fn stats(db: &Path, query: &StatsQuery) -> Result<Value> {
    OneShot::system().stats(db, query)
}

/// The text `graph --out` writes of the diagram's d2 `source`: the SVG
/// the host's d2 draws from it for `svg`, else the source.
pub fn graph_export_text(source: &str, format: &str) -> Result<String> {
    if format == "svg" {
        render_svg(source)
    } else {
        Ok(source.to_owned())
    }
}

/// `graph --format svg` of a served queue: `answer`, the service's
/// `graph --format d2`, drawn by the host's d2 as the command line draws it.
pub fn graph_svg_of(answer: Value) -> Result<Value> {
    let source = answer
        .get(crate::view::RAW_STDOUT)
        .and_then(Value::as_str)
        .context("the queue service's graph answer holds no d2 source")?;
    Ok(json!({ crate::view::RAW_STDOUT: render_svg(source)? }))
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
    crate::application::queue_reads::answer(
        queue,
        &HostReads {
            db,
            one_shot,
            served: false,
        },
        read,
    )
}

/// How the queue service answers a read use case: [`read_queue`] with the
/// user's `config.toml` for the language, as the command line reads it.
pub fn service_reads() -> crate::infrastructure::queue_service::ServiceReads {
    std::sync::Arc::new(|queue, db, read| {
        let one_shot = OneShot {
            user_config: crate::infrastructure::language::user_config_file(),
            ..OneShot::new(queue.generators().clone())
        };
        crate::application::queue_reads::answer(
            queue,
            &HostReads {
                db,
                one_shot: &one_shot,
                served: true,
            },
            read,
        )
    })
}

/// How the queue service shows a task without `--full`: as the command
/// line shows it.
pub fn service_detail() -> crate::infrastructure::queue_service::ServiceDetail {
    crate::view::task_detail
}

/// The host's side of a read of the queue at `db`; `served` in the queue
/// service, where no Git runs (ADR-t3102-3 decision 5): a `stats` or `kpi`
/// sent with nothing its client read with Git is read as if it read
/// nothing ([`StatsGitReads`]'s default).
struct HostReads<'a> {
    db: &'a Path,
    one_shot: &'a OneShot,
    served: bool,
}

impl HostReads<'_> {
    /// What `stats` and `kpi` (`what`) read in place of Git: what the
    /// client sent, and Git itself (`None`) on the DB. The service refuses
    /// one sent with none ([`statistics::NOT_READ`]), as it runs no Git.
    fn git<'g>(
        &self,
        what: &str,
        sent: Option<&'g StatsGitReads>,
    ) -> Result<Option<&'g StatsGitReads>> {
        anyhow::ensure!(
            sent.is_some() || !self.served,
            "{what} {}",
            statistics::NOT_READ
        );
        Ok(sent)
    }
}

impl crate::application::queue_reads::PlanningSources<SqliteQueue> for HostReads<'_> {
    fn changes(&self, queue: &SqliteQueue) -> Result<Option<crate::domain::ChangeSet>> {
        task_changes(queue)
    }
    fn title_prefixes(&self, queue: &SqliteQueue) -> Result<Option<crate::domain::TitlePrefixes>> {
        task_title_prefixes(queue)
    }
    fn related_docs(
        &self,
        queue: &SqliteQueue,
    ) -> Result<Vec<crate::domain::doc_hints::RelatedDocument>> {
        crate::compose::task_related_docs(queue)
    }
    fn goal_tags(&self, queue: &SqliteQueue) -> Result<Option<crate::domain::TagSet>> {
        goal_tags(queue)
    }
    fn claim_holds(&self, queue: &SqliteQueue) -> Result<Vec<Value>> {
        health::claim_holds(queue, &SystemProcesses, queue.generators().clock.as_ref())
    }
    fn live_supervisors(&self, queue: &SqliteQueue) -> Result<Vec<String>> {
        health::live_supervisors(queue, &SystemProcesses, queue.generators().clock.as_ref())
    }
    fn class_aging(
        &self,
        queue: &SqliteQueue,
    ) -> Result<crate::application::claim_view::AgingInputs> {
        crate::compose::class_aging_inputs(queue)
    }
    fn clock<'q>(&self, queue: &'q SqliteQueue) -> &'q dyn crate::application::Clock {
        queue.generators().clock.as_ref()
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
    fn stats(
        &self,
        queue: &SqliteQueue,
        query: &StatsQuery,
        git: Option<&StatsGitReads>,
    ) -> Result<Value> {
        self.one_shot
            .stats_read(queue, self.db, query, self.git("stats", git)?)
    }
    fn ledger_check(
        &self,
        queue: &SqliteQueue,
        query: &StatsQuery,
        git: Option<&StatsGitReads>,
    ) -> Result<Value> {
        self.one_shot
            .ledger_check_of(queue, self.db, query, self.git("stats", git)?)
    }
    fn stats_inputs(&self, queue: &SqliteQueue) -> Result<Value> {
        Ok(serde_json::to_value(statistics::stats_inputs(
            served_checkout(queue)?,
            &queue.all_events()?,
        ))?)
    }
    fn kpi(
        &self,
        queue: &SqliteQueue,
        query: &crate::domain::kpi::KpiQuery,
        git: Option<&StatsGitReads>,
    ) -> Result<Value> {
        self.one_shot
            .kpi_read(queue, self.db, query, self.git("kpi", git)?)
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
    fn plan_request(
        &self,
        queue: &SqliteQueue,
        id: crate::domain::RequestId,
    ) -> Result<crate::domain::plan_request::PlanRequest> {
        queue.request_by_id(id)
    }
    fn plan_requests(
        &self,
        queue: &SqliteQueue,
        all: bool,
    ) -> Result<Vec<crate::domain::plan_request::PlanRequest>> {
        queue.plan_requests(all)
    }
    fn replan_brief(
        &self,
        queue: &SqliteQueue,
        id: crate::domain::ReplanId,
    ) -> Result<crate::domain::replan_plan::ReplanBrief> {
        queue.replan_brief(id)
    }
    fn replan_operations(
        &self,
        queue: &SqliteQueue,
        all: bool,
    ) -> Result<Vec<crate::domain::task_replan::ReplanOperation>> {
        queue.replan_operations(all)
    }
    fn ci_failures(
        &self,
        queue: &SqliteQueue,
        read: &crate::application::queue_reads::CiFailuresRead,
    ) -> Result<Value> {
        ci_failures_of(queue, read.task.map(TaskId::new))
    }
    fn planners(
        &self,
        queue: &SqliteQueue,
        read: &crate::application::queue_reads::PlannersRead,
    ) -> Result<Value> {
        self.one_shot.planners_of(
            queue,
            self.db,
            &crate::infrastructure::adapters::BackgroundSessions,
            read.all,
        )
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
            pid: std::process::id(),
            telemetry: Some(crate::application::telemetry_tokens::TargetTelemetry {
                tokens: &crate::infrastructure::queue_service::SystemServiceAccess,
                db: &db,
                lease: None,
            }),
            steps: Some(crate::application::step_report::CommandSteps {
                files: &crate::infrastructure::run_files::LocalRunFiles,
                sinks: Arc::new(crate::infrastructure::queue_service::SystemServiceAccess),
            }),
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
            telemetry: Some(crate::application::telemetry_tokens::TargetTelemetry {
                tokens: &crate::infrastructure::queue_service::SystemServiceAccess,
                db: &db,
                lease: None,
            }),
            steps: Some(crate::application::step_report::CommandSteps {
                files: &crate::infrastructure::run_files::LocalRunFiles,
                sinks: Arc::new(crate::infrastructure::queue_service::SystemServiceAccess),
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
/// the command line's watch in the inbox's session pushes.
pub fn watch(db: &Path, options: &crate::application::watch::WatchOptions) -> Result<Value> {
    watch_in(db, &open_queue_watch(db)?, options, false)
}

/// `watch` using the live read-only queue already opened by the CLI.
/// Callers must use [`open_queue_watch`], never a migrated snapshot. With
/// `push`, the watch is given the host's `[push]` as it reads now, the
/// queue's `host.toml` over the host-wide one, and the push command;
/// [`crate::application::watch::push_notifier`] decides whether it tells
/// the person of the new asks (ADR-t2159-1 decision 4).
pub fn watch_in(
    db: &Path,
    queue: &SqliteQueue,
    options: &crate::application::watch::WatchOptions,
    push: bool,
) -> Result<Value> {
    ensure!(
        queue.is_read_only()?,
        "watch requires a live read-only queue connection; use open_queue_watch"
    );
    let clock = queue.generators().clock.clone();
    watch_from(
        db,
        db,
        &crate::application::watch::QueueWatch {
            queue,
            clock: clock.as_ref(),
            control: &SystemProcesses,
        },
        clock.as_ref(),
        options,
        push,
    )
}

/// [`watch_in`] through the queue service `client` of the queue at `db`,
/// which the caller named `named` (the path its push names; the push
/// names the repository after the binding each answer gives, wherever the
/// watch runs) (ADR-t3102-3 decision 2): each look is its
/// `watch_poll`, and a look the service does not answer ends the watch at
/// once with that failure, never with a timeout or no events, except that
/// an outage the supervisor recorded is returned once per role and waited
/// out by the next watch of the role. The watcher's record, the push and
/// which outage was returned stay on this side, outside the DB.
pub fn watch_through_service(
    db: &Path,
    named: &Path,
    client: &crate::infrastructure::queue_service::Client,
    options: &crate::application::watch::WatchOptions,
) -> Result<Value> {
    let clock = crate::infrastructure::clock::system().clock;
    watch_from(
        db,
        named,
        &ServiceWatch {
            client,
            told: crate::infrastructure::queue_service::OutagesTold::of(
                db.parent().unwrap_or(Path::new(".")),
            ),
        },
        clock.as_ref(),
        options,
        true,
    )
}

/// A watch's looks as the queue service's `watch_poll`, and the outages
/// of the service they meet.
struct ServiceWatch<'a> {
    client: &'a crate::infrastructure::queue_service::Client,
    told: crate::infrastructure::queue_service::OutagesTold,
}

impl crate::application::watch::OutageNotices for ServiceWatch<'_> {
    fn recorded(&self, error: &anyhow::Error) -> Option<i64> {
        error
            .downcast_ref::<crate::infrastructure::queue_service::ClientFailure>()
            .filter(|failure| failure.code == "unreachable")
            .and_then(|failure| failure.outage.as_ref())
            .map(|outage| outage.since)
    }

    fn told(&self, role: Option<SessionRole>, since: i64) -> bool {
        self.told
            .told(role.map_or("none", SessionRole::as_str), since)
    }

    fn tell(&self, role: Option<SessionRole>, since: i64) -> Result<()> {
        self.told
            .tell(role.map_or("none", SessionRole::as_str), since)
    }
}

impl crate::application::watch::WatchSource for ServiceWatch<'_> {
    fn look(
        &self,
        after: Option<crate::domain::EventId>,
        role: Option<SessionRole>,
        limit: std::time::Duration,
    ) -> Result<crate::application::watch::WatchLook> {
        let mut params = serde_json::json!({});
        if let Some(after) = after {
            params["after"] = serde_json::json!(after);
        }
        if let Some(role) = role {
            params["role"] = serde_json::json!(role.as_str());
        }
        let answer = self.client.call_within(
            crate::domain::queue_service::UseCase::WatchPoll,
            params,
            limit,
        )?;
        serde_json::from_value(answer).context("the queue service's answer to watch_poll")
    }

    fn outages(&self) -> Option<&dyn crate::application::watch::OutageNotices> {
        Some(self)
    }
}

/// A `watch` of the queue at `db` on `source`: the inbox's watcher's
/// record and, with `push`, its notifier of new asks, which names the
/// queue `named` and its repository after the binding of the look that
/// returned them.
fn watch_from(
    db: &Path,
    named: &Path,
    source: &dyn crate::application::watch::WatchSource,
    clock: &dyn crate::application::Clock,
    options: &crate::application::watch::WatchOptions,
    push: bool,
) -> Result<Value> {
    use crate::infrastructure::inbox_watchers;
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
    let notifier = push
        .then(|| {
            crate::application::watch::push_notifier(
                options.role,
                || {
                    crate::infrastructure::push::load_host_push(
                        db.parent().unwrap_or(Path::new(".")),
                        crate::infrastructure::kpi_config::host_wide_file().as_deref(),
                    )
                },
                |binding| push_target(named, binding.map(Path::new)),
                crate::infrastructure::push::run_push,
            )
        })
        .flatten();
    crate::application::watch::watch_through(
        source,
        clock,
        record
            .as_mut()
            .map(|record| record as &mut dyn crate::application::watch::WatchRecord),
        notifier
            .as_ref()
            .map(|notifier| notifier as &dyn crate::application::watch::AskNotifier),
        options,
    )
}

/// The queue at `db`, bound to `binding`, as the inbox's ask notification
/// names it: after its repository, or the working directory for a queue bound to none, as the
/// other commands name it.
fn push_target(db: &Path, binding: Option<&Path>) -> crate::application::push::PushTarget {
    let checkout = crate::infrastructure::adapters::naming_checkout(
        binding,
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    );
    crate::application::push::PushTarget {
        name: crate::application::naming::repository_name(&checkout),
        queue: db.display().to_string(),
    }
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
    /// the release check of the host's `[update]` (ADR-t618-1). `held` is
    /// the open holds of tasks, and the overdue ones join `attention`
    /// ([`health::add_held_tasks`]).
    pub fn status_of(
        &self,
        db: &Path,
        queue: &SqliteQueue,
        role: Option<SessionRole>,
    ) -> Result<Value> {
        self.status_with_agents(db, queue, role, || launchd_queue_agents(db))
    }

    /// [`Self::status_of`] judging what `agents` gives as the queue's
    /// LaunchAgents ([`queue_agents`]) for `duplicate_supervisors`
    /// ([`health::duplicate_supervisors`]); tests give a fake launchd's.
    pub fn status_with_agents(
        &self,
        db: &Path,
        queue: &SqliteQueue,
        role: Option<SessionRole>,
        agents: impl FnOnce() -> Vec<lifecycle::QueueAgent>,
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
        health::add_duplicate_supervisors(
            &mut status,
            &queue.supervisors()?,
            &agents(),
            self.generators.clock.now(),
            &SystemProcesses,
            health::for_role(role),
        )?;
        add_sccache_diagnostics(queue, db, false, &mut status)?;
        health::add_runtimes(&mut status, queue, &SystemProcesses, runtimes_of(queue))?;
        let live_builds = crate::application::release_update::live_builds(&status["supervisors"]);
        status["release_update"] = crate::application::release_update::status(
            queue,
            &host_update(db).config,
            &live_builds,
        )?;
        status["ci"] = crate::application::ci_watch::status(queue, ci_enabled)?;
        if let Some(toolchains) = bound_toolchains(queue, self.profiles)? {
            status["toolchains"] = toolchains;
        }
        status["replans"] = health::replans(&queue.open_replans()?);
        if let Some(recurring) = recurring_view(queue)? {
            status["recurring"] = recurring;
        }
        health::add_held_tasks(
            &mut status,
            &queue.held_tasks()?,
            &*self.generators.clock,
            role,
        )?;
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
        self.doctor_with_agents(db, full, common_dir, || launchd_queue_agents(db))
    }

    /// [`Self::doctor`] judging what `agents` gives as the queue's
    /// LaunchAgents ([`queue_agents`]) for `duplicate_supervisors`
    /// ([`health::duplicate_supervisors`]), read only for a queue that
    /// opens; tests give a fake launchd's.
    pub fn doctor_with_agents(
        &self,
        db: &Path,
        full: bool,
        common_dir: Option<&str>,
        agents: impl FnOnce() -> Vec<lifecycle::QueueAgent>,
    ) -> Result<Value> {
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
                // Two supervisors serving the queue at once.
                health::add_duplicate_supervisors(
                    &mut report,
                    &queue.supervisors()?,
                    &agents(),
                    self.generators.clock.now(),
                    &SystemProcesses,
                    false,
                )?;
                if let Some(repository) = doctor_repository(&queue)? {
                    report["repository"] = repository;
                }
                add_sccache_diagnostics(&queue, db, true, &mut report)?;
                report["roles"] = doctor_roles(&queue);
                report["replans"] = health::replans(&queue.open_replans()?);
                if let Some(toolchains) = bound_toolchains(&queue, self.profiles)? {
                    report["toolchains"] = toolchains;
                }
                if let Some(rules) = bound_command_rules(&queue, self.profiles)? {
                    report["command_rules"] = rules;
                }
                health::add_runtimes(&mut report, &queue, &SystemProcesses, runtimes_of(&queue))?;
                // The recurring drafts and a mistake of their tables
                // (ADR-0072).
                if let Some(recurring) = recurring_view(&queue)? {
                    report["recurring"] = recurring;
                }
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
        // The Claude credential the queue broker sends a container's
        // worker with, chosen in host.toml (ADR-t2177-1): never its value.
        report["claude_credential"] = crate::infrastructure::claude_credential::report(
            db.parent().unwrap_or(Path::new(".")),
            crate::infrastructure::kpi_config::host_wide_file().as_deref(),
        );
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
        self.stats_read(queue, db, query, None)
    }

    /// [`Self::stats_of`] with what the queue service's client read with
    /// Git in place of Git (`git`, ADR-t3102-3 decision 5); `None` reads Git
    /// in the bound main checkout.
    pub fn stats_read(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        query: &StatsQuery,
        git: Option<&StatsGitReads>,
    ) -> Result<Value> {
        let part = GitPart::of(queue, git)?;
        let mut stats = self.with_stats_sources(db, &part, |now, sources| {
            Ok(serde_json::to_value(statistics::stats(
                queue,
                &SystemProcesses,
                now,
                query,
                sources,
            )?)?)
        })?;
        // When each recurring draft is registered next (ADR-0072).
        if let Some(recurring) = recurring_view_of(queue, part.checkout.as_deref())? {
            stats["recurring"] = recurring;
        }
        Ok(stats)
    }

    /// `stats --ledger` on `queue`, the queue at `db`: see
    /// [`crate::application::ledger_check::ledger_check`], with `stats`'
    /// sources ([`Self::stats_read`]'s, `git` as it reads it) and today's
    /// CSV files of the host's load under the queue's `host/`.
    pub fn ledger_check_of(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        query: &StatsQuery,
        git: Option<&StatsGitReads>,
    ) -> Result<Value> {
        let part = GitPart::of(queue, git)?;
        let host_dir = db
            .parent()
            .unwrap_or(Path::new("."))
            .join(crate::domain::host_metrics::HOST_DIR);
        let csv = |from, until| crate::infrastructure::host_metrics::read(&host_dir, from, until);
        self.with_stats_sources(db, &part, |now, sources| {
            Ok(serde_json::to_value(
                crate::application::ledger_check::ledger_check(
                    queue,
                    &SystemProcesses,
                    now,
                    query,
                    sources,
                    &csv,
                )?,
            )?)
        })
    }

    /// Run `read` with `stats`' sources of the queue at `db`, the
    /// `dagq.toml` of `part`'s main checkout and these generators' now:
    /// main's history and the landed commits' areas as its client read
    /// them, or read with Git in that checkout on the DB.
    fn with_stats_sources(
        &self,
        db: &Path,
        part: &GitPart,
        read: impl FnOnce(i64, &StatsSources<'_>) -> Result<Value>,
    ) -> Result<Value> {
        let now = self.generators.clock.now();
        let checkout = part.checkout.as_deref();
        let config_file = || match checkout {
            Some(checkout) => load_stall_config(checkout),
            None => Ok(None),
        };
        let conflicts_file = || match checkout {
            Some(checkout) => load_conflict_config(checkout),
            None => Ok(None),
        };
        let history = |since| match (&part.served, checkout) {
            (Some(served), _) => served.history(since),
            (None, Some(checkout)) => GitRepository::inspect(checkout)?.main_history(since),
            (None, None) => anyhow::bail!(statistics::NO_CHECKOUT),
        };
        let areas = part.areas()?;
        let fresh = part.fresh();
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
            areas: &areas,
            fresh: fresh
                .as_ref()
                .map(|fresh| fresh as &statistics::EventsCheck<'_>),
            utc_offset_secs: clock::local_utc_offset(now),
            counts_checks: counts_checks(checkout, self.profiles),
            rust_toolchain: records_rust_toolchain(checkout, self.profiles),
        };
        read(now, &sources)
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
        self.kpi_read(queue, db, query, None)
    }

    /// [`Self::kpi_of`] with the landed commits' areas the queue service's
    /// client read with Git (`git`, ADR-t3102-3 decision 5) in the main
    /// checkout the service finds with no Git run; `None` reads Git in the
    /// bound main checkout.
    pub fn kpi_read(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        query: &crate::domain::kpi::KpiQuery,
        git: Option<&StatsGitReads>,
    ) -> Result<Value> {
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let part = GitPart::of(queue, git)?;
        let setup = report_setup_with(
            db,
            part.checkout.as_deref(),
            None,
            host_wide.as_deref(),
            now,
            self.profiles,
            part.areas()?,
        )?;
        let fresh = part.fresh();
        Ok(serde_json::to_value(crate::application::kpi::kpi_checked(
            queue,
            now,
            setup.host,
            &setup.config,
            &setup.areas,
            query,
            setup.host_reader(),
            fresh
                .as_ref()
                .map(|fresh| fresh as &statistics::EventsCheck<'_>),
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
            self.profiles,
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
            self.profiles,
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
                setup.host_reader(),
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

    /// `report_inputs` ([`crate::application::report::report_inputs`]) of
    /// `queue`, with the `[kpi]` settings of the checkout it names and the
    /// host's.
    pub fn report_inputs_of(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        params: &crate::application::queue_commands::ReportInputsParams,
    ) -> Result<Value> {
        use crate::application::report;
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let settings = |checkout: &Path| {
            let setup = report_setup_with(
                db,
                Some(checkout),
                None,
                host_wide.as_deref(),
                now,
                self.profiles,
                crate::application::areas::AreaReader::none(),
            )?;
            Ok((setup.host.utc_offset_secs, setup.config))
        };
        Ok(serde_json::to_value(report::report_inputs(
            params,
            queue.repository_binding()?,
            || report::diagram_draft(queue),
            &report::ReportInputsSources {
                files: &LocalRunFiles,
                settings: &settings,
                events: &|| queue.all_events(),
            },
            now,
        )?)?)
    }

    /// `report` (ADR-0051 decision 21): the KPI report of the `period` that
    /// holds `at` (now without; today's and this week's are partial),
    /// made as the supervisor makes it with what its caller read and drew
    /// ([`prepare_report`]: the near-term diagram, the main checkout and
    /// the landed commits' areas) and written under `out` (the queue's
    /// `reports/` without), or with `print` only returned. No Git or
    /// program runs: a checkout that is not the bound repository's is
    /// refused.
    pub fn report_of(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        params: &crate::application::queue_commands::ReportParams,
    ) -> Result<Value> {
        use crate::application::report;
        let period = params.period.parse().map_err(anyhow::Error::msg)?;
        let at = params
            .at
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(anyhow::Error::msg)?;
        let diagram = params.diagram.clone().context(
            "report is sent with what its caller read and drew from report_inputs' answer",
        )?;
        let checkout = params.checkout.as_deref();
        if let Some(checkout) = checkout {
            report::accept_checkout(
                &LocalRunFiles,
                checkout,
                queue.repository_binding()?.as_deref(),
            )?;
        }
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let setup = report_setup_with(
            db,
            checkout,
            params.out.as_deref(),
            host_wide.as_deref(),
            now,
            self.profiles,
            crate::application::areas::AreaReader::judged(
                params
                    .areas
                    .clone()
                    .map(|areas| areas.into_iter().collect()),
            ),
        )?;
        let made = report::make_with(queue, &setup, now, period, at, diagram.into())?;
        if params.print {
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
/// queue's `host.toml` (over `host_wide`) says; the `toolchain` axis by
/// the profiles of `profiles` the `dagq.toml` declares.
pub(super) fn report_port(
    db: &Path,
    checkout: &Path,
    host_wide: Option<PathBuf>,
    diagram_path: Option<std::ffi::OsString>,
    push_retry: [Duration; 2],
    profiles: &'static crate::domain::toolchain::ProfileRegistry,
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
            let mut setup = report_setup(
                &setup_db,
                Some(&checkout),
                None,
                setup_wide.as_deref(),
                now,
                profiles,
            )?;
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
            report_setup(
                &db,
                Some(&checkout),
                None,
                host_wide.as_deref(),
                now,
                &crate::domain::toolchain::BUILTIN,
            )
            .map(|setup| setup.config.min_samples)
        }),
        check,
    }
}

/// The CI watch's port of the supervisor: `[ci_watch]` of `checkout`'s
/// `dagq.toml` read again each pass, its checks through `settings`'
/// `gh` in the main checkout (ADR-t1920-1), each naming the JUnit's tests
/// as the profiles the `dagq.toml` declares then do, resolved against
/// `profiles` ([`crate::infrastructure::run_env::toolchains_in_use`]).
pub(super) fn ci_watch_port(
    db: &Path,
    checkout: &Path,
    settings: &CiWatchOptions,
    profiles: &'static crate::domain::toolchain::ProfileRegistry,
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
            Ok(Arc::new(
                crate::infrastructure::ci_watch::GhSource::new(
                    &program,
                    std::env::var_os("PATH"),
                    &checkout,
                    &remote,
                    config.clone(),
                    branch,
                    &queue_dir,
                )
                .with_junit_test_name(
                    crate::infrastructure::run_env::toolchains_in_use(&checkout, profiles)
                        .junit_test_name(),
                ),
            )
                as Arc<dyn crate::application::ci_watch::CiSource>)
        },
    ) as crate::application::supervise::CiSourceMaker;
    crate::application::supervise::CiWatchPort {
        file,
        source,
        interval: settings.interval,
    }
}

#[cfg(test)]
mod tests {
    use super::{counts_checks, records_rust_toolchain};
    use crate::domain::toolchain::fake::REGISTRY;

    /// `stats`' `rustc` and the KPIs' `toolchain` axis are shown with the
    /// Rust profile declared, dagq's source or not, and not with only
    /// another profile declared or none, even in dagq's source
    /// (ADR-t2268-1 decisions 2 and 3); and not for a queue bound to no
    /// checkout.
    #[test]
    fn the_rust_toolchain_is_shown_as_the_declared_profiles_say() {
        let shown = |package: &str, toml: Option<&str>| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("Cargo.toml"),
                format!("[package]\nname = \"{package}\"\n"),
            )
            .unwrap();
            if let Some(toml) = toml {
                std::fs::write(dir.path().join("dagq.toml"), toml).unwrap();
            }
            records_rust_toolchain(Some(dir.path()), &REGISTRY)
        };
        let rust = "[toolchains.core]\nprofile = \"rust\"\npaths = [\"src/**\"]\n";
        let fake = "[toolchains.tool]\nprofile = \"fake\"\n";
        assert!(shown("other", Some(rust)));
        assert!(shown("dagq", Some(rust)));
        assert!(!shown("dagq", Some(fake)));
        assert!(!shown("other", Some(fake)));
        assert!(!shown("dagq", None));
        assert!(!shown("other", None));
        assert!(!shown(
            "dagq",
            Some("[verify_marks.within]\nclass = \"timeout\"\nline = \"x {test} y\"\n")
        ));
        assert!(!records_rust_toolchain(None, &REGISTRY));
    }

    /// `stats`' counts of what `integrate` repeats are shown where the
    /// rules that work count its checks: with the Rust profile declared or
    /// a project rule that gives one of their classes, dagq's source or
    /// not; not with only a profile whose rules give none, or nothing
    /// declared, even in dagq's source; and not for a queue bound to no
    /// checkout or with a `dagq.toml` that cannot be read.
    #[test]
    fn the_counts_of_the_checks_are_shown_as_the_rules_that_work_say() {
        let shown = |package: &str, toml: Option<&str>| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("Cargo.toml"),
                format!("[package]\nname = \"{package}\"\n"),
            )
            .unwrap();
            if let Some(toml) = toml {
                std::fs::write(dir.path().join("dagq.toml"), toml).unwrap();
            }
            counts_checks(Some(dir.path()), &REGISTRY)
        };
        let rust = "[toolchains.core]\nprofile = \"rust\"\n";
        let fake = "[toolchains.tool]\nprofile = \"fake\"\n";
        let project = "[[telemetry.commands]]\nclass = \"e2e\"\nprogram = \"make\"\n";
        for package in ["dagq", "other"] {
            assert!(shown(package, Some(rust)), "{package}");
            assert!(shown(package, Some(project)), "{package}");
            assert!(!shown(package, Some(fake)), "{package}");
            assert!(!shown(package, None), "{package}");
            assert!(!shown(package, Some("[toolchains\n")), "{package}");
        }
        assert!(!counts_checks(None, &REGISTRY));
    }
}
