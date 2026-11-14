//! The wiring of 観測と分析 (docs/design/architecture.md): `status`,
//! `doctor`, `stats`, `kpi`, `forecast`, `report`, the queue's reads
//! (`read_queue`, also the queue service's), `observe`,
//! `throughput-review`, `events`, `timeline`, `watch` and `ci failures`,
//! and the ports of the supervisor's reports, forecasts and CI watch. Each
//! entry builds the adapters and the settings it reads and calls the use
//! case; what is decided is in `application` and `domain`.
//!
//! `stats` (`--ledger` too), `stats_inputs`, `kpi` (`--compare` too, and
//! the observer's KPI input), `forecast` and `report` (`report_inputs`
//! too) are stopped: each refuses before it reads, as it reads every event
//! of the queue ([`crate::domain::stopped_reads`], ADR-t3745-1). The wiring
//! behind them stays, reading [`refuse`] where it read every event, for the
//! ledger's readers to take.

use super::{
    OneShot, bound_checkout, bound_main_checkout, goal_tags, provider_checks, queue_broker_view,
    queue_service_view, task_changes, task_title_prefixes, worker_adapters,
};
use crate::domain::EventKind;
use crate::infrastructure::{sql_exec::TransactionEntries, store_queue::StoreQueue};
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
        supervisor_service::{ServiceTokens, SupervisorQueues},
    },
    domain::{
        RunId, SessionRole, TaskId,
        stats::StatsQuery,
        stopped_reads::{StoppedRead, refuse},
    },
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

/// Whether the host's inbox was opened with its guardrail, by the newest
/// `inbox_opened` of no person's inbox and `inbox_guardrail_reached`
/// ([`crate::application::inbox_guardrail::judge_latest`]).
fn inbox_guardrail<E: TransactionEntries>(queue: &StoreQueue<E>) -> Result<Value>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    let opened = queue.latest_inbox_opened(None)?;
    let reached = queue.latest_inbox_event(EventKind::InboxGuardrailReached, None)?;
    Ok(crate::application::inbox_guardrail::judge_latest(
        opened.as_ref(),
        reached.as_ref(),
    ))
}

/// `status`: see [`status_for`], with all of the attention.
#[cfg(feature = "test-paths")]
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
#[cfg(feature = "test-paths")]
pub fn status_for(db: &Path, role: Option<SessionRole>) -> Result<Value> {
    OneShot::system().status_for(db, role)
}

/// `doctor` on the system clock: see [`OneShot::doctor`].
#[cfg(feature = "test-paths")]
pub fn doctor(db: &Path, full: bool) -> Result<Value> {
    OneShot::system().doctor(db, full, None)
}

/// What `doctor`'s command adds to the service's report of the host it
/// runs on, on its own PATH: what `graph --format svg` draws with
/// (ADR-0077 decision 4), and the worker providers `claude` and `codex`
/// resolve to and the modes this binary runs them in (ADR-t813-2); what
/// each supervisor fixed is on its entry in `supervisors`.
pub fn doctor_on_this_host(report: &mut Value) -> Result<()> {
    report["d2"] = serde_json::to_value(crate::infrastructure::d2::Tools::on(
        std::env::var_os("PATH").as_deref(),
    ))?;
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
    Ok(())
}

/// How the queue's service makes `doctor`'s report
/// ([`OneShot::doctor_of`] on the system clock, with this user's
/// LaunchAgents of the queue).
pub fn service_doctor<E: TransactionEntries + 'static>()
-> crate::infrastructure::queue_service::ServiceDoctor<StoreQueue<E>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    Arc::new(|db, schema, queue, full| {
        OneShot {
            user_config: crate::infrastructure::language::user_config_file(),
            ..OneShot::system()
        }
        .doctor_of(db, schema, queue, full, || launchd_queue_agents(db))
    })
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

fn add_sccache_diagnostics<E: TransactionEntries>(
    queue: &StoreQueue<E>,
    db: &Path,
    stats: bool,
    output: &mut Value,
) -> Result<()>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
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
fn doctor_run_env<E: TransactionEntries>(
    queue: &StoreQueue<E>,
    db: &Path,
) -> Result<crate::domain::run_env::RunEnvCheck>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
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
fn doctor_roles<E: TransactionEntries>(queue: &StoreQueue<E>) -> Value
where
    StoreQueue<E>: crate::application::SearchIndex,
{
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
fn bound_toolchains<E: TransactionEntries>(
    queue: &StoreQueue<E>,
    profiles: &crate::domain::toolchain::ProfileRegistry,
) -> Result<Option<Value>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
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
fn bound_command_rules<E: TransactionEntries>(
    queue: &StoreQueue<E>,
    profiles: &crate::domain::toolchain::ProfileRegistry,
) -> Result<Option<Value>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
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
fn runtimes_of<E: TransactionEntries>(
    queue: &StoreQueue<E>,
) -> Result<crate::domain::container_runtime::RuntimeConfig>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
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
fn doctor_agents<E: TransactionEntries>(queue: &StoreQueue<E>) -> Result<Option<serde_json::Value>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
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
fn doctor_repository<E: TransactionEntries>(
    queue: &StoreQueue<E>,
) -> Result<Option<serde_json::Value>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
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
/// reports' root (`<queue dir>/reports/`), and the host's d2
/// and TALA on this process's PATH for the dependency diagram; and whether
/// the `toolchain` axis has strata, as the profiles of `checkout`'s
/// `dagq.toml` resolved against `profiles` say
/// ([`crate::domain::toolchain::Toolchains::records_rust_toolchain`]).
fn report_setup(
    db: &Path,
    checkout: Option<&Path>,
    host_wide: Option<&Path>,
    now: i64,
    profiles: &crate::domain::toolchain::ProfileRegistry,
) -> Result<crate::application::report::ReportSetup> {
    use crate::infrastructure::{kpi_config::load_host_kpi, report_config::load_host_report};
    let repository = match checkout {
        Some(checkout) => load_kpi_settings(checkout)?,
        None => None,
    };
    let queue_dir = db.parent().unwrap_or(Path::new("."));
    let host = load_host_kpi(queue_dir, host_wide)?;
    Ok(crate::application::report::ReportSetup {
        root: queue_dir.join(REPORTS_DIR),
        host: crate::application::kpi::Host {
            utc_offset_secs: clock::local_utc_offset(now),
            cores: clock::logical_cores(),
            rust_toolchain: records_rust_toolchain(checkout, profiles),
        },
        config: crate::domain::kpi::KpiConfig::merge(repository.as_ref(), host.as_ref()),
        keep: load_host_report(queue_dir, host_wide)?,
        build: crate::VERSION.to_owned(),
        diagram: d2_renderer(std::env::var_os("PATH")),
        areas: area_map(checkout)?,
    })
}

/// The `[areas]` of `checkout`'s `dagq.toml` (ADR-t980-1), a setting the
/// landed runs' areas are judged by from main's history the ledger holds;
/// none without a checkout or `[areas]`. No Git runs.
fn area_map(checkout: Option<&Path>) -> Result<Option<crate::domain::areas::AreaMap>> {
    match checkout {
        Some(checkout) => crate::infrastructure::run_env::load_area_map(checkout),
        None => Ok(None),
    }
}

/// The main checkout whose `dagq.toml` `stats` and `kpi` read: in the
/// queue service (`served`) the one [`served_checkout`] finds with no Git
/// run, on the DB the bound one.
fn stats_checkout<E: TransactionEntries>(
    queue: &StoreQueue<E>,
    served: bool,
) -> Result<Option<PathBuf>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    if served {
        served_checkout(queue)
    } else {
        bound_checkout(queue)
    }
}

