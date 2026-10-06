//! The repository's `dagq.toml` (ADR-0023 decision 3): `[run.env]` holds
//! environment variables every run gets, in its worker workspace and in the
//! verification commands; its values are strings in which
//! `${DAGQ_QUEUE_DIR}` and `${DAGQ_RUN_DIR}` are expanded. `[stall]` holds
//! the thresholds of the stalled-session checks in seconds (ADR-0043
//! decision 4). `[conflicts]` holds the thresholds of the
//! `conflict_hotspot` alert of `stats` (goal 31). `[recheck]` holds the
//! `command` the landing recheck runs on main's tree with a waiting run
//! merged in (ADR-0068 decision 2). `[disk]` holds how much free disk
//! space a claim and a landing need (ADR-0047 decision 44, task 377).
//! `[resume]` holds the limit of a run's conflict-only attempts (ADR-0047
//! decision 24). `[exit]` held the retries of a `/exit` the session held
//! back and the wait after each (ADR-0047 decision 25): since task 1437 it
//! is still checked when the file is parsed and is otherwise ignored. `[worker.trial]` turns on the limited trial of the worker's model
//! (ADR-0079 decision 4). `[roles.<role>]` holds the provider, model and effort
//! of a session other than the worker's (ADR-0079 decision 7, ADR-t1063-1
//! decision 1). `[supervisor]`
//! holds `parallel`, `max_waiting`, `runtime_planners` and `claim_spacing`
//! of a supervisor started without the flags (task 698, task 941,
//! ADR-t1479-1). `[areas]` maps the
//! areas `stats` and `kpi` split the landed runs by to globs (ADR-t980-1),
//! and `[tasks] changes` names the set of changes a task declares one of.
//! `[goals] tags` names the set of tags a goal's tags are taken from
//! (ADR-t1639-1 decision 6).
//! `[e2e] paths` names, as globs, the paths whose change requires `e2e` of
//! a run (ADR-t963-1 decision 2). `[broker]` holds the resource broker's
//! mode and the limits of its server (ADR-t827-4 decision 4).
//! `[headless] wrapper` chooses where a headless session's wrapper runs:
//! in a cmux workspace or as a background process (ADR-t1404-1).
//! `[provider_fallback] workers` turns off a worker's move off a provider
//! it cannot use, and `jobs` that of a headless job whose role names its
//! provider (ADR-t1857-1).
//! The file is parsed by
//! hand: the format is these tables of `KEY = value` lines, a subset of
//! TOML that needs no parser crate.
use anyhow::{Context, Result, bail, ensure};
use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    application::{Exit, Verifier},
    domain::{
        ChangeSet, GoalTag, TagSet, TaskChange,
        actor_model::{ModelRole, RoleModel, RoleModels, check_effort},
        areas::AreaMap,
        background_wrapper::HeadlessWrapper,
        broker::{BrokerConfig, BrokerMode},
        ci_watch::{CiWatchConfig, DEFAULT_INTERVAL_SECS, MIN_INTERVAL_SECS},
        disk::DiskConfig,
        exit::ExitConfig,
        kpi::KpiSettings,
        landing_branch::RepositoryConfig,
        light_slots::LightChanges,
        provider_switch::ProviderFallback,
        resume::ResumeConfig,
        review_subagents::{self, ReviewSubagent},
        run_env::{RunEnvCheck, RunEnvProgram},
        scope::{dedup_globs, validate_path_globs},
        slot_limits::SupervisorConfig,
        stall::StallConfig,
        stats::ConflictConfig,
        worker_model::WorkerTrial,
    },
    infrastructure::kpi_config::KpiTables,
};

pub const CONFIG_FILE_NAME: &str = "dagq.toml";
pub const QUEUE_DIR_VAR: &str = "DAGQ_QUEUE_DIR";
pub const RUN_DIR_VAR: &str = "DAGQ_RUN_DIR";
const RUN_ENV_TABLE: &str = "run.env";
const STALL_TABLE: &str = "stall";
const CONFLICTS_TABLE: &str = "conflicts";
const RECHECK_TABLE: &str = "recheck";
const DISK_TABLE: &str = "disk";
const RESUME_TABLE: &str = "resume";
const EXIT_TABLE: &str = "exit";
/// `[repository]`: the landing branch and its push (ADR-t615-1).
const REPOSITORY_TABLE: &str = "repository";
/// The keys of `[repository]`.
const REPOSITORY_BRANCH: &str = "branch";
const REPOSITORY_REMOTE: &str = "remote";
const REPOSITORY_PUSH: &str = "push";
/// `[kpi]` and its targets (ADR-0051), read by [`KpiTables`].
const KPI_TABLE: &str = "kpi";
/// `[worker.trial]`: the limited trial of the worker's model (ADR-0079
/// decision 4).
const WORKER_TRIAL_TABLE: &str = "worker.trial";
/// `[roles.<role>]`: the provider, model and effort of a role other than
/// the worker (ADR-0079 decision 7, ADR-t1063-1), one table per role.
const ROLES_PREFIX: &str = "roles.";
/// What [`parse_config`] calls the current table while in a `[roles.*]`.
const ROLES_TABLE: &str = "roles";
/// `[language]` (ADR-t616-2): accepted here without looking into it;
/// [`super::language`] reads and checks it, so a mistake in it never stops
/// a claim or a landing.
const LANGUAGE_TABLE: &str = "language";
/// `[supervisor]`: `parallel` and `max_waiting` (task 698),
/// `runtime_planners` (task 941) and `claim_spacing` (ADR-t1479-1).
const SUPERVISOR_TABLE: &str = "supervisor";
/// `[areas]`: each area's name and its globs (ADR-t980-1).
const AREAS_TABLE: &str = "areas";
/// `[tasks]`: `changes`, the repository's set of changes (ADR-t980-1).
const TASKS_TABLE: &str = "tasks";
/// The one key of `[tasks]`.
const TASKS_CHANGES: &str = "changes";
/// `[goals]`: `tags`, the repository's set of goal tags (ADR-t1639-1
/// decision 6).
const GOALS_TABLE: &str = "goals";
/// The one key of `[goals]`.
const GOALS_TAGS: &str = "tags";
/// `[e2e]`: `paths`, the globs whose change requires `e2e` of a run
/// (ADR-t963-1 decision 2).
const E2E_TABLE: &str = "e2e";
/// The one key of `[e2e]`.
const E2E_PATHS: &str = "paths";
/// `[broker]`: the resource broker's mode and limits (ADR-t827-4
/// decision 4).
const BROKER_TABLE: &str = "broker";
/// `[broker.package]`: the commands `package.install` may run, each a name
/// and its argv (task 840).
const BROKER_PACKAGE_TABLE: &str = "broker.package";
/// `[review.subagents.<agent>]`: the globs that make a review's subagent
/// required (ADR-t1453-1 decision 1), one table per agent.
const REVIEW_SUBAGENTS_PREFIX: &str = "review.subagents.";
/// What [`parse_config`] calls the current table while in a
/// `[review.subagents.*]`.
const REVIEW_SUBAGENTS_TABLE: &str = "review.subagents";
/// The one key of `[review.subagents.<agent>]`.
const REVIEW_SUBAGENT_PATHS: &str = "paths";
/// `[headless]`: `wrapper`, where a headless session's wrapper runs
/// (ADR-t1404-1 decision 7).
const HEADLESS_TABLE: &str = "headless";
/// The one key of `[headless]`.
const HEADLESS_WRAPPER: &str = "wrapper";
/// `[provider_fallback]`: whether a worker and a job move off a provider
/// they cannot use (ADR-t1857-1).
const PROVIDER_FALLBACK_TABLE: &str = "provider_fallback";
/// `[ci_watch]`: the landing branch's CI the supervisor watches
/// (ADR-t1920-1).
const CI_WATCH_TABLE: &str = "ci_watch";
const TABLES: [&str; 20] = [
    RUN_ENV_TABLE,
    STALL_TABLE,
    CONFLICTS_TABLE,
    RECHECK_TABLE,
    DISK_TABLE,
    RESUME_TABLE,
    EXIT_TABLE,
    REPOSITORY_TABLE,
    WORKER_TRIAL_TABLE,
    LANGUAGE_TABLE,
    SUPERVISOR_TABLE,
    AREAS_TABLE,
    TASKS_TABLE,
    GOALS_TABLE,
    E2E_TABLE,
    BROKER_TABLE,
    BROKER_PACKAGE_TABLE,
    HEADLESS_TABLE,
    PROVIDER_FALLBACK_TABLE,
    CI_WATCH_TABLE,
];
/// The one key of `[recheck]`.
const RECHECK_COMMAND: &str = "command";
/// Names the runtime itself sets on a workspace (`DAGQ_ROLE`, `DAGQ_QUEUE`)
/// and may set later; `[run.env]` cannot override them.
const RESERVED_PREFIX: &str = "DAGQ_";

/// The variables of `[run.env]` cargo executes as a program (ADR-0049
/// decision 9): `up`, the supervisor, `integrate` and `doctor` check that
/// each one's value resolves. A tool other than cargo's is not inferred;
/// naming one needs an ADR that adds a table declaring it.
pub const PROGRAM_VARIABLES: [&str; 8] = [
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTC",
    "RUSTDOC",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC",
    "CARGO_BUILD_RUSTDOC",
];

/// `[run.env]` as written, in file order, values unexpanded.
pub fn parse_run_env(text: &str) -> Result<Vec<(String, String)>> {
    Ok(parse_config(text)?.run_env)
}

/// What the file holds: `[run.env]`, `[stall]` (ADR-0043 decision 4),
/// `[conflicts]`, `[recheck]` (ADR-0068 decision 2), `[disk]` (ADR-0047
/// decision 44), `[resume]` (ADR-0047 decision 24), `[exit]` (ADR-0047
/// decision 25), `[kpi]` (ADR-0051 decisions 17 and 19), and the tables
/// each field below names, `[tasks] changes` and `[goals] tags` among them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    /// `[run.env]` as written, in file order, values unexpanded.
    pub run_env: Vec<(String, String)>,
    /// `[stall]`, the defaults for the keys it does not set.
    pub stall: StallConfig,
    /// `[conflicts]`, the defaults for the keys it does not set.
    pub conflicts: ConflictConfig,
    /// `[recheck] command`; none checks the merge only.
    pub recheck_command: Option<String>,
    /// `[disk]`, the defaults for the keys it does not set.
    pub disk: DiskConfig,
    /// `[resume]`, the default for the key it does not set.
    pub resume: ResumeConfig,
    /// `[exit]`, the defaults for the keys it does not set: checked and
    /// otherwise ignored since task 1437 (no `/exit` is retried).
    pub exit: ExitConfig,
    /// `[kpi]` and its `[kpi.targets."<kpi>"]`; `None` without any.
    pub kpi: Option<KpiSettings>,
    /// `[repository]`: the landing branch, the push remote and whether
    /// to push (ADR-t615-1).
    pub repository: RepositoryConfig,
    /// `[worker.trial]`, off unless it says `enabled = true` (ADR-0079
    /// decision 4).
    pub worker_trial: WorkerTrial,
    /// `[roles.<role>]`, none by default (ADR-0079 decision 7).
    pub roles: RoleModels,
    /// `[supervisor]`, each key it sets (task 698).
    pub supervisor: SupervisorConfig,
    /// `[areas]` (ADR-t980-1); `None` without the table.
    pub areas: Option<AreaMap>,
    /// `[tasks] changes` (ADR-t980-1); `None` without it.
    pub changes: Option<ChangeSet>,
    /// `[goals] tags` (ADR-t1639-1 decision 6); `None` without it.
    pub goal_tags: Option<TagSet>,
    /// `[e2e] paths` (ADR-t963-1 decision 2); empty without it.
    pub e2e_paths: Vec<String>,
    /// `[broker]` (ADR-t827-4 decision 4), the defaults (mode `disabled`)
    /// for the keys it does not set.
    pub broker: BrokerConfig,
    /// `[review.subagents.<agent>]` in file order (ADR-t1453-1 decision
    /// 1); empty without any.
    pub review_subagents: Vec<ReviewSubagent>,
    /// `[headless] wrapper` (ADR-t1404-1 decision 7); `None` without it,
    /// which is the default, a workspace.
    pub headless_wrapper: Option<HeadlessWrapper>,
    /// `[provider_fallback]` (ADR-t1857-1), the default (on) for the keys
    /// it does not set.
    pub provider_fallback: ProviderFallback,
    /// `[ci_watch]` (ADR-t1920-1); `None` without the table, which watches
    /// nothing.
    pub ci_watch: Option<CiWatchConfig>,
}

