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
//! decision 24). `[exit]` holds the retries of a `/exit` the session held
//! back and the wait after each (ADR-0047 decision 25). `[worker.trial]` turns on the limited trial of the worker's model
//! (ADR-0079 decision 4). `[roles.<role>]` holds the model and effort of a
//! session other than the worker's (ADR-0079 decision 7). `[supervisor]`
//! holds `parallel` and `max_waiting` of a supervisor started without the
//! flags (task 698). The file is parsed by
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
        actor_model::{ModelRole, RoleModel, RoleModels, check_effort},
        disk::DiskConfig,
        exit::ExitConfig,
        kpi::KpiSettings,
        landing_branch::RepositoryConfig,
        resume::ResumeConfig,
        run_env::{RunEnvCheck, RunEnvProgram},
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
/// `[roles.<role>]`: the model and effort of a role other than the
/// worker (ADR-0079 decision 7), one table per role.
const ROLES_PREFIX: &str = "roles.";
/// What [`parse_config`] calls the current table while in a `[roles.*]`.
const ROLES_TABLE: &str = "roles";
/// `[language]` (ADR-t616-2): accepted here without looking into it;
/// [`super::language`] reads and checks it, so a mistake in it never stops
/// a claim or a landing.
const LANGUAGE_TABLE: &str = "language";
/// `[supervisor]`: `parallel` and `max_waiting` (task 698).
const SUPERVISOR_TABLE: &str = "supervisor";
const TABLES: [&str; 11] = [
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
/// decision 25) and `[kpi]` (ADR-0051
/// decisions 17 and 19).
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
    /// `[exit]`, the defaults for the keys it does not set.
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
    let mut role: Option<ModelRole> = None;
    let mut roles_seen: Vec<ModelRole> = Vec::new();
    let mut role_keys: Vec<String> = Vec::new();
    let mut kpi = KpiTables::default();
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
            let known = TABLES.iter().find(|table| **table == name).with_context(|| {
                format!(
                    "{CONFIG_FILE_NAME}:{number}: unknown table [{name}]; only [{RUN_ENV_TABLE}], [{STALL_TABLE}], [{CONFLICTS_TABLE}], [{RECHECK_TABLE}], [{DISK_TABLE}], [{RESUME_TABLE}], [{EXIT_TABLE}], [{REPOSITORY_TABLE}], [{WORKER_TRIAL_TABLE}], [{ROLES_PREFIX}<role>], [{LANGUAGE_TABLE}], [{SUPERVISOR_TABLE}] and [{KPI_TABLE}] are supported"
                )
            })?;
            ensure!(
                *known == LANGUAGE_TABLE || !seen.contains(known),
                "{CONFIG_FILE_NAME}:{number}: [{name}] is defined twice"
            );
            seen.push(known);
            table = Some(known);
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
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let value = parse_string(rest.trim()).with_context(with)?;
                ensure!(
                    !value.trim().is_empty(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is blank"
                );
                let table = config.roles.entry(role);
                if key == "model" {
                    table.model = Some(value);
                } else {
                    check_effort(&value)
                        .map_err(|error| anyhow::anyhow!("{error}"))
                        .with_context(with)?;
                    table.effort = Some(value);
                }
                role_keys.push(key.to_owned());
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
                "{CONFIG_FILE_NAME}:{number}: a key outside [{RUN_ENV_TABLE}], [{STALL_TABLE}], [{CONFLICTS_TABLE}], [{RECHECK_TABLE}], [{DISK_TABLE}], [{RESUME_TABLE}], [{EXIT_TABLE}], [{REPOSITORY_TABLE}], [{WORKER_TRIAL_TABLE}], [{ROLES_PREFIX}<role>], [{SUPERVISOR_TABLE}] or [{KPI_TABLE}]"
            ),
        }
    }
    config.kpi = kpi.finish().with_context(|| CONFIG_FILE_NAME.to_owned())?;
    Ok(config)
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

/// `[exit]` of the `dagq.toml` in `root` (ADR-0047 decision 25), `None`
/// when there is no file; no table or no key is the default.
pub fn load_exit_config(root: &Path) -> Result<Option<ExitConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .exit,
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
                model: None,
                effort: Some("high".into())
            })
        );
        assert_eq!(
            config.roles.get(ModelRole::Observer),
            Some(&RoleModel {
                model: Some("claude-sonnet-5".into()),
                effort: None
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
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
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
    fn parses_and_loads_the_exit_table() {
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
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_exit_config(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[exit]\nretry_intervals_secs = [1,2,3,4]\n",
        )
        .unwrap();
        assert_eq!(
            load_exit_config(dir.path()).unwrap(),
            Some(ExitConfig {
                retries: 3,
                intervals: secs(&[1, 2, 3, 4]),
            })
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
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[exit]\nretries = x\n").unwrap();
        assert!(load_exit_config(dir.path()).is_err());
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

    #[test]
    fn parses_and_loads_the_supervisor_table() {
        let config =
            parse_config("[supervisor] # slots\nparallel = 3 # build is heavy\nmax_waiting = 0\n")
                .unwrap();
        assert_eq!(
            config.supervisor,
            SupervisorConfig {
                parallel: Some(3),
                max_waiting: Some(0),
            }
        );
        assert_eq!(
            parse_config("[supervisor]\nmax_waiting = 2\n")
                .unwrap()
                .supervisor,
            SupervisorConfig {
                parallel: None,
                max_waiting: Some(2),
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
                "[supervisor]\nparallel = 2\nparallel = 3\n",
                "parallel is defined twice",
            ),
            (
                "[supervisor]\nslots = 2\n",
                "unknown key slots in [supervisor]; the keys are parallel, max_waiting",
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
            })
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[supervisors]\n").unwrap();
        let error = format!("{:#}", load_supervisor_config(dir.path()).unwrap_err());
        assert!(error.contains("[supervisor] and [kpi]"), "{error}");
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
        let text = "[run.env]\nA = 'x'\n[kpi]\nmin_samples = 4\nmax_improvement_proposals = 1\n[kpi.targets.\"phase.work\"]\nkind = \"runtime\"\nmax = 3600\n[stall]\nsend_confirm_secs = 30\n";
        let config = parse_config(text).unwrap();
        assert_eq!(config.run_env, pairs(&[("A", "x")]));
        assert_eq!(config.stall.send_confirm_secs, 30);
        let kpi = config.kpi.unwrap();
        assert_eq!(kpi.min_samples, Some(4));
        assert_eq!(kpi.max_improvement_proposals, Some(1));
        assert_eq!(kpi.targets[0].stratum(), "kind=runtime");
        assert_eq!(parse_config("[stall]\n").unwrap().kpi, None);
        let error = format!("{:#}", parse_config("[kpi]\nx = 1\n").unwrap_err());
        assert!(error.starts_with("dagq.toml:2: unknown key x"), "{error}");
        let error = format!(
            "{:#}",
            parse_config("[kpi.targets.a]\nkind = \"docs\"\n").unwrap_err()
        );
        assert!(error.contains("neither min nor max"), "{error}");
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