/// The main checkout of the repository `queue` is bound to as the queue
/// service finds it, from the binding alone and with no Git run
/// ([`crate::infrastructure::adapters::main_checkout_from_files`]); none for
/// a queue bound to none.
fn served_checkout<E: TransactionEntries>(queue: &StoreQueue<E>) -> Result<Option<PathBuf>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    Ok(queue.repository_binding()?.and_then(|common_dir| {
        crate::infrastructure::adapters::main_checkout_from_files(Path::new(&common_dir))
    }))
}

/// What the queue service answers `read` by `call`: `stats` and `kpi`
/// are sent as they are, as no Git is read for them on either side; the
/// stopped `stats --ledger` is refused before the service is asked
/// ([`stopped_read`]).
pub fn call_stats_read(
    read: &crate::application::queue_reads::QueueRead,
    mut call: impl FnMut(&crate::application::queue_reads::QueueRead) -> Result<Value>,
) -> Result<Value> {
    if let Some(stopped) = stopped_read(read) {
        refuse::<()>(stopped)?;
    }
    call(read)
}

/// The stopped entry `read` is, if it is one: `stats --ledger`.
fn stopped_read(read: &crate::application::queue_reads::QueueRead) -> Option<StoppedRead> {
    use crate::application::queue_reads::QueueRead;
    match read {
        QueueRead::Stats(stats) if stats.ledger => Some(StoppedRead::StatsLedger),
        _ => None,
    }
}

/// `report`'s params as the queue service's client sends them
/// ([`crate::application::report::prepare_request`]), from what `call`
/// answers (the service, or `run_command` on the DB), with the host's Git
/// for the main checkout, and the host's d2 and TALA for the diagram.
pub(super) fn prepare_report(
    params: &mut crate::application::queue_commands::ReportParams,
    call: impl FnMut(&crate::application::queue_commands::QueueCommand) -> Result<Value>,
) -> Result<()> {
    crate::application::report::prepare_request(
        params,
        &crate::application::report::ReportClient {
            main_checkout: &crate::infrastructure::adapters::main_checkout_of,
            diagram: &d2_renderer(std::env::var_os("PATH")),
        },
        call,
    )
}

/// Writes the report the queue service (or `run_command` on the DB)
/// answered as files (`report --out`, [`crate::application::report::ReportFiles`])
/// under `out` with this process's permissions, its retention and its
/// index too: what was written and removed, as a report written under the
/// queue's `reports/` answers it.
pub fn write_report_files(out: &Path, answer: Value) -> Result<Value> {
    let rendered: crate::application::report::ReportFiles =
        serde_json::from_value(answer).context("the report's answer holds no files")?;
    Ok(serde_json::to_value(
        crate::application::report::write_files(&LocalRunFiles, out, &rendered)?,
    )?)
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
fn bound_ci_watch<E: TransactionEntries>(
    queue: &StoreQueue<E>,
) -> Result<Option<crate::domain::ci_watch::CiWatchConfig>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    match bound_checkout(queue)? {
        Some(checkout) => crate::infrastructure::run_env::load_ci_watch(&checkout),
        None => Ok(None),
    }
}

/// `ci failures [--task ID]` on the queue at `db`: see
/// [`crate::application::ci_watch::known_failures`]. Reads only.
#[cfg(feature = "test-paths")]
pub fn ci_failures(db: &Path, task: Option<TaskId>) -> Result<Value> {
    ci_failures_of(&SqliteQueue::open_read_only(db)?, task)
}

/// `ci failures [--task ID]` on `queue`, as the command line and the queue
/// service answer it.
fn ci_failures_of<E: TransactionEntries>(
    queue: &StoreQueue<E>,
    task: Option<TaskId>,
) -> Result<Value>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    let config = bound_ci_watch(queue)?;
    let branch = config.as_ref().and_then(|config| config.branch.clone());
    crate::application::ci_watch::known_failures(queue, config.as_ref(), branch.as_deref(), task)
}

/// `doctor`'s `ci_watch` (ADR-t1920-1): the table, the `gh` this PATH
/// resolves, whether it is logged in, the repository, and the supervisor's
/// last `ci_watch_unavailable` / `ci_watch_available`; none without the
/// table, `error` when the file cannot be read.
fn doctor_ci_watch<E: TransactionEntries>(queue: &StoreQueue<E>) -> Result<Option<Value>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
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

/// `status`'s and `doctor`'s `replans` ([`health::replans_report`]) of the
/// queue under `[replan]` of the bound main checkout.
fn replans_view<E: TransactionEntries>(queue: &StoreQueue<E>) -> Result<Value>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    let config = bound_checkout(queue)?
        .map(|checkout| crate::infrastructure::run_env::load_replan(&checkout));
    health::replans_report(queue, &health::ReplanConfigRead::of(config))
}

/// `status`'s, `stats`' and `doctor`'s `recurring` ([`health::recurring`])
/// of the `dagq.toml` of the bound main checkout and the queue's history;
/// none for a queue bound to no checkout.
fn recurring_view<E: TransactionEntries>(queue: &StoreQueue<E>) -> Result<Option<Value>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    recurring_view_of(queue, bound_checkout(queue)?.as_deref())
}

/// [`recurring_view`] of the `dagq.toml` of `checkout`, the bound main
/// checkout.
fn recurring_view_of<E: TransactionEntries>(
    queue: &StoreQueue<E>,
    checkout: Option<&Path>,
) -> Result<Option<Value>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    let Some(checkout) = checkout else {
        return Ok(None);
    };
    health::recurring(
        crate::infrastructure::run_env::load_recurring(checkout),
        |items| queue.recurring_schedule(items),
    )
}

/// `stats` on the system clock: see [`OneShot::stats`].
#[cfg(feature = "test-paths")]
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
pub fn read_queue<E: TransactionEntries>(
    queue: &mut StoreQueue<E>,
    db: &Path,
    one_shot: &OneShot,
    read: &crate::application::queue_reads::QueueRead,
) -> Result<Value>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
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
pub fn service_reads<E: TransactionEntries + 'static>()
-> crate::infrastructure::queue_service::ServiceReads<StoreQueue<E>>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
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
/// service, where no Git runs (ADR-t3102-3 decision 5): what a client
/// older than the ledger's `stats` sent with `stats` or `kpi`
/// ([`StatsGitReads`]) is not read.
struct HostReads<'a> {
    db: &'a Path,
    one_shot: &'a OneShot,
    served: bool,
}

impl<E: TransactionEntries> crate::application::queue_reads::PlanningSources<StoreQueue<E>>
    for HostReads<'_>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    fn changes(&self, queue: &StoreQueue<E>) -> Result<Option<crate::domain::ChangeSet>> {
        task_changes(queue)
    }
    fn title_prefixes(
        &self,
        queue: &StoreQueue<E>,
    ) -> Result<Option<crate::domain::TitlePrefixes>> {
        task_title_prefixes(queue)
    }
    fn related_docs(
        &self,
        queue: &StoreQueue<E>,
    ) -> Result<Vec<crate::domain::doc_hints::RelatedDocument>> {
        crate::compose::task_related_docs(queue)
    }
    fn goal_tags(&self, queue: &StoreQueue<E>) -> Result<Option<crate::domain::TagSet>> {
        goal_tags(queue)
    }
    fn claim_holds(&self, queue: &StoreQueue<E>) -> Result<Vec<Value>> {
        health::claim_holds(queue, &SystemProcesses, queue.generators().clock.as_ref())
    }
    fn live_supervisors(&self, queue: &StoreQueue<E>) -> Result<Vec<String>> {
        health::live_supervisors(queue, &SystemProcesses, queue.generators().clock.as_ref())
    }
    fn class_aging(
        &self,
        queue: &StoreQueue<E>,
    ) -> Result<crate::application::claim_view::AgingInputs> {
        crate::compose::class_aging_inputs(queue)
    }
    fn clock<'q>(&self, queue: &'q StoreQueue<E>) -> &'q dyn crate::application::Clock {
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