/// Parse the whole file.
pub fn parse_config(text: &str) -> Result<Config> {
    let mut table: Option<&str> = None;
    let mut seen: Vec<&str> = Vec::new();
    let mut config = Config::default();
    let mut stall_keys: Vec<String> = Vec::new();
    let mut conflict_keys: Vec<String> = Vec::new();
    let mut disk_keys: Vec<String> = Vec::new();
    let mut resume_keys: Vec<String> = Vec::new();
    let mut exit_keys: Vec<String> = Vec::new();
    let mut trial_keys: Vec<String> = Vec::new();
    let mut supervisor_keys: Vec<String> = Vec::new();
    // `[supervisor] light_changes` and its line, checked against `[tasks]
    // changes` once the file is read whole (ADR-t1591-1).
    let mut light_changes: Option<(Vec<TaskChange>, usize)> = None;
    let mut broker_keys: Vec<String> = Vec::new();
    let mut fallback_keys: Vec<String> = Vec::new();
    // `[ci_watch]`'s header line and the keys read, the table checked once
    // the file is read whole (its `workflow` is required).
    let mut ci_watch: Option<(usize, CiWatchConfig)> = None;
    let mut ci_watch_keys: Vec<String> = Vec::new();
    let mut role: Option<ModelRole> = None;
    let mut roles_seen: Vec<ModelRole> = Vec::new();
    let mut role_keys: Vec<String> = Vec::new();
    let mut kpi = KpiTables::default();
    let mut areas: Option<Vec<(String, Vec<String>)>> = None;
    let mut e2e_paths_seen: Option<usize> = None;
    // The line of the `[review.subagents.<agent>]` header whose `paths`
    // is not read yet.
    let mut subagent_open: Option<usize> = None;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = strip_comment(header)
                .strip_suffix(']')
                .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: unclosed table header"))?
                .trim();
            // Any next table ends a `[review.subagents.<agent>]`, which
            // must have had its paths.
            if let Some(line) = subagent_open.take() {
                bail!(
                    "{CONFIG_FILE_NAME}:{line}: [{REVIEW_SUBAGENTS_PREFIX}{}] has no {REVIEW_SUBAGENT_PATHS}",
                    config.review_subagents.last().expect("an open agent").name
                );
            }
            if kpi
                .header(name)
                .with_context(|| format!("{CONFIG_FILE_NAME}:{number}"))?
            {
                table = Some(KPI_TABLE);
                continue;
            }
            if let Some(name) = name.strip_prefix(ROLES_PREFIX) {
                let parsed: ModelRole = name.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "{CONFIG_FILE_NAME}:{number}: unknown role [{ROLES_PREFIX}{name}]; the roles are {}",
                        ModelRole::ALL.map(ModelRole::as_str).join(", ")
                    )
                })?;
                ensure!(
                    !roles_seen.contains(&parsed),
                    "{CONFIG_FILE_NAME}:{number}: [{ROLES_PREFIX}{name}] is defined twice"
                );
                roles_seen.push(parsed);
                role = Some(parsed);
                role_keys.clear();
                table = Some(ROLES_TABLE);
                continue;
            }
            if name == REVIEW_SUBAGENTS_TABLE || name.starts_with(REVIEW_SUBAGENTS_PREFIX) {
                let agent = name
                    .strip_prefix(REVIEW_SUBAGENTS_PREFIX)
                    .map(|agent| parse_key(agent.trim()))
                    .transpose()
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}"))?
                    .unwrap_or_default();
                ensure!(
                    !agent.is_empty(),
                    "{CONFIG_FILE_NAME}:{number}: [{name}] names no agent; write [{REVIEW_SUBAGENTS_PREFIX}<agent>]"
                );
                ensure!(
                    review_subagents::valid_agent_name(&agent),
                    "{CONFIG_FILE_NAME}:{number}: agent {agent:?} of [{name}] is not kebab-case (lowercase letters and digits joined by -)"
                );
                ensure!(
                    config.review_subagents.iter().all(|a| a.name != agent),
                    "{CONFIG_FILE_NAME}:{number}: [{name}] is defined twice"
                );
                config.review_subagents.push(ReviewSubagent {
                    name: agent,
                    paths: Vec::new(),
                });
                subagent_open = Some(number);
                table = Some(REVIEW_SUBAGENTS_TABLE);
                continue;
            }
            let known = TABLES.iter().find(|table| **table == name).with_context(|| {
                format!(
                    "{CONFIG_FILE_NAME}:{number}: unknown table [{name}]; only [{RUN_ENV_TABLE}], [{STALL_TABLE}], [{CONFLICTS_TABLE}], [{RECHECK_TABLE}], [{DISK_TABLE}], [{RESUME_TABLE}], [{EXIT_TABLE}], [{REPOSITORY_TABLE}], [{WORKER_TRIAL_TABLE}], [{ROLES_PREFIX}<role>], [{REVIEW_SUBAGENTS_PREFIX}<agent>], [{LANGUAGE_TABLE}], [{SUPERVISOR_TABLE}], [{AREAS_TABLE}], [{TASKS_TABLE}], [{GOALS_TABLE}], [{E2E_TABLE}], [{BROKER_TABLE}], [{BROKER_PACKAGE_TABLE}], [{HEADLESS_TABLE}], [{PROVIDER_FALLBACK_TABLE}], [{CI_WATCH_TABLE}] and [{KPI_TABLE}] are supported"
                )
            })?;
            ensure!(
                *known == LANGUAGE_TABLE || !seen.contains(known),
                "{CONFIG_FILE_NAME}:{number}: [{name}] is defined twice"
            );
            seen.push(known);
            table = Some(known);
            if *known == CI_WATCH_TABLE {
                ci_watch = Some((
                    number,
                    CiWatchConfig {
                        workflow: String::new(),
                        branch: None,
                        interval_secs: DEFAULT_INTERVAL_SECS,
                        junit_artifacts: Vec::new(),
                    },
                ));
            }
            if *known == AREAS_TABLE {
                areas.get_or_insert_with(Vec::new);
            }
            continue;
        }
        if table == Some(LANGUAGE_TABLE) {
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: expected KEY = value"))?;
        let key = key.trim();
        match table {
            Some(REVIEW_SUBAGENTS_TABLE) => {
                let agent = config
                    .review_subagents
                    .last_mut()
                    .expect("a [review.subagents.*] table names its agent");
                ensure!(
                    key == REVIEW_SUBAGENT_PATHS,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{REVIEW_SUBAGENTS_PREFIX}{}]; the key is {REVIEW_SUBAGENT_PATHS}",
                    agent.name
                );
                ensure!(
                    subagent_open.take().is_some(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let globs = parse_string_array(rest.trim()).with_context(with)?;
                ensure!(
                    !globs.is_empty(),
                    "{CONFIG_FILE_NAME}:{number}: {key} of [{REVIEW_SUBAGENTS_PREFIX}{}] names no glob",
                    agent.name
                );
                validate_path_globs(&globs).with_context(with)?;
                agent.paths = dedup_globs(&globs);
            }
            Some(AREAS_TABLE) => {
                let with = || format!("{CONFIG_FILE_NAME}:{number}");
                let name = parse_key(key).with_context(with)?;
                let globs = parse_string_array(rest.trim())
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {name}"))?;
                areas.get_or_insert_with(Vec::new).push((name, globs));
            }
            Some(TASKS_TABLE) => {
                ensure!(
                    key == TASKS_CHANGES,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{TASKS_TABLE}]; the key is {TASKS_CHANGES}"
                );
                ensure!(
                    config.changes.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let changes = parse_string_array(rest.trim())
                    .with_context(with)?
                    .iter()
                    .map(|change| change.parse::<TaskChange>())
                    .collect::<Result<Vec<_>, _>>()
                    .with_context(with)?;
                config.changes = Some(
                    ChangeSet::new(changes)
                        .map_err(anyhow::Error::msg)
                        .with_context(with)?,
                );
            }
            Some(GOALS_TABLE) => {
                ensure!(
                    key == GOALS_TAGS,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{GOALS_TABLE}]; the key is {GOALS_TAGS}"
                );
                ensure!(
                    config.goal_tags.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let tags = parse_string_array(rest.trim())
                    .with_context(with)?
                    .iter()
                    .map(|tag| tag.parse::<GoalTag>())
                    .collect::<Result<Vec<_>, _>>()
                    .with_context(with)?;
                config.goal_tags = Some(
                    TagSet::new(tags)
                        .map_err(anyhow::Error::msg)
                        .with_context(with)?,
                );
            }
            Some(BROKER_TABLE) => {
                ensure!(
                    BrokerConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{BROKER_TABLE}]; the keys are {}",
                    BrokerConfig::KEYS.join(", ")
                );
                ensure!(
                    !broker_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let value = rest.trim();
                let broker = &mut config.broker;
                match key {
                    "mode" => {
                        let text = parse_string(value).with_context(with)?;
                        broker.mode = BrokerMode::parse(&text)
                            .with_context(|| {
                                format!(
                                    "expected \"disabled\", \"preferred\" or \"required\", not {text:?}"
                                )
                            })
                            .with_context(with)?;
                    }
                    "exec_allow" => {
                        broker.exec_allow = parse_string_array(value).with_context(with)?
                    }
                    "exec_env" => broker.exec_env = parse_string_array(value).with_context(with)?,
                    _ => {
                        let number = parse_positive(value, "number")
                            .with_context(with)?
                            .unsigned_abs();
                        match key {
                            "exec_timeout_secs" => broker.exec_timeout_secs = number,
                            "exec_max_timeout_secs" => broker.exec_max_timeout_secs = number,
                            "output_limit_bytes" => broker.output_limit_bytes = number,
                            _ => broker.fs_limit_bytes = number,
                        }
                    }
                }
                broker_keys.push(key.to_owned());
            }
            Some(BROKER_PACKAGE_TABLE) => {
                let with = || format!("{CONFIG_FILE_NAME}:{number}");
                let name = parse_key(key).with_context(with)?;
                ensure!(
                    config
                        .broker
                        .packages
                        .iter()
                        .all(|(existing, _)| *existing != name),
                    "{CONFIG_FILE_NAME}:{number}: {name} is defined twice"
                );
                let argv = parse_string_array(rest.trim())
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {name}"))?;
                dagq_broker_protocol::package::check_command(&name, &argv)
                    .map_err(anyhow::Error::msg)
                    .with_context(with)?;
                config.broker.packages.push((name, argv));
            }
            Some(E2E_TABLE) => {
                ensure!(
                    key == E2E_PATHS,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{E2E_TABLE}]; the key is {E2E_PATHS}"
                );
                ensure!(
                    e2e_paths_seen.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                e2e_paths_seen = Some(number);
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let globs = parse_string_array(rest.trim()).with_context(with)?;
                validate_path_globs(&globs).with_context(with)?;
                config.e2e_paths = dedup_globs(&globs);
            }
            Some(KPI_TABLE) => kpi
                .entry(key, rest.trim())
                .with_context(|| format!("{CONFIG_FILE_NAME}:{number}"))?,
            Some(RUN_ENV_TABLE) => {
                ensure!(
                    is_env_name(key),
                    "{CONFIG_FILE_NAME}:{number}: {key:?} is not an environment variable name"
                );
                ensure!(
                    !key.starts_with(RESERVED_PREFIX),
                    "{CONFIG_FILE_NAME}:{number}: {key} uses the reserved prefix {RESERVED_PREFIX}"
                );
                ensure!(
                    config.run_env.iter().all(|(existing, _)| existing != key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let value = parse_string(rest.trim())
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                config.run_env.push((key.to_owned(), value));
            }
            Some(HEADLESS_TABLE) => {
                ensure!(
                    key == HEADLESS_WRAPPER,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{HEADLESS_TABLE}]; the key is {HEADLESS_WRAPPER}"
                );
                ensure!(
                    config.headless_wrapper.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let text = parse_string(rest.trim()).with_context(with)?;
                let wrapper = HeadlessWrapper::parse(&text)
                    .with_context(|| {
                        format!("expected \"workspace\" or \"background\", not {text:?}")
                    })
                    .with_context(with)?;
                config.headless_wrapper = Some(wrapper);
            }
            Some(PROVIDER_FALLBACK_TABLE) => {
                ensure!(
                    ProviderFallback::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{PROVIDER_FALLBACK_TABLE}]; the keys are {}",
                    ProviderFallback::KEYS.join(", ")
                );
                ensure!(
                    !fallback_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let on = parse_bool(rest.trim())
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                if key == "jobs" {
                    config.provider_fallback.jobs = on;
                } else {
                    config.provider_fallback.workers = on;
                }
                fallback_keys.push(key.to_owned());
            }
            Some(CI_WATCH_TABLE) => {
                ensure!(
                    CiWatchConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{CI_WATCH_TABLE}]; the keys are {}",
                    CiWatchConfig::KEYS.join(", ")
                );
                ensure!(
                    !ci_watch_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                ci_watch_keys.push(key.to_owned());
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let config = &mut ci_watch.as_mut().expect("[ci_watch] was opened").1;
                match key {
                    "interval_secs" => {
                        let secs = parse_positive(rest.trim(), "number of seconds")
                            .with_context(with)?
                            .unsigned_abs();
                        ensure!(
                            secs >= MIN_INTERVAL_SECS,
                            "{CONFIG_FILE_NAME}:{number}: {key} must be at least {MIN_INTERVAL_SECS}, not {secs}"
                        );
                        config.interval_secs = secs;
                    }
                    "junit_artifacts" => {
                        let globs = parse_string_array(rest.trim()).with_context(with)?;
                        for (index, glob) in globs.iter().enumerate() {
                            ensure!(
                                !glob.trim().is_empty(),
                                "{CONFIG_FILE_NAME}:{number}: {key} has an empty glob"
                            );
                            ensure!(
                                !globs[..index].contains(glob),
                                "{CONFIG_FILE_NAME}:{number}: {key} names {glob:?} twice"
                            );
                        }
                        config.junit_artifacts = globs;
                    }
                    _ => {
                        let value = parse_string(rest.trim()).with_context(with)?;
                        ensure!(
                            !value.trim().is_empty(),
                            "{CONFIG_FILE_NAME}:{number}: {key} is blank"
                        );
                        if key == "workflow" {
                            config.workflow = value;
                        } else {
                            ensure!(
                                !value.starts_with("refs/"),
                                "{CONFIG_FILE_NAME}:{number}: {key} is a branch name without refs/heads/, not {value}"
                            );
                            config.branch = Some(value);
                        }
                    }
                }
            }
            Some(RECHECK_TABLE) => {
                ensure!(
                    key == RECHECK_COMMAND,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{RECHECK_TABLE}]; the key is {RECHECK_COMMAND}"
                );
                ensure!(
                    config.recheck_command.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let command = parse_string(rest.trim())
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                ensure!(
                    !command.trim().is_empty(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is blank"
                );
                config.recheck_command = Some(command);
            }
            Some(REPOSITORY_TABLE) => {
                let repository = &mut config.repository;
                let defined = match key {
                    REPOSITORY_BRANCH => repository.branch.is_some(),
                    REPOSITORY_REMOTE => repository.remote.is_some(),
                    REPOSITORY_PUSH => repository.push.is_some(),
                    _ => bail!(
                        "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{REPOSITORY_TABLE}]; the keys are {REPOSITORY_BRANCH}, {REPOSITORY_REMOTE} and {REPOSITORY_PUSH}"
                    ),
                };
                ensure!(
                    !defined,
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                if key == REPOSITORY_PUSH {
                    repository.push =
                        Some(parse_bool(rest.trim()).with_context(|| {
                            format!("{CONFIG_FILE_NAME}:{number}: value of {key}")
                        })?);
                    continue;
                }
                let value = parse_string(rest.trim())
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                ensure!(
                    !value.trim().is_empty(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is blank"
                );
                if key == REPOSITORY_REMOTE {
                    repository.remote = Some(value);
                    continue;
                }
                ensure!(
                    !value.starts_with("refs/"),
                    "{CONFIG_FILE_NAME}:{number}: {key} is a branch name without refs/heads/, not {value}"
                );
                repository.branch = Some(value);
            }
            Some(ROLES_TABLE) => {
                let role = role.expect("a [roles.*] table names its role");
                ensure!(
                    RoleModel::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{ROLES_PREFIX}{}]; the keys are {}",
                    role.as_str(),
                    RoleModel::KEYS.join(", ")
                );
                ensure!(
                    !role_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                role_keys.push(key.to_owned());
                let table = config.roles.entry(role);
                if key == crate::domain::actor_model::ROUTE_KEY {
                    // The route of the runtime's planners is not chosen
                    // any more: they run headless only, and the key is
                    // accepted, whatever its value (a string of any
                    // content, a blank or another value), and ignored
                    // (ADR-t1433-2 decision 3). Only [roles.runtime_planner]
                    // ever took it.
                    ensure!(
                        role == crate::domain::actor_model::ModelRole::RuntimePlanner,
                        "{CONFIG_FILE_NAME}:{number}: {key} is a key of [{ROLES_PREFIX}{}] only (and ignored there); the {} role has no route",
                        crate::domain::actor_model::ModelRole::RuntimePlanner.as_str(),
                        role.as_str()
                    );
                    // Kept only for the supervisor's warning that it is
                    // ignored (ADR-t1433-3 decision 2's handling).
                    let raw = rest.trim();
                    config
                        .roles
                        .ignore_planner_route(parse_string(raw).unwrap_or_else(|_| raw.to_owned()));
                    continue;
                }
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let value = parse_string(rest.trim()).with_context(with)?;
                ensure!(
                    !value.trim().is_empty(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is blank"
                );
                if key == "provider" {
                    let provider = value
                        .parse::<crate::domain::Provider>()
                        .map_err(|error| anyhow::anyhow!("{error}"))
                        .with_context(with)?;
                    table.provider = Some(provider);
                } else if key == "model" {
                    table.model = Some(value);
                } else {
                    check_effort(&value)
                        .map_err(|error| anyhow::anyhow!("{error}"))
                        .with_context(with)?;
                    table.effort = Some(value);
                }
            }
            Some(WORKER_TRIAL_TABLE) => {
                ensure!(
                    WorkerTrial::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{WORKER_TRIAL_TABLE}]; the keys are {}",
                    WorkerTrial::KEYS.join(", ")
                );
                ensure!(
                    !trial_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let value = rest.trim();
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                if key == "enabled" {
                    config.worker_trial.enabled = parse_bool(value).with_context(with)?;
                } else {
                    let window = parse_positive(value, "number").with_context(with)?;
                    config.worker_trial.window = usize::try_from(window).with_context(with)?;
                }
                trial_keys.push(key.to_owned());
            }
            Some(SUPERVISOR_TABLE) => {
                ensure!(
                    SupervisorConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{SUPERVISOR_TABLE}]; the keys are {}",
                    SupervisorConfig::KEYS.join(", ")
                );
                ensure!(
                    !supervisor_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                if key == "parallel" {
                    let parallel = parse_positive(rest.trim(), "number").with_context(with)?;
                    config.supervisor.parallel =
                        Some(u16::try_from(parallel).with_context(with)?.into());
                } else if key == "runtime_planners" {
                    let planners = parse_positive(rest.trim(), "number").with_context(with)?;
                    config.supervisor.runtime_planners =
                        Some(u16::try_from(planners).with_context(with)?.into());
                } else if key == "claim_spacing" {
                    let secs = parse_whole(rest.trim()).with_context(with)?;
                    let secs = u32::try_from(secs).with_context(with)?;
                    config.supervisor.claim_spacing = Some(usize::try_from(secs)?);
                } else if key == "light_changes" {
                    let changes = parse_string_array(rest.trim())
                        .with_context(with)?
                        .iter()
                        .map(|change| change.parse::<TaskChange>())
                        .collect::<Result<Vec<_>, _>>()
                        .with_context(with)?;
                    light_changes = Some((changes, number));
                } else {
                    let limit = parse_whole(rest.trim()).with_context(with)?;
                    config.supervisor.max_waiting =
                        Some(u16::try_from(limit).with_context(with)?.into());
                }
                supervisor_keys.push(key.to_owned());
            }
            Some(DISK_TABLE) => {
                ensure!(
                    DiskConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{DISK_TABLE}]; the keys are {}",
                    DiskConfig::KEYS.join(", ")
                );
                ensure!(
                    !disk_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                if DiskConfig::FACTORS.contains(&key) {
                    let value = parse_positive_number(rest.trim())
                        .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                    config.disk.set_factor(key, value);
                } else {
                    let value = parse_positive(rest.trim(), "number")
                        .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                    config.disk.set_whole(key, value);
                }
                disk_keys.push(key.to_owned());
            }
            Some(RESUME_TABLE) => {
                ensure!(
                    ResumeConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{RESUME_TABLE}]; the keys are {}",
                    ResumeConfig::KEYS.join(", ")
                );
                ensure!(
                    !resume_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let limit = parse_positive(rest.trim(), "number").with_context(with)?;
                config.resume.conflict_only_limit = usize::try_from(limit).with_context(with)?;
                resume_keys.push(key.to_owned());
            }
            Some(EXIT_TABLE) => {
                ensure!(
                    ExitConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{EXIT_TABLE}]; the keys are {}",
                    ExitConfig::KEYS.join(", ")
                );
                ensure!(
                    !exit_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                if key == "retries" {
                    let retries = parse_whole(rest.trim()).with_context(with)?;
                    config.exit.retries = usize::try_from(retries).with_context(with)?;
                } else {
                    config.exit.intervals = parse_seconds_list(rest.trim())
                        .with_context(with)?
                        .into_iter()
                        .map(Duration::from_secs)
                        .collect();
                }
                exit_keys.push(key.to_owned());
            }
            Some(CONFLICTS_TABLE) => {
                ensure!(
                    ConflictConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{CONFLICTS_TABLE}]; the keys are {}",
                    ConflictConfig::KEYS.join(", ")
                );
                ensure!(
                    !conflict_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let value = parse_positive(rest.trim(), "number")
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                config.conflicts.set(key, value);
                conflict_keys.push(key.to_owned());
            }
            Some(_) => {
                ensure!(
                    StallConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{STALL_TABLE}]; the keys are {}",
                    StallConfig::KEYS.join(", ")
                );
                ensure!(
                    !stall_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let secs = parse_positive(rest.trim(), "number of seconds")
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                config.stall.set(key, secs);
                stall_keys.push(key.to_owned());
            }
            None => bail!(
                "{CONFIG_FILE_NAME}:{number}: a key outside [{RUN_ENV_TABLE}], [{STALL_TABLE}], [{CONFLICTS_TABLE}], [{RECHECK_TABLE}], [{DISK_TABLE}], [{RESUME_TABLE}], [{EXIT_TABLE}], [{REPOSITORY_TABLE}], [{WORKER_TRIAL_TABLE}], [{ROLES_PREFIX}<role>], [{SUPERVISOR_TABLE}], [{AREAS_TABLE}], [{TASKS_TABLE}], [{GOALS_TABLE}], [{BROKER_TABLE}], [{BROKER_PACKAGE_TABLE}], [{HEADLESS_TABLE}], [{PROVIDER_FALLBACK_TABLE}], [{CI_WATCH_TABLE}] or [{KPI_TABLE}]"
            ),
        }
    }
    if let Some(line) = subagent_open {
        bail!(
            "{CONFIG_FILE_NAME}:{line}: [{REVIEW_SUBAGENTS_PREFIX}{}] has no {REVIEW_SUBAGENT_PATHS}",
            config.review_subagents.last().expect("an open agent").name
        );
    }
    if let Some((line, watch)) = ci_watch {
        ensure!(
            !watch.workflow.is_empty(),
            "{CONFIG_FILE_NAME}:{line}: [{CI_WATCH_TABLE}] has no workflow"
        );
        config.ci_watch = Some(watch);
    }
    config.kpi = kpi.finish().with_context(|| CONFIG_FILE_NAME.to_owned())?;
    // A provider is checked against its role once the table is read whole
    // (ADR-t1063-1 decision 1).
    config
        .roles
        .check()
        .map_err(anyhow::Error::msg)
        .with_context(|| CONFIG_FILE_NAME.to_owned())?;
    config
        .broker
        .check()
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("{CONFIG_FILE_NAME}: [{BROKER_TABLE}]"))?;
    config.areas = areas
        .map(|areas| AreaMap::new(areas).map_err(anyhow::Error::msg))
        .transpose()
        .with_context(|| format!("{CONFIG_FILE_NAME}: [{AREAS_TABLE}]"))?;
    if let Some((changes, line)) = light_changes {
        config.supervisor.light_changes = Some(
            LightChanges::new(changes, config.changes.as_ref())
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("{CONFIG_FILE_NAME}:{line}: value of light_changes"))?,
        );
    }
    Ok(config)
}

/// A key as written: bare, or a TOML string (`"src-domain"`).
fn parse_key(key: &str) -> Result<String> {
    if key.starts_with(['"', '\'']) {
        parse_string(key)
    } else {
        Ok(key.to_owned())
    }
}

/// A one-line array of TOML strings (`["src/**", 'docs/**']`), followed by
/// nothing but an optional comment.
fn parse_string_array(text: &str) -> Result<Vec<String>> {
    let mut rest = text
        .strip_prefix('[')
        .with_context(|| format!("expected an array of strings like [\"src/**\"], not {text}"))?
        .trim_start();
    let mut values = Vec::new();
    loop {
        if let Some(after) = rest.strip_prefix(']') {
            let after = after.trim();
            ensure!(
                after.is_empty() || after.starts_with('#'),
                "unexpected text after the array: {after}"
            );
            return Ok(values);
        }
        let quote = rest.chars().next().context("unterminated array")?;
        ensure!(
            quote == '"' || quote == '\'',
            "expected a quoted string in the array, not {rest}"
        );
        // A basic string's `\"` does not end it.
        let mut escaped = false;
        let end = rest
            .char_indices()
            .skip(1)
            .find(|&(_, c)| {
                let ends = c == quote && !escaped;
                escaped = quote == '"' && c == '\\' && !escaped;
                ends
            })
            .context("unterminated string in the array")?
            .0
            + 1;
        values.push(parse_string(&rest[..end])?);
        rest = rest[end..].trim_start();
        if let Some(after) = rest.strip_prefix(',') {
            rest = after.trim_start();
        } else {
            ensure!(!rest.is_empty(), "unterminated array");
            ensure!(
                rest.starts_with(']'),
                "expected , or ] in the array, not {rest}"
            );
        }
    }
}

/// A positive integer (a `what`), followed by nothing but an optional comment.
pub(super) fn parse_positive(text: &str, what: &str) -> Result<i64> {
    let digits = strip_comment(text);
    ensure!(!digits.is_empty(), "missing value");
    let value: i64 = digits
        .replace('_', "")
        .parse()
        .with_context(|| format!("expected a whole {what}, not {digits}"))?;
    ensure!(value > 0, "must be a positive {what}, not {value}");
    Ok(value)
}

/// A non-empty array of positive whole numbers of seconds (`[30, 60]`),
/// followed by nothing but an optional comment.
fn parse_seconds_list(text: &str) -> Result<Vec<u64>> {
    let list = strip_comment(text);
    ensure!(!list.is_empty(), "missing value");
    let inner = list
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .with_context(|| format!("expected an array of seconds like [30, 60], not {list}"))?;
    ensure!(!inner.trim().is_empty(), "the array is empty");
    let values = inner
        .split(',')
        .map(str::trim)
        .map(|item| {
            let secs = parse_positive(item, "number of seconds")?;
            u64::try_from(secs).context("seconds out of range")
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(!values.is_empty(), "the array is empty");
    Ok(values)
}

/// A whole number, 0 or more, followed by nothing but an optional comment.
fn parse_whole(text: &str) -> Result<i64> {
    let digits = strip_comment(text);
    ensure!(!digits.is_empty(), "missing value");
    let value: i64 = digits
        .replace('_', "")
        .parse()
        .with_context(|| format!("expected a whole number, not {digits}"))?;
    ensure!(value >= 0, "must be 0 or more, not {value}");
    Ok(value)
}

/// `true` or `false`, followed by nothing but an optional comment.
fn parse_bool(text: &str) -> Result<bool> {
    match strip_comment(text) {
        "true" => Ok(true),
        "false" => Ok(false),
        "" => bail!("missing value"),
        other => bail!("expected true or false, not {other}"),
    }
}

/// A positive number, whole or not (`1.5`), followed by nothing but an
/// optional comment.
fn parse_positive_number(text: &str) -> Result<f64> {
    let digits = strip_comment(text);
    ensure!(!digits.is_empty(), "missing value");
    let value: f64 = digits
        .replace('_', "")
        .parse()
        .with_context(|| format!("expected a number, not {digits}"))?;
    ensure!(
        value.is_finite() && value > 0.0,
        "must be a positive number, not {digits}"
    );
    Ok(value)
}

/// `[disk]` of the `dagq.toml` in `root` (ADR-0047 decision 44), `None`
/// when there is no file; no table or no key is the default.
pub fn load_disk_config(root: &Path) -> Result<Option<DiskConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .disk,
    ))
}

/// `[resume]` of the `dagq.toml` in `root` (ADR-0047 decision 24), `None`
/// when there is no file; no table or no key is the default.
pub fn load_resume_config(root: &Path) -> Result<Option<ResumeConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .resume,
    ))
}

/// `[stall]` of the `dagq.toml` in `root` (ADR-0043 decision 4), `None`
/// when there is no file; no table or no key is the default.
pub fn load_stall_config(root: &Path) -> Result<Option<StallConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .stall,
    ))
}

/// `[conflicts]` of the `dagq.toml` in `root`, `None` when there is no
/// file; no table or no key is the default.
pub fn load_conflict_config(root: &Path) -> Result<Option<ConflictConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .conflicts,
    ))
}

/// `[kpi]` of the `dagq.toml` in `root` (ADR-0051 decision 17), `None`
/// when there is no file or no `[kpi]` table.
pub fn load_kpi_settings(root: &Path) -> Result<Option<KpiSettings>> {
    let path = root.join(CONFIG_FILE_NAME);
    Ok(match read_config(&path)? {
        Some(text) => {
            parse_config(&text)
                .with_context(|| format!("parse {}", path.display()))?
                .kpi
        }
        None => None,
    })
}

/// `[recheck] command` of the `dagq.toml` in `root`; no file, no table or
/// no key is none.
pub fn load_recheck_command(root: &Path) -> Result<Option<String>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .recheck_command)
}

/// `[repository]` of the `dagq.toml` in `root` (ADR-t615-1); no file, no
/// table or no key is the default.
pub fn load_repository_config(root: &Path) -> Result<RepositoryConfig> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(RepositoryConfig::default());
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .repository)
}