impl<E: TransactionEntries> crate::application::queue_reads::ObservationSources<StoreQueue<E>>
    for HostReads<'_>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    fn status(&self, queue: &StoreQueue<E>, role: Option<SessionRole>) -> Result<Value> {
        self.one_shot.status_of(self.db, queue, role)
    }
    fn stats(
        &self,
        queue: &StoreQueue<E>,
        query: &StatsQuery,
        _: Option<&StatsGitReads>,
    ) -> Result<Value> {
        self.one_shot.stats_read(queue, query, self.served)
    }
    fn ledger_check(
        &self,
        _: &StoreQueue<E>,
        _: &StatsQuery,
        _: Option<&StatsGitReads>,
    ) -> Result<Value> {
        Ok(refuse(StoppedRead::StatsLedger)?)
    }
    fn stats_inputs(&self, queue: &StoreQueue<E>) -> Result<Value> {
        Ok(serde_json::to_value(statistics::stats_inputs(
            served_checkout(queue)?,
            crate::application::EventStore::latest_event_id(queue)?,
        ))?)
    }
    fn kpi(
        &self,
        queue: &StoreQueue<E>,
        query: &crate::domain::kpi::KpiQuery,
        _: Option<&StatsGitReads>,
    ) -> Result<Value> {
        self.one_shot.kpi_read(queue, self.db, query, self.served)
    }
    fn observer_kpi(&self, queue: &StoreQueue<E>, _: Option<&StatsGitReads>) -> Result<Value> {
        self.one_shot.observer_kpi_read(queue, self.db, self.served)
    }
    fn forecast(
        &self,
        queue: &StoreQueue<E>,
        query: &crate::application::forecast::ForecastQuery,
    ) -> Result<Value> {
        self.one_shot.forecast_of(queue, self.db, query)
    }
    fn improvements(&self, queue: &StoreQueue<E>) -> Result<Value> {
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
    fn clock<'q>(&self, queue: &'q StoreQueue<E>) -> &'q dyn crate::application::Clock {
        queue.generators().clock.as_ref()
    }
    fn plan_request(
        &self,
        queue: &StoreQueue<E>,
        id: crate::domain::RequestId,
    ) -> Result<crate::domain::plan_request::PlanRequest> {
        queue.request_by_id(id)
    }
    fn plan_requests(
        &self,
        queue: &StoreQueue<E>,
        all: bool,
    ) -> Result<Vec<crate::domain::plan_request::PlanRequest>> {
        queue.plan_requests(all)
    }
    fn replan_brief(
        &self,
        queue: &StoreQueue<E>,
        id: crate::domain::ReplanId,
    ) -> Result<crate::domain::replan_plan::ReplanBrief> {
        queue.replan_brief(id)
    }
    fn replan_operations(
        &self,
        queue: &StoreQueue<E>,
        all: bool,
    ) -> Result<Vec<crate::domain::task_replan::ReplanOperation>> {
        queue.replan_operations(all)
    }
    fn ci_failures(
        &self,
        queue: &StoreQueue<E>,
        read: &crate::application::queue_reads::CiFailuresRead,
    ) -> Result<Value> {
        ci_failures_of(queue, read.task.map(TaskId::new))
    }
    fn planners(
        &self,
        queue: &StoreQueue<E>,
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
/// provider that starts it. Started by a supervisor as its own
/// ([`crate::domain::queue_service::SUPERVISOR_REGISTRATION_ENV`]), it
/// opens no queue and reaches it through the service as that supervisor.
pub fn observe(
    db: &Path,
    provider: &dyn AgentProvider,
    signals: Option<&dyn crate::application::AgentSignals>,
    options: &crate::application::observer::ObserveOptions,
) -> Result<Value> {
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    if let Some(queues) = job_queues(&db)? {
        return observe_through(&db, queues, provider, signals, options);
    }
    observe_on_db(&db, provider, signals, options)
}

/// [`observe`] on the queue at `db` (canonical) opened in this process: a
/// test's, which a library call runs. A process of the binary opens no
/// queue, and one not started by a supervisor is refused.
#[cfg(not(feature = "test-paths"))]
fn observe_on_db(
    _: &Path,
    _: &dyn AgentProvider,
    _: Option<&dyn crate::application::AgentSignals>,
    _: &crate::application::observer::ObserveOptions,
) -> Result<Value> {
    anyhow::bail!(NO_SUPERVISOR)
}

/// [`observe`] on the queue at `db` (canonical) opened in this process: a
/// test's, which a library call runs.
#[cfg(feature = "test-paths")]
fn observe_on_db(
    db: &Path,
    provider: &dyn AgentProvider,
    signals: Option<&dyn crate::application::AgentSignals>,
    options: &crate::application::observer::ObserveOptions,
) -> Result<Value> {
    let db = db.to_path_buf();
    let mut queue = SqliteQueue::open(&db)?;
    // The agent's events go to this command's own queue, as its records do.
    let events = SqliteQueue::open(&db)?;
    let generators = queue.generators().clone();
    let sources = ObserverReads {
        one_shot: OneShot::new(generators.clone()),
    };
    let telemetry = crate::infrastructure::queue_service::RequestedTelemetryTokens::of_caller(&db);
    crate::application::observer::observe(
        &mut queue,
        &db,
        provider,
        signals,
        options,
        &crate::application::observer::ObserverEnvironment {
            sources: &sources,
            host: &crate::infrastructure::observer::LocalObserver {
                events: Some(&events),
            },
            generators: &generators,
            pid: std::process::id(),
            // The executor's token is issued by the queue's service at
            // the request of the command's caller (ADR-t3102-1 decision 2).
            telemetry: Some(crate::application::telemetry_tokens::TargetTelemetry {
                tokens: &telemetry,
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

/// [`observe`] of the queue at `db` (canonical) through the supervisor's
/// queue `queues`, opening none: the observation of a supervisor that
/// started the command as its own.
pub fn observe_through(
    db: &Path,
    queues: SupervisorQueues,
    provider: &dyn AgentProvider,
    signals: Option<&dyn crate::application::AgentSignals>,
    options: &crate::application::observer::ObserveOptions,
) -> Result<Value> {
    let tokens = job_tokens(queues.clone());
    observe_with(db, queues, tokens, provider, signals, options)
}

/// [`observe_through`] with its agent's telemetry's tokens `tokens`: a
/// supervisor's ([`job_tokens`]), or the caller's of a control-side command
/// ([`super::ControlQueues`]).
pub(super) fn observe_with(
    db: &Path,
    queues: SupervisorQueues,
    tokens: ServiceTokens,
    provider: &dyn AgentProvider,
    signals: Option<&dyn crate::application::AgentSignals>,
    options: &crate::application::observer::ObserveOptions,
) -> Result<Value> {
    let generators = clock::system();
    let (mut queue, events) = (queues.queue(), queues.queue());
    crate::application::observer::observe(
        &mut queue,
        db,
        provider,
        signals,
        options,
        &crate::application::observer::ObserverEnvironment {
            sources: &crate::infrastructure::job_reads::JobReads,
            host: &crate::infrastructure::observer::LocalObserver {
                events: Some(&events),
            },
            generators: &generators,
            pid: std::process::id(),
            telemetry: Some(crate::application::telemetry_tokens::TargetTelemetry {
                tokens: &tokens,
                db,
                lease: None,
            }),
            steps: Some(crate::application::step_report::CommandSteps {
                files: &crate::infrastructure::run_files::LocalRunFiles,
                sinks: Arc::new(crate::infrastructure::queue_service::SystemServiceAccess),
            }),
        },
    )
}

/// [`throughput_review`] of the queue at `db` (canonical) through the
/// supervisor's queue `queues`, opening none: the review of a supervisor
/// that started the command as its own.
pub fn throughput_review_through(
    db: &Path,
    queues: SupervisorQueues,
    provider: &dyn AgentProvider,
    options: &crate::application::throughput_review::ReviewOptions,
) -> Result<Value> {
    let tokens = job_tokens(queues.clone());
    throughput_review_with(db, queues, tokens, provider, options)
}

/// [`throughput_review_through`] with its agent's telemetry's tokens
/// `tokens` ([`observe_with`]).
pub(super) fn throughput_review_with(
    db: &Path,
    queues: SupervisorQueues,
    tokens: ServiceTokens,
    provider: &dyn AgentProvider,
    options: &crate::application::throughput_review::ReviewOptions,
) -> Result<Value> {
    let generators = clock::system();
    let (mut queue, events) = (queues.queue(), queues.queue());
    crate::application::throughput_review::review(
        &mut queue,
        db,
        provider,
        options,
        &crate::application::throughput_review::ThroughputReviewEnvironment {
            sources: &crate::infrastructure::job_reads::JobReads,
            host: &crate::infrastructure::throughput_review::LocalThroughputReview {
                events: Some(&events),
            },
            generators: &generators,
            signals: Some(&review_signals()),
            telemetry: Some(crate::application::telemetry_tokens::TargetTelemetry {
                tokens: &tokens,
                db,
                lease: None,
            }),
            steps: Some(crate::application::step_report::CommandSteps {
                files: &crate::infrastructure::run_files::LocalRunFiles,
                sinks: Arc::new(crate::infrastructure::queue_service::SystemServiceAccess),
            }),
        },
    )
}

/// Claude Code's reading of the walls in a review's output, which starts
/// nothing (task 438, ADR-t1857-1).
fn review_signals() -> crate::infrastructure::adapters::ClaudeCode {
    crate::infrastructure::adapters::ClaudeCode {
        executable: PathBuf::from("claude"),
    }
}

/// Why a job command no supervisor started is refused: it reaches the
/// queue only as the supervisor's (ADR-t3102-2 decision 1).
#[cfg(not(feature = "test-paths"))]
const NO_SUPERVISOR: &str = "a job command reaches the queue only through its supervisor's queue \
     service, and opens no queue: run it as a supervisor starts it, or through `dagq` on the host";

/// The queue of the supervisor that started this job command as its own
/// ([`crate::domain::queue_service::SUPERVISOR_REGISTRATION_ENV`]), through
/// the service with that supervisor's token; `None` for a command no
/// supervisor started, which only a test's library call runs on its own
/// queue (the binary refuses it).
fn job_queues(db: &Path) -> Result<Option<SupervisorQueues>> {
    let Some(registration) =
        std::env::var(crate::domain::queue_service::SUPERVISOR_REGISTRATION_ENV)
            .ok()
            .filter(|registration| !registration.is_empty())
    else {
        return Ok(None);
    };
    let actor = crate::domain::ActorContext::from_env(|name| std::env::var(name).ok())?;
    let pid = crate::application::supervisor_service::job_command_pid(&actor)?;
    let caller = crate::infrastructure::queue_service::SocketCaller::adopt(
        db,
        &crate::domain::LeaseToken::new(registration),
        pid,
    )?;
    // Its results' records are sent again on its own thread until the
    // service takes them, and past the wait kept for the supervisor to send
    // (ADR-t3102-3 decision 5).
    let caller = crate::application::supervisor_service::resend::Resending::new(
        Arc::new(caller),
        crate::application::supervisor_service::resend::Resend::Wait,
        clock::system(),
    )
    .kept_in(Arc::new(crate::infrastructure::held_records::HeldDir::of(
        db,
    )));
    Ok(Some(SupervisorQueues::new(Arc::new(caller), actor)))
}

/// The telemetry tokens of a job command's agent, issued and revoked
/// through the service by the supervisor's queue `queues`.
fn job_tokens(queues: SupervisorQueues) -> ServiceTokens {
    ServiceTokens {
        queues,
        files: Arc::new(crate::infrastructure::queue_service::SystemServiceAccess),
        telemetry_files: Arc::new(crate::infrastructure::queue_service::SystemServiceAccess),
    }
}

/// The observer's reads that [`OneShot`] gives on the opened queue.
#[cfg(feature = "test-paths")]
struct ObserverReads {
    one_shot: OneShot,
}

#[cfg(feature = "test-paths")]
impl<E: TransactionEntries> crate::application::observer::ObserverSources<StoreQueue<E>>
    for ObserverReads
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    fn stats(&self, queue: &StoreQueue<E>, db: &Path, query: &StatsQuery) -> Result<Value> {
        self.one_shot.stats_of(queue, db, query)
    }
    fn kpi(&self, queue: &StoreQueue<E>, db: &Path) -> Result<Value> {
        self.one_shot.observer_kpi(queue, db)
    }
    fn improvements(&self, queue: &StoreQueue<E>) -> Result<Value> {
        self.one_shot.improvements_of(queue)
    }
    fn checkout(&self, queue: &StoreQueue<E>) -> Result<Option<PathBuf>> {
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
    if let Some(queues) = job_queues(&db)? {
        return throughput_review_through(&db, queues, provider, options);
    }
    throughput_review_on_db(&db, provider, options)
}

/// [`throughput_review`] on the queue at `db` (canonical) opened in this
/// process: a test's, which a library call runs. A process of the binary
/// opens no queue, and one not started by a supervisor is refused.
#[cfg(not(feature = "test-paths"))]
fn throughput_review_on_db(
    _: &Path,
    _: &dyn AgentProvider,
    _: &crate::application::throughput_review::ReviewOptions,
) -> Result<Value> {
    anyhow::bail!(NO_SUPERVISOR)
}

/// [`throughput_review`] on the queue at `db` (canonical) opened in this
/// process: a test's, which a library call runs.
#[cfg(feature = "test-paths")]
fn throughput_review_on_db(
    db: &Path,
    provider: &dyn AgentProvider,
    options: &crate::application::throughput_review::ReviewOptions,
) -> Result<Value> {
    let db = db.to_path_buf();
    let signals = review_signals();
    let mut queue = SqliteQueue::open(&db)?;
    // The agent's events go to this command's own queue, as its records do.
    let events = SqliteQueue::open(&db)?;
    let generators = queue.generators().clone();
    let sources = ThroughputReviewReads {
        one_shot: OneShot::new(generators.clone()),
    };
    let telemetry = crate::infrastructure::queue_service::RequestedTelemetryTokens::of_caller(&db);
    crate::application::throughput_review::review(
        &mut queue,
        &db,
        provider,
        options,
        &crate::application::throughput_review::ThroughputReviewEnvironment {
            sources: &sources,
            host: &crate::infrastructure::throughput_review::LocalThroughputReview {
                events: Some(&events),
            },
            generators: &generators,
            signals: Some(&signals),
            // The executor's token is issued by the queue's service at
            // the request of the command's caller (ADR-t3102-1 decision 2).
            telemetry: Some(crate::application::telemetry_tokens::TargetTelemetry {
                tokens: &telemetry,
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
#[cfg(feature = "test-paths")]
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
#[cfg(feature = "test-paths")]
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
#[cfg(feature = "test-paths")]
struct ThroughputReviewReads {
    one_shot: OneShot,
}

#[cfg(feature = "test-paths")]
impl<E: TransactionEntries>
    crate::application::throughput_review::ThroughputReviewSources<StoreQueue<E>>
    for ThroughputReviewReads
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    fn stats(&self, queue: &StoreQueue<E>, db: &Path, query: &StatsQuery) -> Result<Value> {
        self.one_shot.stats_of(queue, db, query)
    }
    fn kpi(
        &self,
        queue: &StoreQueue<E>,
        db: &Path,
        query: &crate::domain::kpi::KpiQuery,
    ) -> Result<Value> {
        self.one_shot.kpi_of(queue, db, query)
    }
    fn checkout(&self, queue: &StoreQueue<E>) -> Result<Option<PathBuf>> {
        bound_checkout(queue)
    }
    fn record_finding(
        &self,
        queue: &mut StoreQueue<E>,
        finding: crate::domain::NewFinding,
    ) -> Result<i64> {
        Ok(queue.record_finding(finding)?.finding.id.as_i64())
    }
}

/// `events --after`: the events after `after`, oldest first, in compact
/// form with no filter.
#[cfg(feature = "test-paths")]
pub fn events(db: &Path, after: crate::domain::EventId, limit: usize, all: bool) -> Result<Value> {
    events_matching(
        db,
        &crate::application::watch::EventsQuery {
            after,
            limit,
            all,
            full: false,
            filter: crate::domain::EventFilter::default(),
        },
    )
}

/// `events` with its filters and `--full` on the queue at `db`.
#[cfg(feature = "test-paths")]
pub fn events_matching(db: &Path, query: &crate::application::watch::EventsQuery) -> Result<Value> {
    crate::application::watch::events_in(&SqliteQueue::open_read_only(db)?, query)
}

/// `timeline RUN` on the queue at `db`.
#[cfg(feature = "test-paths")]
pub fn timeline(db: &Path, run: &RunId, gap_secs: i64, full: bool) -> Result<Value> {
    timeline_in(&SqliteQueue::open_read_only(db)?, run, gap_secs, full)
}

/// [`timeline`] on a queue the caller already opened, so a command opens
/// it once.
pub fn timeline_in<E: TransactionEntries>(
    queue: &StoreQueue<E>,
    run: &RunId,
    gap_secs: i64,
    full: bool,
) -> Result<Value>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
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
#[cfg(feature = "test-paths")]
pub fn watch(db: &Path, options: &crate::application::watch::WatchOptions) -> Result<Value> {
    watch_in(db, &SqliteQueue::open_watch(db)?, options, false)
}

/// `watch` on the live read-only queue its caller opened (the queue
/// service's look, or a test's), never a migrated snapshot
/// ([`SqliteQueue::open_watch`]). With
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
        "watch requires a live read-only queue connection; use SqliteQueue::open_watch"
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
    fn unreachable(&self, error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<crate::infrastructure::queue_service::ClientFailure>()
            .is_some_and(|failure| failure.code == "unreachable")
    }

    fn look(
        &self,
        after: Option<crate::domain::EventId>,
        role: Option<SessionRole>,
        limit: std::time::Duration,
    ) -> Result<crate::application::watch::WatchLook> {
        watch_poll(self.client, after, role, limit)
    }

    fn outages(&self) -> Option<&dyn crate::application::watch::OutageNotices> {
        Some(self)
    }
}

/// One look of a watch as the queue service's `watch_poll` through
/// `client`, past `after` for `role`, within `limit`.
fn watch_poll(
    client: &crate::infrastructure::queue_service::Client,
    after: Option<crate::domain::EventId>,
    role: Option<SessionRole>,
    limit: std::time::Duration,
) -> Result<crate::application::watch::WatchLook> {
    let answer = client.watch_poll_within(watch_poll_params(after, role), limit)?;
    serde_json::from_value(answer).context("the queue service's answer to watch_poll")
}

/// `watch` of the agent of a person's inbox for a control side, through
/// its relay (`client`'s route): the same looks as a watch through the
/// service ([`watch_through_service`]), each one `watch_poll` the relay
/// sends, with the output of any watch. No queue's directory is read or
/// written (no watcher's record, no outage told or notifier, which are
/// the host's): a look that gets no answer ends the watch with its
/// failure, and the watch goes to no DB and no service's socket instead.
/// Narrowing it to the person's own asks is the control side's.
pub fn watch_through_relay(
    client: &crate::infrastructure::queue_service::Client,
    options: &crate::application::watch::WatchOptions,
) -> Result<Value> {
    let clock = crate::infrastructure::clock::system().clock;
    let call = |_use_case, params, limit| client.watch_poll_within(params, limit);
    watch_relayed(&call, clock.as_ref(), options)
}

/// One use case sent through a relay within a limit: the result, or the
/// client's failure.
type RelayCall<'a> =
    dyn Fn(crate::domain::queue_service::UseCase, Value, std::time::Duration) -> Result<Value> + 'a;

/// [`watch_through_relay`] with each look sent by `call`.
fn watch_relayed(
    call: &RelayCall<'_>,
    clock: &dyn crate::application::Clock,
    options: &crate::application::watch::WatchOptions,
) -> Result<Value> {
    crate::application::watch::watch_through(&RelayWatch { call }, clock, None, None, options)
}

/// A watch's looks through an inbox's relay, which knows no outage of a
/// service.
struct RelayWatch<'a> {
    call: &'a RelayCall<'a>,
}

impl crate::application::watch::WatchSource for RelayWatch<'_> {
    fn unreachable(&self, error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<crate::infrastructure::queue_service::ClientFailure>()
            .is_some_and(|failure| failure.code == "unreachable")
    }

    fn look(
        &self,
        after: Option<crate::domain::EventId>,
        role: Option<SessionRole>,
        limit: std::time::Duration,
    ) -> Result<crate::application::watch::WatchLook> {
        let answer = (self.call)(
            crate::domain::queue_service::UseCase::WatchPoll,
            watch_poll_params(after, role),
            limit,
        )?;
        serde_json::from_value(answer).context("the queue service's answer to watch_poll")
    }
}

/// The params of `watch_poll` past `after` for `role`.
fn watch_poll_params(after: Option<crate::domain::EventId>, role: Option<SessionRole>) -> Value {
    let mut params = serde_json::json!({});
    if let Some(after) = after {
        params["after"] = serde_json::json!(after);
    }
    if let Some(role) = role {
        params["role"] = serde_json::json!(role.as_str());
    }
    params
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

/// `status`'s `requests` ([`health::requests`]): the `open` planning requests with the planner
/// of the runtime's open for each and its asks, and which of them wait for a planner to open.
fn open_requests<E: TransactionEntries>(queue: &StoreQueue<E>) -> Result<Value>
where
    StoreQueue<E>: crate::application::SearchIndex,
{
    let waiting: Vec<_> = queue
        .planner_requests()?
        .into_iter()
        .map(|request| request.id)
        .collect();
    let open = queue
        .plan_requests(false)?
        .into_iter()
        .map(|request| {
            let planner = queue.request_planner(request.id)?;
            let asks = queue.request_asks(request.id)?;
            Ok((request, planner, asks))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(health::requests(&open, &waiting))
}

impl OneShot {
    /// `status --role`: see [`health::status`], measured to these
    /// generators' now.
    #[cfg(feature = "test-paths")]
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
    pub fn status_of<E: TransactionEntries>(
        &self,
        db: &Path,
        queue: &StoreQueue<E>,
        role: Option<SessionRole>,
    ) -> Result<Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        self.status_with_agents(db, queue, role, || launchd_queue_agents(db))
    }

    /// [`Self::status_of`] judging what `agents` gives as the queue's
    /// LaunchAgents ([`queue_agents`]) for `duplicate_supervisors`
    /// ([`health::duplicate_supervisors`]); tests give a fake launchd's.
    pub fn status_with_agents<E: TransactionEntries>(
        &self,
        db: &Path,
        queue: &StoreQueue<E>,
        role: Option<SessionRole>,
        agents: impl FnOnce() -> Vec<lifecycle::QueueAgent>,
    ) -> Result<Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
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
            crate::application::ci_watch::freshness_reading(&ci_watch),
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
        status["replans"] = replans_view(queue)?;
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
            status["queue_broker"] = queue_broker_view(db, Some(queue));
            status["requests"] = open_requests(queue)?;
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
    fn language_report<E: TransactionEntries>(
        &self,
        queue: &StoreQueue<E>,
    ) -> Result<crate::infrastructure::language::LanguageReport>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        Ok(crate::infrastructure::language::language_report(
            bound_checkout(queue)?.as_deref(),
            self.user_config.as_deref(),
        ))
    }

    /// `doctor` of the queue at `db`, opened in this process: a test's
    /// library call. The binary asks the queue's service, which makes the
    /// report with [`Self::doctor_of`] (ADR-t3102-2 decision 1).
    #[cfg(feature = "test-paths")]
    pub fn doctor(&self, db: &Path, full: bool, common_dir: Option<&str>) -> Result<Value> {
        self.doctor_with_agents(db, full, common_dir, || launchd_queue_agents(db))
    }

    /// [`Self::doctor`] judging what `agents` gives as the queue's
    /// LaunchAgents ([`queue_agents`]); `common_dir`, when given, is the
    /// repository the queue must be bound to, as the service checks it of
    /// the binary's request.
    #[cfg(feature = "test-paths")]
    pub fn doctor_with_agents(
        &self,
        db: &Path,
        full: bool,
        common_dir: Option<&str>,
        agents: impl FnOnce() -> Vec<lifecycle::QueueAgent>,
    ) -> Result<Value> {
        if let Some(common_dir) = common_dir
            && let Some(bound) = SqliteQueue::binding_at(db)?
        {
            ensure!(
                bound == common_dir,
                "queue is bound to another Git repository: {bound} (this repository is {common_dir})"
            );
        }
        let (schema, queue) = SqliteQueue::inspect_read_only(db)?;
        let mut report = self.doctor_of(db, schema, queue, full, agents)?;
        doctor_on_this_host(&mut report)?;
        Ok(report)
    }

    /// `doctor`: see [`health::doctor`], of the queue at `db` as the
    /// queue's service read it (`queue`), with its `schema` as `migrate
    /// --check` reports it. The schema is reported even for a queue that
    /// needs `migrate` or refuses this binary (ADR-0045 decision 5); such a
    /// queue has no `supervisors` or `runs`, and `error` says why. What
    /// `agents` gives is judged as the queue's LaunchAgents
    /// ([`queue_agents`]) for `duplicate_supervisors`
    /// ([`health::duplicate_supervisors`]), read only for a queue that
    /// opens; tests give a fake launchd's.
    pub fn doctor_of<E: TransactionEntries>(
        &self,
        db: &Path,
        schema: crate::infrastructure::sqlite::SchemaState,
        queue: ReadOnlyQueue<StoreQueue<E>>,
        full: bool,
        agents: impl FnOnce() -> Vec<lifecycle::QueueAgent>,
    ) -> Result<Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        let mut service_view = None;
        let mut broker_view = None;
        let mut report = match queue {
            ReadOnlyQueue::Refused { error, .. } => {
                health::refused(self.generators.clock.now(), &error)
            }
            ReadOnlyQueue::Readable(queue) => {
                let queue = queue.with_generators(self.generators.clone());
                service_view = Some(queue_service_view(db, Some(&queue)));
                broker_view = Some(queue_broker_view(db, Some(&queue)));
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
                report["replans"] = replans_view(&queue)?;
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
        report["queue_service"] = service_view.unwrap_or_else(|| queue_service_view::<E>(db, None));
        // The queue broker (ADR-t2114-1): whether it runs, its address and
        // build, and its attention from a queue that can be read.
        report["queue_broker"] = broker_view.unwrap_or_else(|| queue_broker_view::<E>(db, None));
        // The host's `[update]`, with what was taken as its default
        // (ADR-t618-1 decision 3).
        report["release_update"] = serde_json::to_value(host_update(db))?;
        // The Claude credential the queue broker sends a container's
        // worker with, chosen in host.toml (ADR-t2177-1): never its value.
        report["claude_credential"] = crate::infrastructure::claude_credential::report(
            db.parent().unwrap_or(Path::new(".")),
            crate::infrastructure::kpi_config::host_wide_file().as_deref(),
            crate::infrastructure::claude_credential::Keychain::of_host()
                .as_ref()
                .map(|keychain| {
                    keychain as &dyn crate::infrastructure::claude_credential::KeychainLookup
                }),
        );
        Ok(report)
    }

    /// `stats`: see [`statistics::stats`], measured to these generators'
    /// now, from the queue's ledger alone; `[stall]`, `[conflicts]` and
    /// `[areas]` from the `dagq.toml` of the main checkout of the
    /// repository the queue is bound to.
    #[cfg(feature = "test-paths")]
    pub fn stats(&self, db: &Path, query: &StatsQuery) -> Result<Value> {
        self.stats_of(&self.open_read_only(db)?, db, query)
    }

    /// [`Self::stats`] on `queue`, the queue at `db` the caller already
    /// opened, so a command opens it once.
    pub fn stats_of<E: TransactionEntries>(
        &self,
        queue: &StoreQueue<E>,
        _db: &Path,
        query: &StatsQuery,
    ) -> Result<Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        self.stats_read(queue, query, false)
    }

    /// [`Self::stats_of`], in the queue service when `served` (the main
    /// checkout is the one [`served_checkout`] finds).
    pub fn stats_read<E: TransactionEntries>(
        &self,
        queue: &StoreQueue<E>,
        query: &StatsQuery,
        served: bool,
    ) -> Result<Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        let checkout = stats_checkout(queue, served)?;
        let now = self.generators.clock.now();
        let config_file = || match checkout.as_deref() {
            Some(checkout) => load_stall_config(checkout),
            None => Ok(None),
        };
        let conflicts_file = || match checkout.as_deref() {
            Some(checkout) => load_conflict_config(checkout),
            None => Ok(None),
        };
        let areas = area_map(checkout.as_deref())?;
        let sources = StatsSources {
            config_file: &config_file,
            conflicts_file: &conflicts_file,
            areas: areas.as_ref(),
            utc_offset_secs: clock::local_utc_offset(now),
            counts_checks: counts_checks(checkout.as_deref(), self.profiles),
            rust_toolchain: records_rust_toolchain(checkout.as_deref(), self.profiles),
        };
        let mut stats = serde_json::to_value(statistics::stats(queue, now, query, &sources)?)?;
        // When each recurring draft is registered next (ADR-0072).
        if let Some(recurring) = recurring_view_of(queue, checkout.as_deref())? {
            stats["recurring"] = recurring;
        }
        Ok(stats)
    }

    /// `kpi` (ADR-0051 decision 9): see [`crate::domain::kpi::kpi`],
    /// measured to these generators' now in the host's time zone, judged by
    /// the `[kpi]` of the main checkout's `dagq.toml` with the host's
    /// `host.toml` (the queue's, over the host-wide one) over it.
    pub fn kpi_of<E: TransactionEntries>(
        &self,
        queue: &StoreQueue<E>,
        db: &Path,
        query: &crate::domain::kpi::KpiQuery,
    ) -> Result<Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        self.kpi_read(queue, db, query, false)
    }

    /// [`Self::kpi_of`], in the queue service when `served`, from the
    /// queue's ledger alone ([`crate::application::kpi::kpi`]).
    pub fn kpi_read<E: TransactionEntries>(
        &self,
        queue: &StoreQueue<E>,
        db: &Path,
        query: &crate::domain::kpi::KpiQuery,
        served: bool,
    ) -> Result<Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let checkout = stats_checkout(queue, served)?;
        let setup = report_setup(
            db,
            checkout.as_deref(),
            host_wide.as_deref(),
            now,
            self.profiles,
        )?;
        Ok(serde_json::to_value(crate::application::kpi::kpi(
            queue,
            now,
            setup.host,
            &setup.config,
            setup.areas.as_ref(),
            query,
        )?)?)
    }

    /// `forecast` (ADR-0070 decision 2): see
    /// [`crate::application::forecast::forecast`], at these generators' now,
    /// with the `min_samples` of the `[kpi]` settings `kpi` judges by, the
    /// samples from the queue's ledger.
    pub fn forecast_of<E: TransactionEntries>(
        &self,
        queue: &StoreQueue<E>,
        db: &Path,
        query: &crate::application::forecast::ForecastQuery,
    ) -> Result<Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let setup = report_setup(
            db,
            bound_checkout(queue)?.as_deref(),
            host_wide.as_deref(),
            now,
            self.profiles,
        )?;
        Ok(serde_json::to_value(
            crate::application::forecast::forecast(
                queue,
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
    /// them, of one window of the ledger, with the open breaches' events
    /// ([`crate::domain::kpi::observe::observer_input`]).
    /// Uses the same KPI settings and host-load records as the KPI command,
    /// including host-load targets. The input carries target judgments,
    /// each breach's kind, subject, evidence and marks, and the trend and
    /// forecast scoring for each period.
    pub fn observer_kpi<E: TransactionEntries>(
        &self,
        queue: &StoreQueue<E>,
        db: &Path,
    ) -> Result<serde_json::Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        self.observer_kpi_read(queue, db, false)
    }

    /// [`Self::observer_kpi`], in the queue service when `served`.
    pub fn observer_kpi_read<E: TransactionEntries>(
        &self,
        queue: &StoreQueue<E>,
        db: &Path,
        served: bool,
    ) -> Result<serde_json::Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        use crate::domain::kpi::{KpiQuery, Period, observe};
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let checkout = stats_checkout(queue, served)?;
        let setup = report_setup(
            db,
            checkout.as_deref(),
            host_wide.as_deref(),
            now,
            self.profiles,
        )?;
        let observed = |period, last| KpiQuery {
            period,
            last,
            ..KpiQuery::default()
        };
        let window = crate::application::report::window(
            queue,
            now,
            &setup,
            vec![observed(Period::Day, 7), observed(Period::Week, 4)],
        )?;
        let of = |period, last| {
            crate::application::kpi::of_window(
                &window,
                now,
                setup.host,
                &setup.config,
                setup.areas.as_ref(),
                &observed(period, last),
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
    pub fn improvements_of<E: TransactionEntries>(
        &self,
        queue: &StoreQueue<E>,
    ) -> Result<serde_json::Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        let limit = match bound_checkout(queue)? {
            Some(checkout) => max_improvement_proposals(&checkout)?,
            None => crate::domain::kpi::config::DEFAULT_MAX_IMPROVEMENT_PROPOSALS,
        };
        crate::application::kpi::improvements(queue, limit)
    }

    /// `report_inputs` ([`crate::application::report::report_inputs`]) of
    /// `queue`: a checkout `params` names is accepted only as the main
    /// checkout [`served_checkout`] finds.
    pub fn report_inputs_of<E: TransactionEntries>(
        &self,
        queue: &StoreQueue<E>,
        params: &crate::application::queue_commands::ReportInputsParams,
    ) -> Result<Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
        use crate::application::report;
        let window = report::diagram_window(queue, self.generators.clock.now())?;
        Ok(serde_json::to_value(report::report_inputs(
            params,
            queue.repository_binding()?,
            served_checkout(queue)?.as_deref(),
            || report::diagram_draft(&window),
            &report::ReportInputsSources {
                files: &LocalRunFiles,
            },
        )?)?)
    }

    /// `report` (ADR-0051 decision 21): the KPI report of the `period` that
    /// holds `at` (now without; today's and this week's are partial),
    /// made as the supervisor makes it with what its caller read and drew
    /// ([`prepare_report`]: the near-term diagram and the main checkout)
    /// from the queue's ledger and written under the queue's `reports/`,
    /// with `print` only returned, or with `files` answered as the files
    /// its caller writes under its own `--out`
    /// ([`crate::application::report::ReportFiles`]): nothing is written or
    /// removed anywhere but under `reports/`, and params naming an `out`
    /// are refused before they get here
    /// ([`crate::application::queue_commands::ReportParams`]). No Git or
    /// program runs: the `dagq.toml` read is the main checkout's
    /// [`served_checkout`] finds, and a checkout `params` names that is not
    /// it is refused ([`crate::application::report::accept_checkout`]).
    pub fn report_of<E: TransactionEntries>(
        &self,
        queue: &StoreQueue<E>,
        db: &Path,
        params: &crate::application::queue_commands::ReportParams,
    ) -> Result<Value>
    where
        StoreQueue<E>: crate::application::SearchIndex,
    {
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
        let served = served_checkout(queue)?;
        let checkout = params
            .checkout
            .as_deref()
            .map(|checkout| report::accept_checkout(&LocalRunFiles, checkout, served.as_deref()))
            .transpose()?;
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let setup = report_setup(db, checkout, host_wide.as_deref(), now, self.profiles)?;
        let window = report::window(queue, now, &setup, vec![report::report_query(period, at)])?;
        let made = report::make_with(&window, &setup, now, period, at, diagram.into())?;
        if params.print {
            return Ok(serde_json::to_value(made)?);
        }
        if params.files {
            return Ok(serde_json::to_value(report::render(
                &setup, &made, period, now,
            )?)?);
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
    ledger: Arc<dyn crate::application::QueueOpener<dyn crate::application::LedgerStore + Send>>,
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
        ledger,
    }
}

/// The forecast snapshots' port of the supervisor (ADR-0070 decision 3):
/// the `min_samples` of the `[kpi]` [`report_setup`] reads, looked for
/// every `check`, the samples read from the LedgerStore `ledger` opens.
pub(super) fn forecast_port(
    db: &Path,
    checkout: &Path,
    host_wide: Option<PathBuf>,
    check: Duration,
    ledger: Arc<dyn crate::application::QueueOpener<dyn crate::application::LedgerStore + Send>>,
) -> crate::application::supervise::ForecastPort {
    let (db, checkout) = (db.to_path_buf(), checkout.to_path_buf());
    crate::application::supervise::ForecastPort {
        utc_offset: clock::local_utc_offset,
        min_samples: Arc::new(move |now| {
            report_setup(
                &db,
                Some(&checkout),
                host_wide.as_deref(),
                now,
                &crate::domain::toolchain::BUILTIN,
            )
            .map(|setup| setup.config.min_samples)
        }),
        check,
        ledger,
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

    /// A watch through an inbox's relay: each look is one `watch_poll`
    /// with the cursor and the role, its output is any watch's (the events
    /// and the cursor to go on from), the next watch goes on from that
    /// cursor even after the relay got a new token between the two, and a
    /// look that gets no answer ends the watch with that failure.
    #[test]
    fn a_watch_through_the_relay_looks_with_watch_poll_and_goes_on_from_its_cursor() {
        use super::*;
        use crate::application::inbox_relay::{InboxTokens, RelaySend, SessionGate, relay};
        use crate::domain::queue_service::{
            API_VERSION, ServiceErrorCode, ServiceRequest, ServiceResponse, UseCase,
        };
        use std::sync::Mutex;
        struct Held(Mutex<String>, Mutex<usize>);
        impl InboxTokens for Held {
            fn read(&self) -> Result<Option<String>> {
                Ok(Some(self.0.lock().unwrap().clone()))
            }
            fn renew(&self, last: Option<&str>) -> Result<String> {
                let held = self.0.lock().unwrap().clone();
                if Some(held.as_str()) != last {
                    return Ok(held);
                }
                *self.1.lock().unwrap() += 1;
                *self.0.lock().unwrap() = "new".into();
                Ok("new".into())
            }
        }
        /// Takes "old" for the first look alone, then "new"; each look
        /// answers one event past its cursor.
        struct Side(Mutex<Vec<(Option<String>, Value)>>);
        impl RelaySend for Side {
            fn send(&self, token: Option<&str>, request: &ServiceRequest) -> ServiceResponse {
                let mut sent = self.0.lock().unwrap();
                sent.push((token.map(str::to_owned), request.params.clone()));
                assert_eq!(request.use_case, UseCase::WatchPoll);
                if token == Some("old") && sent.len() > 1 || token.is_none() {
                    return ServiceResponse::failure(ServiceErrorCode::Unauthenticated, "x");
                }
                let after = request.params["after"].as_i64().unwrap_or(10);
                ServiceResponse::success(serde_json::json!({
                    "after": after, "cursor": after + 1, "woken": true, "opened": [],
                    "events": [{"id": after + 1, "kind": "ask_opened"}],
                    "pulses": {}, "supervisors": [],
                }))
            }
        }
        let tokens = Held(Mutex::new("old".into()), Mutex::new(0));
        let side = Side(Mutex::new(Vec::new()));
        let gate = SessionGate::default();
        let call = |use_case, params, _| {
            let request = ServiceRequest {
                caller: None,
                api_version: API_VERSION,
                token: None,
                env_role: None,
                queue: None,
                binding: None,
                use_case,
                params,
            };
            let response = relay(&request, &tokens, &side, &gate);
            match response.result {
                Some(result) if response.ok => Ok(result),
                _ => Err(anyhow::anyhow!("{:?}", response.error.unwrap().code)),
            }
        };
        /// A clock that stands still: no look waits on real time.
        struct Still(std::time::Instant);
        impl crate::application::Clock for Still {
            fn system_time(&self) -> std::time::SystemTime {
                std::time::SystemTime::UNIX_EPOCH
            }
            fn monotonic(&self) -> std::time::Instant {
                self.0
            }
        }
        let still = Still(std::time::Instant::now());
        let clock: &dyn crate::application::Clock = &still;
        let options = |after| crate::application::watch::WatchOptions {
            after,
            timeout: Some(std::time::Duration::from_secs(5)),
            interval: std::time::Duration::from_millis(10),
            role: Some(SessionRole::Inbox),
        };
        let first = watch_relayed(&call, clock, &options(None)).unwrap();
        assert_eq!(first["cursor"], 11, "{first}");
        assert_eq!(first["events"][0]["id"], 11, "{first}");
        let next = watch_relayed(
            &call,
            clock,
            &options(Some(crate::domain::EventId::new(11))),
        )
        .unwrap();
        assert_eq!(next["cursor"], 12, "{next}");
        let sent = side.0.lock().unwrap().clone();
        assert_eq!(sent[0].1, serde_json::json!({"role": "inbox"}));
        assert_eq!(
            sent[1],
            (
                Some("old".into()),
                serde_json::json!({"after": 11, "role": "inbox"})
            )
        );
        assert_eq!(
            sent[2],
            (
                Some("new".into()),
                serde_json::json!({"after": 11, "role": "inbox"})
            )
        );
        assert_eq!(*tokens.1.lock().unwrap(), 1);
        // A relay that answers nothing ends the watch with its failure.
        let gone = |_, _, _| -> Result<Value> { Err(anyhow::anyhow!("unreachable: the relay")) };
        let failed = watch_relayed(&gone, clock, &options(None)).unwrap_err();
        assert!(format!("{failed:#}").contains("unreachable"), "{failed:#}");
    }
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

    /// The client of `stats` (`--ledger`), `kpi` and their job reads is
    /// refused as the entry it is, before the service is asked anything;
    /// another read is not one of them.
    #[test]
    fn the_stopped_reads_are_refused_before_the_service_is_asked() {
        use super::{StoppedRead, call_stats_read, stopped_read};
        use crate::application::queue_reads::{JobRead, QueueRead};
        use crate::domain::queue_service::UseCase;
        let parsed = |use_case, params| QueueRead::parse(use_case, &params).unwrap().unwrap();
        let read = parsed(UseCase::Stats, serde_json::json!({"ledger": true}));
        assert_eq!(stopped_read(&read), Some(StoppedRead::StatsLedger));
        let mut asked = 0;
        let error = call_stats_read(&read, |_| {
            asked += 1;
            Ok(serde_json::json!({}))
        })
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("stats --ledger is stopped"), "{error}");
        assert_eq!(asked, 0);
        // `stats`, `kpi` and `stats_inputs` read the ledger: sent as they are.
        for read in [
            parsed(UseCase::Stats, serde_json::json!({})),
            parsed(UseCase::StatsInputs, serde_json::json!({})),
            parsed(UseCase::Kpi, serde_json::json!({"compare": "12"})),
            QueueRead::Job(JobRead::ObserverKpi { git: None }),
            parsed(UseCase::Events, serde_json::json!({})),
        ] {
            assert_eq!(stopped_read(&read), None, "{read:?}");
            let mut sent = Vec::new();
            call_stats_read(&read, |read| {
                sent.push(read.clone());
                Ok(serde_json::json!({}))
            })
            .unwrap();
            assert_eq!(sent.len(), 1, "{read:?}");
        }
    }
}