/// `[supervisor]` of the `dagq.toml` in `root` (task 698), `None` when
/// there is no file; no table or no key sets nothing.
pub fn load_supervisor_config(root: &Path) -> Result<Option<SupervisorConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .supervisor,
    ))
}

/// `[provider_fallback]` of the `dagq.toml` in `root` (ADR-t1857-1),
/// `None` when there is no file; no table or no key is the default (on).
pub fn load_provider_fallback(root: &Path) -> Result<Option<ProviderFallback>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .provider_fallback,
    ))
}

/// `[ci_watch]` of the `dagq.toml` in `root` (ADR-t1920-1); no file or
/// no table is none, which watches nothing.
pub fn load_ci_watch(root: &Path) -> Result<Option<CiWatchConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .ci_watch)
}

/// `[broker]` of the `dagq.toml` in `root` (ADR-t827-4 decision 4); no
/// file, no table or no key is the default, mode `disabled`.
pub fn load_broker_config(root: &Path) -> Result<BrokerConfig> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(BrokerConfig::default());
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .broker)
}

/// `[areas]` of the `dagq.toml` in `root` (ADR-t980-1), `None` when there
/// is no file or no `[areas]` table.
pub fn load_area_map(root: &Path) -> Result<Option<AreaMap>> {
    let path = root.join(CONFIG_FILE_NAME);
    Ok(match read_config(&path)? {
        Some(text) => {
            parse_config(&text)
                .with_context(|| format!("parse {}", path.display()))?
                .areas
        }
        None => None,
    })
}

/// `[tasks] changes` of the `dagq.toml` in `root` (ADR-t980-1), `None`
/// when there is no file or no such key.
pub fn load_change_set(root: &Path) -> Result<Option<ChangeSet>> {
    let path = root.join(CONFIG_FILE_NAME);
    Ok(match read_config(&path)? {
        Some(text) => {
            parse_config(&text)
                .with_context(|| format!("parse {}", path.display()))?
                .changes
        }
        None => None,
    })
}

/// `[goals] tags` of the `dagq.toml` in `root` (ADR-t1639-1 decision 6),
/// `None` when there is no file or no such key.
pub fn load_goal_tags(root: &Path) -> Result<Option<TagSet>> {
    let path = root.join(CONFIG_FILE_NAME);
    Ok(match read_config(&path)? {
        Some(text) => {
            parse_config(&text)
                .with_context(|| format!("parse {}", path.display()))?
                .goal_tags
        }
        None => None,
    })
}

/// `[e2e] paths` of the `dagq.toml` in `root` (ADR-t963-1 decision 2);
/// no file, no table or no key is none.
pub fn load_e2e_paths(root: &Path) -> Result<Vec<String>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(Vec::new());
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .e2e_paths)
}

/// `[headless] wrapper` of the `dagq.toml` in `root` as written: `None`
/// for no file, no table or no key.
pub fn load_headless_wrapper_setting(root: &Path) -> Result<Option<HeadlessWrapper>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .headless_wrapper)
}

/// `[roles.<role>]` of the `dagq.toml` in `root` (ADR-0079 decision 7);
/// no file is no role's.
pub fn load_role_models(root: &Path) -> Result<RoleModels> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(RoleModels::default());
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .roles)
}

/// `[worker.trial]` of the `dagq.toml` in `root` (ADR-0079 decision 4);
/// no file, no table or no key is the default, which is off.
pub fn load_worker_trial(root: &Path) -> Result<WorkerTrial> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(WorkerTrial::default());
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .worker_trial)
}

fn read_config(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// `value` with `${DAGQ_QUEUE_DIR}` and `${DAGQ_RUN_DIR}` replaced. Any other
/// `$` text stays as written: the value is not a shell word.
pub fn expand(value: &str, queue_dir: &str, run_dir: &str) -> String {
    value
        .replace(&format!("${{{QUEUE_DIR_VAR}}}"), queue_dir)
        .replace(&format!("${{{RUN_DIR_VAR}}}"), run_dir)
}

/// The expanded `[run.env]` of the `dagq.toml` in `root`; no file is an
/// empty table.
pub fn load_run_env(
    root: &Path,
    queue_dir: &Path,
    run_dir: &Path,
) -> Result<Vec<(String, String)>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(Vec::new());
    };
    let queue_dir = path_str(queue_dir)?;
    let run_dir = path_str(run_dir)?;
    Ok(parse_run_env(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .into_iter()
        .map(|(key, value)| {
            let value = expand(&value, queue_dir, run_dir);
            (key, value)
        })
        .collect())
}

/// `[run.env]` of the `dagq.toml` in `root` as written, values unexpanded;
/// `None` without the file.
pub fn load_run_env_table(root: &Path) -> Result<Option<Vec<(String, String)>>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    parse_run_env(&text)
        .map(Some)
        .with_context(|| format!("parse {}", path.display()))
}

/// The file in the queue's directory holding its `[run.env]` hash salt.
pub const RUN_ENV_SALT_FILE: &str = "run-env-salt";

/// The salt in `<queue_dir>/run-env-salt`, made (random, owner-only) when
/// there is none. It is written to a temporary file and linked into place,
/// so the file is never seen empty, and two processes making it at once
/// keep the first one's. A lost file gets a new salt, and the next
/// `run_env_changed` names every key once.
pub fn run_env_salt(queue_dir: &Path) -> Result<String> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let path = queue_dir.join(RUN_ENV_SALT_FILE);
    let read = || -> Result<String> {
        let salt = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        ensure!(!salt.trim().is_empty(), "{} is empty", path.display());
        Ok(salt.trim().to_owned())
    };
    if path.exists() {
        return read();
    }
    let salt = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let temporary = queue_dir.join(format!(
        ".{RUN_ENV_SALT_FILE}.{}",
        uuid::Uuid::new_v4().simple()
    ));
    let made = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| file.write_all(salt.as_bytes()))
        .with_context(|| format!("write {}", temporary.display()))
        .and_then(|()| match fs::hard_link(&temporary, &path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error).with_context(|| format!("link {}", path.display())),
        });
    let _ = fs::remove_file(&temporary);
    if made? { Ok(salt) } else { read() }
}

/// Where `value` resolves as a program: a value with a `/` is that path, one
/// without is looked up in each directory of `path` in order, like a shell
/// does; either must be an executable file.
pub fn resolve_program(value: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if value.contains('/') {
        let candidate = PathBuf::from(value);
        return is_executable(&candidate).then_some(candidate);
    }
    env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(value))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Resolve every variable of `env` (expanded) that names a program
/// ([`PROGRAM_VARIABLES`]) in `path`. An empty value names none (cargo runs
/// no wrapper for an empty `RUSTC_WRAPPER`).
pub fn check_programs(env: &[(String, String)], path: Option<&OsStr>) -> RunEnvCheck {
    RunEnvCheck {
        config: true,
        path: path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        programs: env
            .iter()
            .filter(|(key, value)| PROGRAM_VARIABLES.contains(&key.as_str()) && !value.is_empty())
            .map(|(key, value)| RunEnvProgram {
                variable: key.clone(),
                value: value.clone(),
                resolved: resolve_program(value, path)
                    .map(|resolved| resolved.to_string_lossy().into_owned()),
            })
            .collect(),
    }
}

/// Check the programs of the `[run.env]` in the `dagq.toml` of `root` in
/// `path`, the values expanded for the run whose directory is `run_dir`.
/// Without a run (`up`, a claim, `doctor`) a value that names
/// `${DAGQ_RUN_DIR}` is not checked: nothing is in a run directory before
/// the run exists. No file is nothing to check.
pub fn check_run_env_programs(
    root: &Path,
    queue_dir: &Path,
    run_dir: Option<&Path>,
    path: Option<&OsStr>,
) -> Result<RunEnvCheck> {
    let file = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&file)? else {
        return Ok(RunEnvCheck::default());
    };
    let queue_dir = path_str(queue_dir)?;
    let run_dir = run_dir.map(path_str).transpose()?;
    let run_dir_var = format!("${{{RUN_DIR_VAR}}}");
    let env: Vec<(String, String)> = parse_run_env(&text)
        .with_context(|| format!("parse {}", file.display()))?
        .into_iter()
        .filter(|(_, value)| run_dir.is_some() || !value.contains(&run_dir_var))
        .map(|(key, value)| {
            let value = expand(&value, queue_dir, run_dir.unwrap_or_default());
            (key, value)
        })
        .collect();
    Ok(check_programs(&env, path))
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

fn is_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// The text before a `#` comment, for lines that hold no string.
pub(super) fn strip_comment(text: &str) -> &str {
    text.split_once('#')
        .map_or(text, |(before, _)| before)
        .trim()
}

/// A TOML literal (`'...'`) or basic (`"..."`) string, followed by nothing
/// but an optional comment.
pub(super) fn parse_string(text: &str) -> Result<String> {
    let mut chars = text.chars();
    let quote = chars.next().context("missing value")?;
    let mut value = String::new();
    match quote {
        '\'' => loop {
            match chars.next() {
                Some('\'') => break,
                Some(c) => value.push(c),
                None => bail!("unterminated string"),
            }
        },
        '"' => loop {
            match chars.next() {
                Some('"') => break,
                Some('\\') => value.push(match chars.next() {
                    Some('\\') => '\\',
                    Some('"') => '"',
                    Some('n') => '\n',
                    Some('t') => '\t',
                    other => bail!("unsupported escape \\{}", other.unwrap_or(' ')),
                }),
                Some(c) => value.push(c),
                None => bail!("unterminated string"),
            }
        },
        _ => bail!("expected a quoted string"),
    }
    let rest = chars.as_str().trim();
    ensure!(
        rest.is_empty() || rest.starts_with('#'),
        "unexpected text after the string: {rest}"
    );
    Ok(value)
}

/// The verification port for `integrate`: `[run.env]` from the `dagq.toml`
/// of `checkout` (the main checkout, since `integrate` may be called from
/// any worktree of the repository) with the directory of the queue `db`,
/// and each command in `/bin/sh` with its output in the log.
pub struct ShellVerifier {
    pub checkout: PathBuf,
    pub db: PathBuf,
    /// The user's `config.toml` its [`Verifier::language`] reads
    /// (ADR-t616-2); `None` reads none.
    pub user_config: Option<PathBuf>,
    /// How long one verification command may run in all before it is
    /// killed and recorded as a `timeout` failure (task 639):
    /// [`VERIFICATION_TIMEOUT`](super::adapters::VERIFICATION_TIMEOUT)
    /// but in tests.
    pub verification_timeout: std::time::Duration,
}

impl Verifier for ShellVerifier {
    fn run_env(&self, run_dir: &Path) -> Result<Vec<(String, String)>> {
        let queue_dir = self
            .db
            .parent()
            .context("queue database has no directory")?;
        load_run_env(&self.checkout, queue_dir, run_dir)
    }

    fn run_env_programs(&self, run_dir: Option<&Path>) -> Result<RunEnvCheck> {
        let queue_dir = self
            .db
            .parent()
            .context("queue database has no directory")?;
        check_run_env_programs(
            &self.checkout,
            queue_dir,
            run_dir,
            env::var_os("PATH").as_deref(),
        )
    }

    fn run_env_table(&self) -> Result<Option<Vec<(String, String)>>> {
        load_run_env_table(&self.checkout)
    }

    fn run_env_salt(&self) -> Result<String> {
        let queue_dir = self
            .db
            .parent()
            .context("queue database has no directory")?;
        run_env_salt(queue_dir)
    }

    fn recheck_command(&self) -> Result<Option<String>> {
        load_recheck_command(&self.checkout)
    }

    fn worker_trial(&self) -> Result<WorkerTrial> {
        load_worker_trial(&self.checkout)
    }
    fn e2e_paths(&self) -> Result<Vec<String>> {
        load_e2e_paths(&self.checkout)
    }
    fn review_subagents_in(&self, text: &str) -> Result<Vec<ReviewSubagent>> {
        Ok(parse_config(text)?.review_subagents)
    }
    fn headless_wrapper_setting(&self) -> Result<Option<HeadlessWrapper>> {
        load_headless_wrapper_setting(&self.checkout)
    }
    fn role_models(&self) -> Result<RoleModels> {
        load_role_models(&self.checkout)
    }

    fn language(&self) -> Option<crate::domain::language::Language> {
        super::language::language_for_prompt(Some(&self.checkout), self.user_config.as_deref())
    }

    fn run_to_log(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<Exit> {
        super::adapters::run_shell_to_log(command, cwd, env, log, self.verification_timeout)
            .map(super::process::exit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_run_env_salt_is_made_once_and_kept_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let salt = run_env_salt(dir.path()).unwrap();
        assert_eq!(salt.len(), 64);
        assert_eq!(run_env_salt(dir.path()).unwrap(), salt);
        let file = dir.path().join(RUN_ENV_SALT_FILE);
        let mode = fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::write(&file, "").unwrap();
        assert!(run_env_salt(dir.path()).is_err());
        fs::remove_file(&file).unwrap();
        assert_ne!(run_env_salt(dir.path()).unwrap(), salt);
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// `[areas]` maps each area's name, bare or quoted, to a one-line
    /// array of globs; a mistake in it is refused with its line.
    #[test]
    fn parses_the_areas_table() {
        let config = parse_config(
            "[areas] # by path\nsrc = [\"src/**\", 'migrations/**'] # runtime\n\"docs-site\" = [ \"docs/**\" ,\"*.md\", ]\n",
        )
        .unwrap();
        let map = config.areas.unwrap();
        assert_eq!(map.names().collect::<Vec<_>>(), ["src", "docs-site"]);
        let quoted = parse_config("[areas]\nq = [\"a\\\"b\", 'c\\']\n").unwrap();
        assert_eq!(quoted.areas.unwrap().areas_of(["a\"b", "c\\"]), ["q"]);
        assert_eq!(
            map.areas_of(["README.md", "build.rs"]),
            ["docs-site", "other"]
        );
        assert_eq!(
            parse_config("[areas]\n").unwrap().areas,
            Some(AreaMap::default())
        );
        assert_eq!(parse_config("").unwrap().areas, None);
        let error = |text: &str| format!("{:#}", parse_config(text).unwrap_err());
        for (text, expected) in [
            ("[areas]\nsrc = \"src/**\"\n", "expected an array"),
            ("[areas]\nsrc = [src]\n", "expected a quoted string"),
            ("[areas]\nsrc = [\"src\" \"x\"]\n", "expected , or ]"),
            ("[areas]\nsrc = [\"src\"\n", "unterminated"),
            ("[areas]\nsrc = [\"src\n", "unterminated string"),
            ("[areas]\nsrc = [\"src\"] x\n", "after the array"),
            ("[areas]\nsrc = []\n", "no glob"),
            ("[areas]\nSrc = [\"src\"]\n", "lowercase slug"),
            ("[areas]\nall = [\"src\"]\n", "runtime gives"),
            ("[areas]\na = [\"x\"]\na = [\"y\"]\n", "defined twice"),
            ("[areas]\na = [\"/x\"]\n", "area a"),
            ("[areas]\n\"a = [\"x\"]\n", "line 2"),
        ] {
            let got = error(text);
            assert!(
                got.contains(expected) || (expected == "line 2" && got.contains(":2")),
                "{text}: {got}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_area_map(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[areas]\ndocs = [\"docs/**\"]\n",
        )
        .unwrap();
        assert_eq!(
            load_area_map(dir.path())
                .unwrap()
                .unwrap()
                .areas_of(["docs/x.md"]),
            ["docs"]
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[areas]\nX = [\"a\"]\n").unwrap();
        assert!(load_area_map(dir.path()).is_err());
    }

    /// `[review.subagents.<agent>] paths` names the globs that make a
    /// review's subagent required (ADR-t1453-1 decision 1); a mistake in
    /// it is refused with its line.
    #[test]
    fn parses_the_review_subagents() {
        let config = parse_config(
            "[review.subagents.design-consistency]\npaths = [\"src/**\", 'docs/design/**', \"src/**\"] # design\n\
             [run.env]\nA = \"1\"\n\
             [review.subagents.\"migrations\"]\npaths = [\"migrations/*.sql\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.review_subagents,
            [
                ReviewSubagent {
                    name: "design-consistency".into(),
                    paths: vec!["src/**".into(), "docs/design/**".into()],
                },
                ReviewSubagent {
                    name: "migrations".into(),
                    paths: vec!["migrations/*.sql".into()],
                },
            ]
        );
        assert_eq!(config.run_env, [("A".to_owned(), "1".to_owned())]);
        assert!(parse_config("").unwrap().review_subagents.is_empty());
        for (text, expected) in [
            (
                "[review.subagents.a]\nglobs = [\"a\"]\n",
                "dagq.toml:2: unknown key globs in [review.subagents.a]; the key is paths",
            ),
            (
                "[review.subagents.a]\npaths = [\"a\"]\npaths = [\"b\"]\n",
                "dagq.toml:3: paths is defined twice",
            ),
            (
                "[review.subagents.a]\npaths = \"src/**\"\n",
                "dagq.toml:2: value of paths",
            ),
            (
                "[review.subagents.a]\npaths = [\"/src/**\"]\n",
                "dagq.toml:2: value of paths: invalid --paths glob \"/src/**\"",
            ),
            (
                "[review.subagents.a]\npaths = [\"docs//a\"]\n",
                "dagq.toml:2: value of paths",
            ),
            (
                "[review.subagents.a]\npaths = []\n",
                "dagq.toml:2: paths of [review.subagents.a] names no glob",
            ),
            (
                "[review.subagents]\n",
                "dagq.toml:1: [review.subagents] names no agent",
            ),
            (
                "[review.subagents.]\npaths = [\"a\"]\n",
                "dagq.toml:1: [review.subagents.] names no agent",
            ),
            (
                "[review.subagents.Design]\n",
                "dagq.toml:1: agent \"Design\" of [review.subagents.Design] is not kebab-case",
            ),
            (
                "[review.subagents.a]\npaths = [\"a\"]\n[review.subagents.a]\npaths = [\"b\"]\n",
                "dagq.toml:3: [review.subagents.a] is defined twice",
            ),
            (
                "[review.subagents.a]\n",
                "dagq.toml:1: [review.subagents.a] has no paths",
            ),
            (
                "[review.subagents.a]\n[review.subagents.b]\npaths = [\"a\"]\n",
                "dagq.toml:1: [review.subagents.a] has no paths",
            ),
            (
                "[review.subagents.a]\n[run.env]\n",
                "dagq.toml:1: [review.subagents.a] has no paths",
            ),
            (
                "[review.subagents.a]\n[roles.review]\nnot_a_key = \"x\"\n",
                "dagq.toml:1: [review.subagents.a] has no paths",
            ),
            (
                "[review.subagents.a]\n[kpi]\nnot_a_key = 1\n",
                "dagq.toml:1: [review.subagents.a] has no paths",
            ),
            ("[review]\n", "dagq.toml:1: unknown table [review]"),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let verifier = ShellVerifier {
            checkout: PathBuf::from("/nonexistent"),
            db: PathBuf::from("/nonexistent/queue.db"),
            user_config: None,
            verification_timeout: Duration::from_secs(1),
        };
        assert_eq!(
            verifier
                .review_subagents_in("[review.subagents.a]\npaths = [\"x\"]\n")
                .unwrap()
                .len(),
            1
        );
        assert!(verifier.review_subagents_in("[nope]\n").is_err());
    }

    /// `[e2e] paths` names the globs whose change requires e2e
    /// (ADR-t963-1 decision 2); a mistake in it is refused with its line.
    #[test]
    fn parses_the_paths_of_the_e2e_table() {
        let config = parse_config(
            "[run.env]\nA = \"1\"\n[e2e]\npaths = [\"src/infrastructure/**\", 'tests/e2e.rs', \"tests/e2e.rs\"] # narrow\n",
        )
        .unwrap();
        assert_eq!(config.e2e_paths, ["src/infrastructure/**", "tests/e2e.rs"]);
        assert!(parse_config("[e2e]\n").unwrap().e2e_paths.is_empty());
        assert!(parse_config("").unwrap().e2e_paths.is_empty());
        for (text, expected) in [
            ("[e2e]\nglobs = [\"a\"]\n", "dagq.toml:2: unknown key globs"),
            (
                "[e2e]\npaths = [\"a\"]\npaths = [\"b\"]\n",
                "dagq.toml:3: paths is defined twice",
            ),
            ("[e2e]\npaths = \"src/**\"\n", "dagq.toml:2: value of paths"),
            (
                "\n[e2e]\npaths = [\"/src/**\"]\n",
                "dagq.toml:3: value of paths",
            ),
            (
                "[e2e]\npaths = [\"src/../x\"]\n",
                "dagq.toml:2: value of paths",
            ),
            (
                "[e2e]\npaths = [\"a\"]\n[e2e]\n",
                "dagq.toml:3: [e2e] is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(load_e2e_paths(dir.path()).unwrap().is_empty());
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[e2e]\npaths = [\"tests/e2e.rs\"]\n",
        )
        .unwrap();
        assert_eq!(load_e2e_paths(dir.path()).unwrap(), ["tests/e2e.rs"]);
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[e2e]\npaths = 1\n").unwrap();
        assert!(load_e2e_paths(dir.path()).is_err());
    }

    /// `[headless] wrapper` chooses where a headless session's wrapper
    /// runs (ADR-t1404-1 decision 7); a workspace without it.
    #[test]
    fn parses_the_wrapper_of_the_headless_table() {
        let config = parse_config("[headless]\nwrapper = \"background\" # goal 89\n").unwrap();
        assert_eq!(config.headless_wrapper, Some(HeadlessWrapper::Background));
        assert_eq!(parse_config("").unwrap().headless_wrapper, None);
        for (text, expected) in [
            (
                "[headless]\nmode = \"a\"\n",
                "dagq.toml:2: unknown key mode",
            ),
            (
                "[headless]\nwrapper = \"workspace\"\nwrapper = \"background\"\n",
                "dagq.toml:3: wrapper is defined twice",
            ),
            (
                "[headless]\nwrapper = \"cmux\"\n",
                "dagq.toml:2: value of wrapper",
            ),
            ("[headless]\nwrapper = 1\n", "dagq.toml:2: value of wrapper"),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_headless_wrapper_setting(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[headless]\nwrapper = 'background'\n",
        )
        .unwrap();
        assert_eq!(
            load_headless_wrapper_setting(dir.path()).unwrap(),
            Some(HeadlessWrapper::Background)
        );
        // Written as it is, so that a worker can tell `"workspace"` it
        // ignores from no key (ADR-t1433-3 decision 2).
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[headless]\nwrapper = 'workspace'\n",
        )
        .unwrap();
        assert_eq!(
            load_headless_wrapper_setting(dir.path()).unwrap(),
            Some(HeadlessWrapper::Workspace)
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[headless]\nwrapper = 2\n",
        )
        .unwrap();
        assert!(load_headless_wrapper_setting(dir.path()).is_err());
    }

    /// `[goals] tags` names the repository's set of goal tags (ADR-t1639-1
    /// decision 6).
    #[test]
    fn parses_the_tags_of_the_goals_table() {
        let config = parse_config("[goals]\ntags = [\"codex\", 'cmux'] # set\n").unwrap();
        let tags = config.goal_tags.unwrap();
        assert_eq!(
            tags.values()
                .iter()
                .map(GoalTag::as_str)
                .collect::<Vec<_>>(),
            ["codex", "cmux"]
        );
        assert_eq!(parse_config("[goals]\n").unwrap().goal_tags, None);
        assert_eq!(parse_config("").unwrap().goal_tags, None);
        let error = |text: &str| format!("{:#}", parse_config(text).unwrap_err());
        for (text, expected) in [
            ("[goals]\nlabels = [\"a\"]\n", "unknown key labels"),
            ("[goals]\ntags = [\"a\"]\ntags = [\"b\"]\n", "defined twice"),
            ("[goals]\ntags = \"a\"\n", "expected an array"),
            ("[goals]\ntags = [\"Codex\"]\n", "goal tag"),
            ("[goals]\ntags = []\n", "names no tag"),
            ("[goals]\ntags = [\"a\", \"a\"]\n", "twice"),
            ("[goals]\ntags = [\"a\"]\n[goals]\n", "defined twice"),
        ] {
            let got = error(text);
            assert!(got.contains(expected), "{text}: {got}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_goal_tags(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[goals]\ntags = [\"codex\"]\n",
        )
        .unwrap();
        assert_eq!(
            load_goal_tags(dir.path()).unwrap().unwrap().values().len(),
            1
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[goals]\ntags = 1\n").unwrap();
        assert!(load_goal_tags(dir.path()).is_err());
    }

    /// `[tasks] changes` names the repository's set of changes (ADR-t980-1).
    #[test]
    fn parses_the_changes_of_the_tasks_table() {
        let config = parse_config("[tasks]\nchanges = [\"feature\", 'fix'] # set\n").unwrap();
        let changes = config.changes.unwrap();
        assert_eq!(
            changes
                .values()
                .iter()
                .map(TaskChange::as_str)
                .collect::<Vec<_>>(),
            ["feature", "fix"]
        );
        assert_eq!(parse_config("[tasks]\n").unwrap().changes, None);
        assert_eq!(parse_config("").unwrap().changes, None);
        let error = |text: &str| format!("{:#}", parse_config(text).unwrap_err());
        for (text, expected) in [
            ("[tasks]\nkinds = [\"a\"]\n", "unknown key kinds"),
            (
                "[tasks]\nchanges = [\"a\"]\nchanges = [\"b\"]\n",
                "defined twice",
            ),
            ("[tasks]\nchanges = \"a\"\n", "expected an array"),
            ("[tasks]\nchanges = [\"Fix\"]\n", "task change"),
            ("[tasks]\nchanges = [\"unknown\"]\n", "task change"),
            ("[tasks]\nchanges = []\n", "names no change"),
            ("[tasks]\nchanges = [\"a\", \"a\"]\n", "twice"),
            ("[tasks]\nchanges = [\"a\"]\n[tasks]\n", "defined twice"),
        ] {
            let got = error(text);
            assert!(got.contains(expected), "{text}: {got}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_change_set(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[tasks]\nchanges = [\"fix\"]\n",
        )
        .unwrap();
        assert_eq!(
            load_change_set(dir.path()).unwrap().unwrap().values().len(),
            1
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[tasks]\nchanges = 1\n").unwrap();
        assert!(load_change_set(dir.path()).is_err());
    }

    #[test]
    fn parses_the_run_env_table_in_file_order() {
        let text = r#"
# build cache shared by every run
[run.env]  # the only table
CARGO_TARGET_DIR = '${DAGQ_QUEUE_DIR}/target'
RUST_LOG="info" # trailing comment
QUOTED = "a \"b\" \\ c\td\n"
LITERAL = 'no \n escapes # here'
"#;
        assert_eq!(
            parse_run_env(text).unwrap(),
            pairs(&[
                ("CARGO_TARGET_DIR", "${DAGQ_QUEUE_DIR}/target"),
                ("RUST_LOG", "info"),
                ("QUOTED", "a \"b\" \\ c\td\n"),
                ("LITERAL", "no \\n escapes # here"),
            ])
        );
    }

    #[test]
    fn parses_the_role_tables() {
        use crate::domain::actor_model::{ModelRole, RoleModel};
        assert_eq!(parse_config("").unwrap().roles, RoleModels::default());
        let config = parse_config(
            "[roles.plan_review]\neffort = \"high\" # up\n\n[roles.observer]\nmodel = 'claude-sonnet-5'\n[roles.review]\n",
        )
        .unwrap();
        assert_eq!(
            config.roles.get(ModelRole::PlanReview),
            Some(&RoleModel {
                provider: None,
                model: None,
                effort: Some("high".into()),
            })
        );
        assert_eq!(
            config.roles.get(ModelRole::Observer),
            Some(&RoleModel {
                provider: None,
                model: Some("claude-sonnet-5".into()),
                effort: None,
            })
        );
        // A table without a key is none.
        assert_eq!(config.roles.get(ModelRole::Review), None);
        assert_eq!(config.roles.launch(ModelRole::Review).arguments(), None);
        for (text, expected) in [
            ("[roles.nobody]", "unknown role [roles.nobody]"),
            (
                "[roles.review]\nother = 'x'",
                "unknown key other in [roles.review]",
            ),
            ("[roles.review]\neffort = 'huge'", "effort"),
            ("[roles.review]\nmodel = ' '", "model is blank"),
            (
                "[roles.review]\nmodel = 'a'\nmodel = 'b'",
                "is defined twice",
            ),
            ("[roles.review]\n[roles.review]", "is defined twice"),
            ("[roles.review]\nprovider = 'gemini'", "provider"),
            // Codex runs goal, normal run, plan and throughput reviews, the
            // observer and the recovery job (ADR-t1207-1, tasks 1218, 1220,
            // 1223 and 1225), and no planner.
            (
                "[roles.runtime_planner]\nprovider = 'codex'",
                "[roles.runtime_planner]: provider codex cannot run the runtime_planner role",
            ),
            (
                "[roles.goal_review]\nmodel = 'claude-opus-5-5'\nprovider = 'codex'",
                "model claude-opus-5-5 is Claude's",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let config = parse_config(
            "[roles.goal_review]\nprovider = \"codex\"\nmodel = 'gpt-6-astra'\n[roles.observer]\nprovider = 'claude'\n",
        )
        .unwrap();
        assert_eq!(
            config.roles.get(ModelRole::GoalReview),
            Some(&RoleModel {
                provider: Some(crate::domain::Provider::Codex),
                model: Some("gpt-6-astra".into()),
                effort: None,
            })
        );
        assert_eq!(
            config.roles.provider(ModelRole::Observer).0,
            crate::domain::Provider::Claude
        );
        // The plan review runs on Codex too (task 1218).
        let config = parse_config("[roles.plan_review]\nprovider = 'codex'\n").unwrap();
        assert_eq!(
            config.roles.provider(ModelRole::PlanReview).0,
            crate::domain::Provider::Codex
        );
        // And the throughput review (task 1220).
        let config = parse_config("[roles.throughput_review]\nprovider = 'codex'\n").unwrap();
        assert_eq!(
            config.roles.provider(ModelRole::ThroughputReview).0,
            crate::domain::Provider::Codex
        );
        // And the observer (task 1223, ADR-t1222-1).
        let config = parse_config("[roles.observer]\nprovider = 'codex'\n").unwrap();
        assert_eq!(
            config.roles.provider(ModelRole::Observer).0,
            crate::domain::Provider::Codex
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_role_models(dir.path()).unwrap(), RoleModels::default());
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[roles.planner]\neffort = 'xhigh'\n",
        )
        .unwrap();
        assert_eq!(
            load_role_models(dir.path())
                .unwrap()
                .launch(ModelRole::Planner)
                .arguments(),
            Some(("claude-opus-5-5", "xhigh"))
        );
    }

    #[test]
    fn the_old_route_of_the_runtimes_planners_is_accepted_and_ignored() {
        use crate::domain::actor_model::{ModelRole, RoleModel};
        // The runtime's planners run headless only (ADR-t1433-2 decision
        // 3): `route` of [roles.runtime_planner], whatever its value, loads
        // and gives the planner's agent nothing; its value is kept only for
        // the supervisor's warning that it is ignored.
        assert_eq!(
            parse_config("").unwrap().roles.ignored_planner_route(),
            None
        );
        for (text, value) in [
            (
                "[roles.runtime_planner]\nroute = \"headless\"\n",
                "headless",
            ),
            (
                "[roles.runtime_planner]\nroute = 'interactive'\n",
                "interactive",
            ),
            ("[roles.runtime_planner]\nroute = 'screen'\n", "screen"),
            ("[roles.runtime_planner]\nroute = ''\n", ""),
            ("[roles.runtime_planner]\nroute = true\n", "true"),
        ] {
            let config = parse_config(text).unwrap();
            assert_eq!(
                config.roles.launch(ModelRole::RuntimePlanner).arguments(),
                None,
                "{text:?}"
            );
            assert_eq!(config.roles.ignored_planner_route(), Some(value));
        }
        let config =
            parse_config("[roles.runtime_planner]\nroute = 'interactive'\neffort = 'high'\n")
                .unwrap();
        assert_eq!(
            config.roles.get(ModelRole::RuntimePlanner),
            Some(&RoleModel {
                provider: None,
                model: None,
                effort: Some("high".into()),
            })
        );
        assert_eq!(
            config.roles.launch(ModelRole::RuntimePlanner).arguments(),
            Some(("claude-opus-5-5", "high"))
        );
        for (text, expected) in [
            (
                "[roles.planner]\nroute = 'headless'",
                "route is a key of [roles.runtime_planner] only",
            ),
            (
                "[roles.runtime_planner]\nroute = 'headless'\nroute = 'headless'",
                "is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
    }

    #[test]
    fn parses_the_worker_trial_table() {
        // Off unless it says so (ADR-0079 decision 4).
        assert_eq!(
            parse_config("").unwrap().worker_trial,
            WorkerTrial::default()
        );
        let config = parse_config("[worker.trial]\nenabled = true # on\nwindow = 30\n").unwrap();
        assert_eq!(
            config.worker_trial,
            WorkerTrial {
                enabled: true,
                window: 30
            }
        );
        let off = parse_config("[worker.trial]\nenabled = false\n").unwrap();
        assert_eq!(off.worker_trial, WorkerTrial::default());
        for (text, error) in [
            (
                "[worker.trial]\nother = 1",
                "unknown key other in [worker.trial]",
            ),
            ("[worker.trial]\nenabled = 1", "expected true or false"),
            ("[worker.trial]\nwindow = 0", "positive number"),
            (
                "[worker.trial]\nwindow = 1\nwindow = 2",
                "window is defined twice",
            ),
            ("[worker.trial]\n[worker.trial]", "is defined twice"),
        ] {
            let message = format!("{:#}", parse_config(text).unwrap_err());
            assert!(message.contains(error), "{text}: {message}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_worker_trial(dir.path()).unwrap(),
            WorkerTrial::default()
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[worker.trial]\nenabled = true\n",
        )
        .unwrap();
        assert!(load_worker_trial(dir.path()).unwrap().enabled);
        let verifier = ShellVerifier {
            checkout: dir.path().to_owned(),
            db: dir.path().join("q.db"),
            user_config: None,
            verification_timeout: crate::infrastructure::adapters::VERIFICATION_TIMEOUT,
        };
        assert!(verifier.worker_trial().unwrap().enabled);
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[worker.trial]\nx = 1\n").unwrap();
        assert!(load_worker_trial(dir.path()).is_err());
    }

    #[test]
    fn accepts_the_language_table_without_reading_it() {
        // Its reader checks it (ADR-t616-2); a mistake in it never stops
        // what reads the other tables.
        let config =
            parse_config("[language]\nname = 1\nnot a key\n[stall]\nsend_confirm_secs = 5\n")
                .unwrap();
        assert_eq!(config.stall.send_confirm_secs, 5);
        assert!(parse_config("[language]\n[language]").is_ok());
    }

    #[test]
    fn parses_the_broker_table() {
        // No table is the default: disabled.
        assert_eq!(parse_config("").unwrap().broker, BrokerConfig::default());
        let config = parse_config(
            "[broker]\nmode = \"preferred\" # the contract\nexec_allow = [\"sh\", \"ls\"]\nexec_env = []\nexec_timeout_secs = 30\nexec_max_timeout_secs = 120\noutput_limit_bytes = 2048\nfs_limit_bytes = 4096\n",
        )
        .unwrap()
        .broker;
        assert_eq!(
            config,
            BrokerConfig {
                mode: BrokerMode::Preferred,
                exec_allow: vec!["sh".into(), "ls".into()],
                exec_env: Vec::new(),
                exec_timeout_secs: 30,
                exec_max_timeout_secs: 120,
                output_limit_bytes: 2048,
                fs_limit_bytes: 4096,
                packages: Vec::new(),
            }
        );
        let packages = parse_config(
            "[broker]\nmode = \"preferred\"\n[broker.package]\ncargo-fetch = [\"cargo\", \"fetch\"] # deps\n\"npm-install\" = [\"npm\", \"install\"]\n",
        )
        .unwrap()
        .broker
        .packages;
        assert_eq!(
            packages,
            [
                (
                    "cargo-fetch".to_owned(),
                    vec!["cargo".to_owned(), "fetch".to_owned()]
                ),
                (
                    "npm-install".to_owned(),
                    vec!["npm".to_owned(), "install".to_owned()]
                ),
            ]
        );
        for (text, error) in [
            ("[broker]\nmode = \"on\"\n", "dagq.toml:2: value of mode"),
            ("[broker]\nport = 1\n", "unknown key port in [broker]"),
            (
                "[broker]\nmode = \"preferred\"\nmode = \"disabled\"\n",
                "mode is defined twice",
            ),
            (
                "[broker]\nexec_timeout_secs = 0\n",
                "value of exec_timeout_secs",
            ),
            (
                "[broker]\nexec_timeout_secs = 400\n",
                "above exec_max_timeout_secs",
            ),
            ("[broker]\n[broker]\n", "[broker] is defined twice"),
            (
                "[broker.package]\nfetch = [\"cargo\"]\nfetch = [\"npm\"]\n",
                "fetch is defined twice",
            ),
            (
                "[broker.package]\npull = [\"git\", \"pull\"]\n",
                "dagq.toml:2: package command pull: git runs through the git operations only",
            ),
            (
                "[broker.package]\nfetch = []\n",
                "dagq.toml:2: package command fetch: the argv is empty",
            ),
            (
                "[broker.package]\nfetch = \"cargo fetch\"\n",
                "dagq.toml:2: value of fetch",
            ),
            (
                "[broker.package]\n[broker.package]\n",
                "[broker.package] is defined twice",
            ),
        ] {
            let message = format!("{:#}", parse_config(text).unwrap_err());
            assert!(message.contains(error), "{text}: {message}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_broker_config(dir.path()).unwrap(),
            BrokerConfig::default()
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[broker]\nmode = \"required\"\n",
        )
        .unwrap();
        assert_eq!(
            load_broker_config(dir.path()).unwrap().mode,
            BrokerMode::Required
        );
    }

    #[test]
    fn parses_the_disk_table() {
        let config = parse_config(
            "[disk]\nsample_runs = 5\nclaim_factor = 2.5 # more\nintegrate_factor = 1\nmin_free_bytes = 1_000\n",
        )
        .unwrap();
        assert_eq!(
            config.disk,
            DiskConfig {
                sample_runs: 5,
                claim_factor: 2.5,
                integrate_factor: 1.0,
                min_free_bytes: Some(1000),
            }
        );
        assert_eq!(parse_config("").unwrap().disk, DiskConfig::default());
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_disk_config(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[disk]\nclaim_factor = 3\n",
        )
        .unwrap();
        assert_eq!(
            load_disk_config(dir.path()).unwrap().unwrap().claim_factor,
            3.0
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[disk]\nx = 3\n").unwrap();
        assert!(load_disk_config(dir.path()).is_err());
    }

    #[test]
    fn empty_text_and_empty_table_are_no_env() {
        assert!(parse_run_env("").unwrap().is_empty());
        assert!(parse_run_env("# nothing\n[run.env]\n").unwrap().is_empty());
        assert_eq!(
            parse_run_env("\u{feff}[run.env]\nA = 'x'").unwrap(),
            pairs(&[("A", "x")])
        );
    }

    #[test]
    fn rejects_what_the_subset_does_not_support() {
        for (text, message) in [
            ("[run.env\nA = 'x'", "unclosed table header"),
            ("[build]\nA = 'x'", "unknown table [build]"),
            (
                "[stall]\nother_secs = 1",
                "unknown key other_secs in [stall]",
            ),
            (
                "[stall]\nsend_confirm_secs = 0",
                "positive number of seconds",
            ),
            (
                "[stall]\nsend_confirm_secs = -5",
                "positive number of seconds",
            ),
            (
                "[stall]\nsend_confirm_secs = '60'",
                "whole number of seconds",
            ),
            ("[stall]\nsend_confirm_secs = ", "missing value"),
            (
                "[stall]\nsend_confirm_secs = 1\nsend_confirm_secs = 2",
                "send_confirm_secs is defined twice",
            ),
            ("[stall]\n[stall]", "[stall] is defined twice"),
            ("[conflicts]\nother = 1", "unknown key other in [conflicts]"),
            (
                "[conflicts]\nhotspot_conflicts = 0",
                "positive number, not 0",
            ),
            (
                "[conflicts]\nhotspot_conflicts = 1\nhotspot_conflicts = 2",
                "hotspot_conflicts is defined twice",
            ),
            ("A = 'x'", "a key outside [run.env]"),
            ("[disk]\nother = 1", "unknown key other in [disk]"),
            ("[disk]\nclaim_factor = 0", "positive number, not 0"),
            ("[disk]\nclaim_factor = x", "expected a number, not x"),
            ("[disk]\nclaim_factor = ", "missing value"),
            ("[disk]\nsample_runs = 1.5", "whole number"),
            (
                "[disk]\nsample_runs = 1\nsample_runs = 2",
                "sample_runs is defined twice",
            ),
            ("[resume]\nother = 1", "2: unknown key other in [resume]"),
            (
                "[resume]\nconflict_only_limit = 0",
                "2: value of conflict_only_limit",
            ),
            (
                "[resume]\nconflict_only_limit = 0",
                "positive number, not 0",
            ),
            (
                "[resume]\nconflict_only_limit = -1",
                "positive number, not -1",
            ),
            ("[resume]\nconflict_only_limit = 1.5", "whole number"),
            ("[resume]\nconflict_only_limit = ", "missing value"),
            (
                "[resume]\nconflict_only_limit = 1\nconflict_only_limit = 2",
                "3: conflict_only_limit is defined twice",
            ),
            ("[resume]\n[resume]", "2: [resume] is defined twice"),
            ("[recheck]\nargs = 'x'", "unknown key args in [recheck]"),
            (
                "[repository]\nremotes = 'x'",
                "unknown key remotes in [repository]",
            ),
            ("[repository]\nremote = ''", "remote is blank"),
            ("[repository]\nremote = x", "expected a quoted string"),
            (
                "[repository]\nremote = 'a'\nremote = 'b'",
                "remote is defined twice",
            ),
            ("[repository]\npush = 'false'", "expected true or false"),
            ("[repository]\npush = ", "missing value"),
            (
                "[repository]\npush = true\npush = false",
                "push is defined twice",
            ),
            ("[repository]\nbranch = ''", "branch is blank"),
            (
                "[repository]\nbranch = 'refs/heads/x'",
                "without refs/heads/",
            ),
            ("[repository]\nbranch = x", "expected a quoted string"),
            (
                "[repository]\nbranch = 'a'\nbranch = 'b'",
                "branch is defined twice",
            ),
            (
                "[recheck]\ncommand = 'x'\ncommand = 'y'",
                "command is defined twice",
            ),
            ("[recheck]\ncommand = ' '", "command is blank"),
            ("[recheck]\ncommand = x", "expected a quoted string"),
            ("[run.env]\nA 'x'", "expected KEY"),
            ("[run.env]\n1A = 'x'", "not an environment variable name"),
            ("[run.env]\nA-B = 'x'", "not an environment variable name"),
            ("[run.env]\nDAGQ_ROLE = 'x'", "reserved prefix"),
            ("[run.env]\nA = 'x'\nA = 'y'", "A is defined twice"),
            ("[run.env]\n[run.env]", "[run.env] is defined twice"),
            ("[run.env]\nA = x", "expected a quoted string"),
            ("[run.env]\nA = ", "missing value"),
            ("[run.env]\nA = 'x", "unterminated string"),
            ("[run.env]\nA = \"x", "unterminated string"),
            ("[run.env]\nA = \"\\q\"", "unsupported escape \\q"),
            ("[run.env]\nA = 'x' y", "unexpected text after the string"),
        ] {
            let error = format!("{:#}", parse_run_env(text).unwrap_err());
            assert!(error.contains(message), "{text:?}: {error}");
        }
    }

    #[test]
    fn parses_the_exit_table_that_is_accepted_and_ignored() {
        use crate::domain::exit::ExitConfig;
        let secs = |list: &[u64]| {
            list.iter()
                .map(|secs| Duration::from_secs(*secs))
                .collect::<Vec<_>>()
        };
        let config = parse_config(
            "[exit] # retries\nretries = 2 # fewer\nretry_intervals_secs = [5, 10] # apart\n",
        )
        .unwrap();
        assert_eq!(
            config.exit,
            ExitConfig {
                retries: 2,
                intervals: secs(&[5, 10]),
            }
        );
        assert_eq!(parse_config("").unwrap().exit, ExitConfig::default());
        let config = parse_config("[exit]\nretries = 0\n").unwrap();
        assert_eq!(config.exit.retries, 0);
        assert_eq!(config.exit.intervals, secs(&[30, 60, 120]));
        assert_eq!(
            parse_config("[exit]\nretry_intervals_secs = [1,2,3,4]\n")
                .unwrap()
                .exit,
            ExitConfig {
                retries: 3,
                intervals: secs(&[1, 2, 3, 4]),
            }
        );
        for (text, message) in [
            ("[exit]\nx = 1", "dagq.toml:2: unknown key x in [exit]"),
            ("[exit]\nretries = -1", "must be 0 or more"),
            (
                "[exit]\nretries = 1\nretries = 2",
                "retries is defined twice",
            ),
            (
                "[exit]\nretry_intervals_secs = 30",
                "expected an array of seconds",
            ),
            ("[exit]\nretry_intervals_secs = []", "the array is empty"),
            ("[exit]\nretry_intervals_secs = [0]", "must be a positive"),
            (
                "[exit]\nretry_intervals_secs = [a]",
                "expected a whole number of seconds",
            ),
            ("[exit]\nretry_intervals_secs = ", "missing value"),
            ("[exit]\nretry_intervals_secs = [30,,60]", "missing value"),
            ("[exit]\n[exit]", "[exit] is defined twice"),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(message), "{text:?}: {error}");
        }
        assert!(parse_config("[exit]\nretries = x\n").is_err());
    }

    #[test]
    fn parses_and_loads_the_resume_table() {
        let config =
            parse_config("[resume] # attempts\nconflict_only_limit = 8 # more\n[stall]\nsend_confirm_secs = 30\n")
                .unwrap();
        assert_eq!(
            config.resume,
            ResumeConfig {
                conflict_only_limit: 8
            }
        );
        assert_eq!(config.stall.send_confirm_secs, 30);
        assert_eq!(parse_config("").unwrap().resume, ResumeConfig::default());
        assert_eq!(
            parse_config("[resume]\n").unwrap().resume,
            ResumeConfig::default()
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_resume_config(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[resume]\nconflict_only_limit = 2\n",
        )
        .unwrap();
        assert_eq!(
            load_resume_config(dir.path()).unwrap(),
            Some(ResumeConfig {
                conflict_only_limit: 2
            })
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[resume]\nx = 1\n").unwrap();
        let error = format!("{:#}", load_resume_config(dir.path()).unwrap_err());
        assert!(
            error.contains("dagq.toml:2: unknown key x in [resume]"),
            "{error}"
        );
    }

    /// `[ci_watch]` (ADR-t1920-1): `workflow` is required, the rest have
    /// defaults; values out of range, unknown and repeated keys are errors
    /// with their line.
    #[test]
    fn parses_the_ci_watch_table() {
        assert_eq!(parse_config("").unwrap().ci_watch, None);
        assert_eq!(
            parse_config("[ci_watch]\nworkflow = \"ci.yml\"\n")
                .unwrap()
                .ci_watch,
            Some(CiWatchConfig {
                workflow: "ci.yml".into(),
                branch: None,
                interval_secs: 600,
                junit_artifacts: Vec::new(),
            })
        );
        assert_eq!(
            parse_config(
                "[ci_watch]\nworkflow = 'CI'\nbranch = 'trunk'\ninterval_secs = 60 # a minute\njunit_artifacts = ['junit-*', 'more'] # globs\n"
            )
            .unwrap()
            .ci_watch,
            Some(CiWatchConfig {
                workflow: "CI".into(),
                branch: Some("trunk".into()),
                interval_secs: 60,
                junit_artifacts: vec!["junit-*".into(), "more".into()],
            })
        );
        for (text, expected) in [
            ("[ci_watch]\n", "dagq.toml:1: [ci_watch] has no workflow"),
            (
                "[ci_watch]\nworkflow = 'a'\nkind = 'b'\n",
                "dagq.toml:3: unknown key kind in [ci_watch]; the keys are workflow, branch, interval_secs, junit_artifacts",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\nworkflow = 'b'\n",
                "dagq.toml:3: workflow is defined twice",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\ninterval_secs = 59\n",
                "dagq.toml:3: interval_secs must be at least 60, not 59",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\ninterval_secs = 'x'\n",
                "dagq.toml:3: value of interval_secs",
            ),
            (
                "[ci_watch]\nworkflow = ' '\n",
                "dagq.toml:2: workflow is blank",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\nbranch = 'refs/heads/main'\n",
                "branch is a branch name without refs/heads/",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\njunit_artifacts = 'x'\n",
                "dagq.toml:3: value of junit_artifacts",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\njunit_artifacts = ['']\n",
                "dagq.toml:3: junit_artifacts has an empty glob",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\njunit_artifacts = ['x', 'x']\n",
                "dagq.toml:3: junit_artifacts names \"x\" twice",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\n[ci_watch]\n",
                "dagq.toml:3: [ci_watch] is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_ci_watch(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[ci_watch]\nworkflow = 'ci.yml'\n",
        )
        .unwrap();
        assert_eq!(
            load_ci_watch(dir.path()).unwrap().unwrap().workflow,
            "ci.yml"
        );
    }

    /// `[provider_fallback] workers` and `jobs` are bools, each on without
    /// it; the table knows no other key (ADR-t1857-1).
    #[test]
    fn parses_the_workers_and_jobs_of_the_provider_fallback_table() {
        let fallback = |text: &str| parse_config(text).unwrap().provider_fallback;
        assert_eq!(fallback(""), ProviderFallback::default());
        assert!(fallback("").workers && fallback("").jobs);
        assert_eq!(
            fallback("[provider_fallback]\n"),
            ProviderFallback::default()
        );
        assert_eq!(
            fallback("[provider_fallback]\nworkers = false # wait\n"),
            ProviderFallback {
                workers: false,
                jobs: true
            }
        );
        assert_eq!(
            fallback("[provider_fallback]\njobs = false\n"),
            ProviderFallback {
                workers: true,
                jobs: false
            }
        );
        assert_eq!(
            fallback("[provider_fallback]\nworkers = true\njobs = true\n"),
            ProviderFallback::default()
        );
        for (text, expected) in [
            (
                "[provider_fallback]\nlimit = false\n",
                "dagq.toml:2: unknown key limit in [provider_fallback]; the keys are workers, jobs",
            ),
            (
                "[provider_fallback]\nworkers = \"no\"\n",
                "dagq.toml:2: value of workers: expected true or false",
            ),
            (
                "[provider_fallback]\njobs = 1\n",
                "dagq.toml:2: value of jobs: expected true or false",
            ),
            (
                "[provider_fallback]\nworkers = false\nworkers = true\n",
                "dagq.toml:3: workers is defined twice",
            ),
            (
                "[provider_fallback]\njobs = false\njobs = true\n",
                "dagq.toml:3: jobs is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_provider_fallback(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[provider_fallback]\nworkers = false\njobs = false\n",
        )
        .unwrap();
        assert_eq!(
            load_provider_fallback(dir.path()).unwrap(),
            Some(ProviderFallback {
                workers: false,
                jobs: false
            })
        );
    }

    /// `[supervisor] light_changes` names changes of `[tasks] changes`,
    /// wherever `[tasks]` is, each once (ADR-t1591-1).
    #[test]
    fn parses_the_light_changes_of_the_supervisor_table() {
        let tasks = "[tasks]\nchanges = [\"feature\", \"docs\", \"config\"]\n";
        let config = parse_config(&format!(
            "[supervisor]\nlight_changes = [\"docs\", 'config'] # light\n{tasks}"
        ))
        .unwrap();
        let light = config.supervisor.light_changes();
        assert_eq!(
            light
                .values()
                .iter()
                .map(|change| change.as_str())
                .collect::<Vec<_>>(),
            ["docs", "config"]
        );
        // Left out, nothing is light.
        assert!(
            parse_config(tasks)
                .unwrap()
                .supervisor
                .light_changes()
                .is_empty()
        );
        for (text, message) in [
            (
                format!("{tasks}[supervisor]\nlight_changes = [\"measure\"]\n"),
                "dagq.toml:4: value of light_changes: names measure, which is not one of [tasks] changes",
            ),
            (
                format!("{tasks}[supervisor]\n\nlight_changes = [\"docs\", \"docs\"]\n"),
                "dagq.toml:5: value of light_changes: names docs twice",
            ),
            (
                format!("{tasks}[supervisor]\nlight_changes = \"docs\"\n"),
                "dagq.toml:4: value of light_changes: expected an array",
            ),
            (
                format!("{tasks}[supervisor]\nlight_changes = [\"Docs\"]\n"),
                "dagq.toml:4: value of light_changes",
            ),
            (
                format!("{tasks}[supervisor]\nlight_changes = []\n"),
                "dagq.toml:4: value of light_changes: names no change",
            ),
            (
                "[supervisor]\nlight_changes = [\"docs\"]\n".to_owned(),
                "dagq.toml:2: value of light_changes: names docs, but there is no [tasks] changes",
            ),
            (
                format!(
                    "{tasks}[supervisor]\nlight_changes = [\"docs\"]\nlight_changes = [\"docs\"]\n"
                ),
                "dagq.toml:5: light_changes is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(&text).unwrap_err());
            assert!(error.contains(message), "{text:?}: {error}");
        }
    }

    #[test]
    fn parses_and_loads_the_supervisor_table() {
        let config =
            parse_config("[supervisor] # slots\nparallel = 3 # build is heavy\nmax_waiting = 0\nruntime_planners = 2\nclaim_spacing = 120 # secs\n")
                .unwrap();
        assert_eq!(
            config.supervisor,
            SupervisorConfig {
                parallel: Some(3),
                max_waiting: Some(0),
                runtime_planners: Some(2),
                claim_spacing: Some(120),
                light_changes: None,
            }
        );
        assert_eq!(
            parse_config("[supervisor]\nclaim_spacing = 0\n")
                .unwrap()
                .supervisor
                .claim_spacing,
            Some(0)
        );
        assert_eq!(
            parse_config("[supervisor]\nmax_waiting = 2\n")
                .unwrap()
                .supervisor,
            SupervisorConfig {
                parallel: None,
                max_waiting: Some(2),
                runtime_planners: None,
                claim_spacing: None,
                light_changes: None,
            }
        );
        assert_eq!(
            parse_config("").unwrap().supervisor,
            SupervisorConfig::default()
        );
        for (text, message) in [
            ("[supervisor]\nparallel = 0\n", "must be a positive number"),
            ("[supervisor]\nparallel = -1\n", "must be a positive number"),
            ("[supervisor]\nparallel = 70000\n", "value of parallel"),
            ("[supervisor]\nmax_waiting = -1\n", "must be 0 or more"),
            (
                "[supervisor]\nmax_waiting = two\n",
                "expected a whole number",
            ),
            ("[supervisor]\nmax_waiting =\n", "missing value"),
            (
                "[supervisor]\nruntime_planners = 0\n",
                "dagq.toml:2: value of runtime_planners",
            ),
            (
                "[supervisor]\nruntime_planners = 0\n",
                "must be a positive number",
            ),
            (
                "[supervisor]\nruntime_planners = two\n",
                "dagq.toml:2: value of runtime_planners",
            ),
            (
                "[supervisor]\nruntime_planners = 1.5\n",
                "value of runtime_planners",
            ),
            (
                "[supervisor]\nruntime_planners = 70000\n",
                "value of runtime_planners",
            ),
            (
                "[supervisor]\nruntime_planners = 2\nruntime_planners = 3\n",
                "dagq.toml:3: runtime_planners is defined twice",
            ),
            (
                "[supervisor]\nparallel = 2\nparallel = 3\n",
                "parallel is defined twice",
            ),
            (
                "[supervisor]\nslots = 2\n",
                "unknown key slots in [supervisor]; the keys are parallel, max_waiting, runtime_planners, claim_spacing, light_changes",
            ),
            (
                "[supervisor]\nclaim_spacing = -1\n",
                "dagq.toml:2: value of claim_spacing: must be 0 or more",
            ),
            (
                "[supervisor]\n\nclaim_spacing = 3m\n",
                "dagq.toml:3: value of claim_spacing: expected a whole number",
            ),
            (
                "[supervisor]\nclaim_spacing = 1.5\n",
                "dagq.toml:2: value of claim_spacing",
            ),
            (
                "[supervisor]\nclaim_spacing = 5000000000\n",
                "dagq.toml:2: value of claim_spacing",
            ),
            (
                "[supervisor]\nclaim_spacing =\n",
                "dagq.toml:2: value of claim_spacing: missing value",
            ),
            (
                "[supervisor]\n[supervisor]\n",
                "[supervisor] is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(message), "{text:?}: {error}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_supervisor_config(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[run.env]\nA = 'x'\n[supervisor]\nparallel = 2\n",
        )
        .unwrap();
        assert_eq!(
            load_supervisor_config(dir.path()).unwrap(),
            Some(SupervisorConfig {
                parallel: Some(2),
                max_waiting: None,
                runtime_planners: None,
                claim_spacing: None,
                light_changes: None,
            })
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[supervisors]\n").unwrap();
        let error = format!("{:#}", load_supervisor_config(dir.path()).unwrap_err());
        assert!(
            error.contains(
                "[supervisor], [areas], [tasks], [goals], [e2e], [broker], [broker.package], [headless], [provider_fallback], [ci_watch] and [kpi]"
            ),
            "{error}"
        );
    }

    #[test]
    fn reads_the_recheck_command() {
        let config =
            parse_config("[recheck]\ncommand = \"cargo check --locked\" # fast\n").unwrap();
        assert_eq!(
            config.recheck_command.as_deref(),
            Some("cargo check --locked")
        );
        assert_eq!(parse_config("").unwrap().recheck_command, None);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_recheck_command(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[run.env]\nA = 'x'\n[recheck]\ncommand = 'make check'\n",
        )
        .unwrap();
        assert_eq!(
            load_recheck_command(dir.path()).unwrap().as_deref(),
            Some("make check")
        );
        let verifier = ShellVerifier {
            checkout: dir.path().to_owned(),
            db: dir.path().join("queue.db"),
            user_config: None,
            verification_timeout: crate::infrastructure::adapters::VERIFICATION_TIMEOUT,
        };
        assert_eq!(
            verifier.recheck_command().unwrap().as_deref(),
            Some("make check")
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[recheck]\nnope = 1\n").unwrap();
        assert!(load_recheck_command(dir.path()).is_err());
    }

    #[test]
    fn reads_the_repository_table() {
        let config = parse_config("[repository]\nbranch = \"master\" # trunk\n").unwrap();
        assert_eq!(config.repository.branch.as_deref(), Some("master"));
        assert_eq!(config.repository.remote, None);
        assert_eq!(config.repository.push, None);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_repository_config(dir.path()).unwrap(),
            RepositoryConfig::default()
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[run.env]\nA = 'x'\n[repository]\nbranch = 'trunk'\nremote = 'upstream'\npush = false # local only\n",
        )
        .unwrap();
        assert_eq!(
            load_repository_config(dir.path()).unwrap(),
            RepositoryConfig {
                branch: Some("trunk".into()),
                remote: Some("upstream".into()),
                push: Some(false),
            }
        );
        let config = parse_config("[repository]\npush = true\n").unwrap();
        assert_eq!(config.repository.push, Some(true));
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[repository]\nx = 1\n").unwrap();
        assert!(load_repository_config(dir.path()).is_err());
    }

    #[test]
    fn parses_the_stall_table_next_to_the_run_env() {
        let text = "[stall] # thresholds\nidle_without_receipt_secs = 600 # ten minutes\nbackground_alert_secs = 3_600\n[run.env]\nA = 'x'\n";
        let config = parse_config(text).unwrap();
        assert_eq!(config.run_env, pairs(&[("A", "x")]));
        assert_eq!(
            config.stall,
            StallConfig {
                idle_without_receipt_secs: 600,
                send_confirm_secs: crate::domain::stall::DEFAULT_SEND_CONFIRM_SECS,
                background_alert_secs: 3600,
                idle_process_secs: crate::domain::stall::DEFAULT_IDLE_PROCESS_SECS,
                screen_idle_secs: crate::domain::stall::DEFAULT_SCREEN_IDLE_SECS,
                ..StallConfig::default()
            }
        );
        assert_eq!(parse_config("").unwrap().stall, StallConfig::default());
    }

    /// `[kpi]` and its targets sit among the other tables; a table after
    /// them is its own again.
    #[test]
    fn parses_and_loads_the_kpi_tables() {
        let text = "[run.env]\nA = 'x'\n[kpi]\nmin_samples = 4\nmax_improvement_proposals = 1\n[kpi.targets.\"phase.work\"]\nchange = \"fix\"\nmax = 3600\n[stall]\nsend_confirm_secs = 30\n";
        let config = parse_config(text).unwrap();
        assert_eq!(config.run_env, pairs(&[("A", "x")]));
        assert_eq!(config.stall.send_confirm_secs, 30);
        let kpi = config.kpi.unwrap();
        assert_eq!(kpi.min_samples, Some(4));
        assert_eq!(kpi.max_improvement_proposals, Some(1));
        assert_eq!(kpi.targets[0].stratum(), "change=fix");
        assert_eq!(parse_config("[stall]\n").unwrap().kpi, None);
        let error = format!("{:#}", parse_config("[kpi]\nx = 1\n").unwrap_err());
        assert!(error.starts_with("dagq.toml:2: unknown key x"), "{error}");
        let error = format!(
            "{:#}",
            parse_config("[kpi.targets.a]\nchange = \"docs\"\n").unwrap_err()
        );
        assert!(error.contains("neither min nor max"), "{error}");
        // The task's kind is gone (ADR-t980-1), in dagq.toml as in host.toml.
        let error = format!(
            "{:#}",
            parse_config("[kpi.targets.a]\nkind = \"docs\"\nmax = 1\n").unwrap_err()
        );
        assert!(
            error.contains("kind of a target was removed") && error.contains("change or area"),
            "{error}"
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_kpi_settings(dir.path()).unwrap(), None);
        fs::write(dir.path().join(CONFIG_FILE_NAME), text).unwrap();
        assert_eq!(
            load_kpi_settings(dir.path()).unwrap().unwrap().min_samples,
            Some(4)
        );
    }

    #[test]
    fn loads_the_stall_table_of_the_file_in_the_root() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_stall_config(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[stall]\nsend_confirm_secs = 30\n",
        )
        .unwrap();
        assert_eq!(
            load_stall_config(dir.path())
                .unwrap()
                .unwrap()
                .send_confirm_secs,
            30
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[stall]\nx = 1\n").unwrap();
        let error = format!("{:#}", load_stall_config(dir.path()).unwrap_err());
        assert!(
            error.contains("parse ") && error.contains("unknown key"),
            "{error}"
        );
    }

    #[test]
    fn loads_the_conflicts_table_of_the_file_in_the_root() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_conflict_config(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[conflicts]\nhotspot_ratio_percent = 50 # half\n[stall]\nsend_confirm_secs = 30\n",
        )
        .unwrap();
        assert_eq!(
            load_conflict_config(dir.path()).unwrap().unwrap(),
            ConflictConfig {
                hotspot_ratio_percent: 50,
                ..ConflictConfig::default()
            }
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[conflicts]\nx = 1\n").unwrap();
        assert!(load_conflict_config(dir.path()).is_err());
    }

    #[test]
    fn errors_name_the_line() {
        let error = format!("{:#}", parse_run_env("[run.env]\n\nA = 1").unwrap_err());
        assert!(error.starts_with("dagq.toml:3: value of A"), "{error}");
    }

    #[test]
    fn expands_only_the_queue_and_run_directories() {
        assert_eq!(
            expand(
                "${DAGQ_QUEUE_DIR}/target:${DAGQ_RUN_DIR}/tmp:${HOME}:$DAGQ_RUN_DIR:${DAGQ_QUEUE_DIR}",
                "/q",
                "/q/runs/r"
            ),
            "/q/target:/q/runs/r/tmp:${HOME}:$DAGQ_RUN_DIR:/q"
        );
    }

    #[test]
    fn loads_and_expands_the_file_in_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Path::new("/data/dagq/abc");
        let run = Path::new("/data/dagq/abc/runs/r1");
        assert!(load_run_env(dir.path(), queue, run).unwrap().is_empty());
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[run.env]\nCARGO_TARGET_DIR = '${DAGQ_QUEUE_DIR}/target'\nTMPDIR = \"${DAGQ_RUN_DIR}/tmp\"\n",
        )
        .unwrap();
        assert_eq!(
            load_run_env(dir.path(), queue, run).unwrap(),
            pairs(&[
                ("CARGO_TARGET_DIR", "/data/dagq/abc/target"),
                ("TMPDIR", "/data/dagq/abc/runs/r1/tmp"),
            ])
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[other]\n").unwrap();
        let error = format!("{:#}", load_run_env(dir.path(), queue, run).unwrap_err());
        assert!(
            error.contains("parse ") && error.contains("unknown table"),
            "{error}"
        );
    }

    fn executable(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn resolves_a_program_by_path_or_in_the_path_list() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let tool = executable(&bin, "tool");
        fs::write(bin.join("plain"), "not executable").unwrap();
        fs::create_dir(bin.join("folder")).unwrap();
        let path = env::join_paths(["", "/nonexistent", bin.to_str().unwrap()]).unwrap();
        assert_eq!(resolve_program("tool", Some(&path)), Some(tool.clone()));
        assert_eq!(resolve_program(tool.to_str().unwrap(), None), Some(tool));
        for missing in ["plain", "folder", "absent"] {
            assert_eq!(resolve_program(missing, Some(&path)), None, "{missing}");
        }
        assert_eq!(resolve_program("tool", None), None);
        assert_eq!(
            resolve_program(bin.join("plain").to_str().unwrap(), Some(&path)),
            None
        );
    }

    #[test]
    fn checks_only_the_variables_cargo_executes_with_a_value() {
        let dir = tempfile::tempdir().unwrap();
        executable(dir.path(), "sccache");
        let path = dir.path().as_os_str();
        let check = check_programs(
            &pairs(&[
                ("RUSTC_WRAPPER", "sccache"),
                ("SCCACHE_IGNORE_SERVER_IO_ERROR", "1"),
                ("RUSTC_WORKSPACE_WRAPPER", ""),
                ("CARGO_BUILD_RUSTDOC", "missing-rustdoc"),
            ]),
            Some(path),
        );
        assert!(check.config);
        assert_eq!(check.path, dir.path().to_str().unwrap());
        assert_eq!(
            check.programs,
            vec![
                RunEnvProgram {
                    variable: "RUSTC_WRAPPER".into(),
                    value: "sccache".into(),
                    resolved: Some(dir.path().join("sccache").to_str().unwrap().into()),
                },
                RunEnvProgram {
                    variable: "CARGO_BUILD_RUSTDOC".into(),
                    value: "missing-rustdoc".into(),
                    resolved: None,
                },
            ]
        );
        assert_eq!(check.missing().len(), 1);
    }

    #[test]
    fn checks_the_run_env_of_the_file_in_the_root() {
        let root = tempfile::tempdir().unwrap();
        let queue = tempfile::tempdir().unwrap();
        let bin = queue.path().join("bin");
        fs::create_dir(&bin).unwrap();
        executable(&bin, "sccache");
        let path = bin.as_os_str();
        // No dagq.toml: nothing to check, whatever the PATH holds.
        let none = check_run_env_programs(root.path(), queue.path(), None, None).unwrap();
        assert_eq!(none, RunEnvCheck::default());
        assert!(none.missing_message().is_none());
        // A [run.env] without a program: nothing missing.
        fs::write(root.path().join(CONFIG_FILE_NAME), "[run.env]\nA = 'x'\n").unwrap();
        let plain = check_run_env_programs(root.path(), queue.path(), None, Some(path)).unwrap();
        assert!(plain.config && plain.programs.is_empty());
        // Found on the PATH, by name and by an expanded path.
        fs::write(
            root.path().join(CONFIG_FILE_NAME),
            "[run.env]\nRUSTC_WRAPPER = 'sccache'\nRUSTC_WORKSPACE_WRAPPER = '${DAGQ_QUEUE_DIR}/bin/sccache'\nRUSTDOC = '${DAGQ_RUN_DIR}/rustdoc'\n",
        )
        .unwrap();
        let found = check_run_env_programs(root.path(), queue.path(), None, Some(path)).unwrap();
        assert_eq!(found.programs.len(), 2, "{found:?}");
        assert!(found.missing().is_empty(), "{found:?}");
        // With a run, the run directory is expanded and checked too.
        let run = queue.path().join("runs/r1");
        fs::create_dir_all(&run).unwrap();
        let in_run =
            check_run_env_programs(root.path(), queue.path(), Some(&run), Some(path)).unwrap();
        assert_eq!(in_run.missing()[0].variable, "RUSTDOC");
        assert_eq!(
            in_run.missing()[0].value,
            run.join("rustdoc").to_str().unwrap()
        );
        // Missing from the PATH.
        let missing = check_run_env_programs(
            root.path(),
            queue.path(),
            None,
            Some(OsStr::new("/nonexistent")),
        )
        .unwrap();
        assert_eq!(missing.missing()[0].variable, "RUSTC_WRAPPER");
        assert!(
            missing
                .missing_message()
                .unwrap()
                .contains("RUSTC_WRAPPER = \"sccache\"")
        );
        fs::write(root.path().join(CONFIG_FILE_NAME), "[other]\n").unwrap();
        assert!(check_run_env_programs(root.path(), queue.path(), None, Some(path)).is_err());
    }

    #[test]
    fn the_shell_verifier_checks_on_the_process_path() {
        let root = tempfile::tempdir().unwrap();
        let queue = tempfile::tempdir().unwrap();
        let verifier = ShellVerifier {
            checkout: root.path().to_path_buf(),
            db: queue.path().join("queue.sqlite3"),
            user_config: None,
            verification_timeout: crate::infrastructure::adapters::VERIFICATION_TIMEOUT,
        };
        assert!(!verifier.run_env_programs(None).unwrap().config);
        fs::write(
            root.path().join(CONFIG_FILE_NAME),
            "[run.env]\nRUSTC_WRAPPER = '/nonexistent/sccache'\nRUSTC = 'sh'\n",
        )
        .unwrap();
        let check = verifier.run_env_programs(Some(queue.path())).unwrap();
        assert_eq!(check.programs.len(), 2);
        assert_eq!(check.missing().len(), 1, "{check:?}");
        assert_eq!(check.missing()[0].variable, "RUSTC_WRAPPER");
    }

    #[test]
    fn unreadable_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(CONFIG_FILE_NAME)).unwrap();
        let error = format!(
            "{:#}",
            load_run_env(dir.path(), Path::new("/q"), Path::new("/r")).unwrap_err()
        );
        assert!(error.contains("read "), "{error}");
    }
}
